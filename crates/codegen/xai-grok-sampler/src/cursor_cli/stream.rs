//! Layer-2 style stream: ConversationRequest → SamplingEvents via Cursor CLI.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::Stream;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use xai_grok_sampling_types::{ConversationItem, ConversationRequest, TokenUsage, ToolCall};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingErrorKind, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

use super::process::{
    CursorCliError, CursorNdjsonEvent, parse_ndjson_line, read_stdout_lines, spawn_ask_stream,
};
use super::prompt::{
    build_compact_prompt, build_prompt, build_response, looks_like_tool_intent,
    parse_assistant_output, should_suppress_assistant_stream, tool_call_from_cursor_ndjson,
    tool_calls_required,
};

/// Options for one Cursor CLI sampling turn.
#[derive(Debug, Clone, Default)]
pub struct CursorCliStreamOpts {
    /// Resume a prior Cursor CLI session (`--resume`).
    pub resume: Option<String>,
    /// When set, the stream writes the latest Cursor `session_id` here on success
    /// so the shell can resume on the next tool-result turn.
    pub session_slot: Option<Arc<Mutex<Option<String>>>>,
    /// Real project cwd for `--workspace` (ask-mode is read-only).
    pub workspace: Option<std::path::PathBuf>,
}

/// Drive one Cursor CLI turn into [`SamplingEvent`]s.
///
/// Always uses `--mode ask`. Grok owns tools via the `grok-tool-calls` fence.
pub fn stream_cursor_cli(
    request: ConversationRequest,
    model: String,
    request_id: RequestId,
    idle_timeout: Duration,
    cancel_token: CancellationToken,
    opts: CursorCliStreamOpts,
) -> Pin<Box<dyn Stream<Item = SamplingEvent> + Send>> {
    Box::pin(async_stream::stream! {
        let stream_start = Instant::now();
        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        // Prefer the live session slot over a frozen `opts.resume` snapshot so
        // in-turn updates to the slot are visible on the next spawn.
        let resume = opts
            .session_slot
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|g| g.clone()))
            .or_else(|| opts.resume.clone())
            .filter(|s| !s.is_empty());
        let is_resume = resume.is_some();
        let workspace = opts.workspace.clone();
        let prompt = build_prompt(&request, is_resume, workspace.as_deref());
        let tools_required = tool_calls_required(&request);
        // Agent turns always offer tools. Live-streaming assistant text is unsafe
        // here: Cursor often emits a `grok-tool-calls` fence (or raw JSON) in
        // small deltas, and early tokens reach the UI before suppress can fire.
        // Buffer text until final parse; shell `fallback_text` delivers clean
        // prose when `message_chunks_emitted == 0`.
        let tools_offered = !request.tools.is_empty();

        let spawned = match spawn_ask_stream(
            &model,
            &prompt,
            resume.as_deref(),
            workspace.as_deref(),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                yield failed_event(&request_id, e);
                return;
            }
        };
        let mut child = spawned.child;
        let stderr_task = spawned.stderr_task;
        let _temp_workspace = spawned.temp_workspace;

        let mut lines = std::pin::pin!(read_stdout_lines(spawned.stdout));
        let mut assistant_text = String::new();
        // Terminal `result.result`, kept separate so it can replace the
        // streamed buffer instead of being concatenated onto it.
        let mut canonical_result: Option<String> = None;
        let mut first_token = false;
        let mut chunk_index = 0u64;
        let mut message_chunks = 0u64;
        let mut chunk_timestamps: Vec<Instant> = Vec::new();
        let mut usage: Option<TokenUsage> = None;
        let mut fatal: Option<CursorCliError> = None;
        let mut cursor_session_id: Option<String> = None;
        let mut native_tool_warned = false;
        // Tools recovered from NDJSON `tool_call` events (keyed by call_id).
        let mut native_tools: std::collections::HashMap<String, ToolCall> =
            std::collections::HashMap::new();
        let mut native_tools_anon: Vec<ToolCall> = Vec::new();

        loop {
            let next = tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    let _ = child.kill().await;
                    // Drop without a terminal event; drive_l2 treats cancel
                    // via its own select and returns AttemptOutcome::Cancelled.
                    return;
                }
                item = tokio::time::timeout(idle_timeout, lines.next()) => item,
            };

            match next {
                Err(_) => {
                    let _ = child.kill().await;
                    fatal = Some(CursorCliError::Timeout);
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    let _ = child.kill().await;
                    fatal = Some(CursorCliError::Io(e));
                    break;
                }
                Ok(Some(Ok(line))) => {
                    let Some(event) = parse_ndjson_line(&line) else {
                        continue;
                    };
                    match event {
                        CursorNdjsonEvent::ThinkingDelta(text) => {
                            if !first_token {
                                first_token = true;
                                yield SamplingEvent::FirstToken {
                                    request_id: request_id.clone(),
                                };
                            }
                            chunk_timestamps.push(Instant::now());
                            let idx = chunk_index;
                            chunk_index += 1;
                            yield SamplingEvent::ChannelToken {
                                request_id: request_id.clone(),
                                channel: SamplingChannel::Reasoning,
                                text,
                                chunk_index: idx,
                            };
                        }
                        CursorNdjsonEvent::AssistantText(text) => {
                            // Real deltas, including a short token that repeats
                            // what was already said. Concatenate; do not treat
                            // the chunk as a cumulative snapshot.
                            let Some(delta) = append_assistant_delta(&mut assistant_text, &text)
                            else {
                                continue;
                            };
                            if !first_token {
                                first_token = true;
                                yield SamplingEvent::FirstToken {
                                    request_id: request_id.clone(),
                                };
                            }
                            chunk_timestamps.push(Instant::now());
                            // With tools offered: never stream text live (see
                            // `tools_offered` above). Reasoning still streams.
                            if tools_offered {
                                continue;
                            }
                            // No-tools turns: still hold back obvious tool JSON
                            // so a stray fence cannot flash into the UI.
                            if should_suppress_assistant_stream(&assistant_text) {
                                continue;
                            }
                            message_chunks += 1;
                            let idx = chunk_index;
                            chunk_index += 1;
                            yield SamplingEvent::ChannelToken {
                                request_id: request_id.clone(),
                                channel: SamplingChannel::Text,
                                text: delta,
                                chunk_index: idx,
                            };
                        }
                        CursorNdjsonEvent::NativeToolCall {
                            subtype,
                            call_id,
                            raw,
                        } => {
                            if !native_tool_warned {
                                native_tool_warned = true;
                                warn!(
                                    subtype = %subtype,
                                    call_id = ?call_id,
                                    "Cursor CLI emitted a native tool_call in ask mode; \
                                     translating when possible (Grok owns tools)"
                                );
                            }
                            if let Some(tc) = tool_call_from_cursor_ndjson(&raw) {
                                if let Some(id) = call_id.filter(|s| !s.is_empty()) {
                                    native_tools.insert(id, tc);
                                } else if !tc.id.is_empty() {
                                    let id = tc.id.to_string();
                                    native_tools.insert(id, tc);
                                } else {
                                    native_tools_anon.push(tc);
                                }
                            }
                        }
                        CursorNdjsonEvent::Result {
                            text,
                            usage: u,
                            is_error,
                            error_message,
                            session_id,
                        } => {
                            if let Some(sid) = session_id {
                                cursor_session_id = Some(sid);
                            }
                            if is_error {
                                fatal = Some(CursorCliError::Exit {
                                    code: Some(1),
                                    stderr: error_message.unwrap_or_else(|| {
                                        text.unwrap_or_else(|| "Cursor Agent CLI error".into())
                                    }),
                                });
                                break;
                            }
                            if let Some(t) = text.filter(|s| !s.is_empty()) {
                                // Deduplicated full answer. Replacing the
                                // streamed buffer with this (below) is what
                                // `fallback_text` prints when no text chunks
                                // were streamed. Merging it on top repeats
                                // every segment that a tool-call flush already
                                // copied into the buffer.
                                canonical_result = Some(t);
                            }
                            if let Some(u) = u {
                                usage = Some(TokenUsage {
                                    prompt_tokens: u.input_tokens,
                                    completion_tokens: u.output_tokens,
                                    total_tokens: u.input_tokens + u.output_tokens,
                                    reasoning_tokens: 0,
                                    cached_prompt_tokens: u.cache_read_tokens,
                                    cache_creation_prompt_tokens: u.cache_write_tokens,
                                });
                            }
                        }
                        CursorNdjsonEvent::SessionId(session_id) => {
                            debug!(%session_id, "Cursor Agent CLI session started");
                            cursor_session_id = Some(session_id);
                        }
                        CursorNdjsonEvent::Other => {}
                    }
                }
            }
        }

        // stderr was drained concurrently from spawn; join after stdout ends.
        let stderr = match stderr_task.await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Cursor Agent CLI stderr drain task failed");
                String::new()
            }
        };
        let wait_status = child.wait().await;

        match wait_status {
            Ok(status) if status.success() => {
                // Auth failures often exit 0 with an empty stream and a stderr
                // message — treat those as fatal instead of empty-response retries.
                if assistant_text.is_empty()
                    && fatal.is_none()
                    && let Some(err) = classify_cli_stderr(&stderr, status.code())
                {
                    fatal = Some(err);
                }
            }
            Ok(status) if !status.success() && fatal.is_none() => {
                // Do not soft-accept non-zero exits just because some assistant
                // tokens arrived — that commits truncated/partial turns.
                fatal = Some(classify_cli_stderr(&stderr, status.code()).unwrap_or_else(|| {
                    CursorCliError::Exit {
                        code: status.code(),
                        stderr: stderr.clone(),
                    }
                }));
            }
            Ok(_) => {}
            Err(e) if fatal.is_none() => fatal = Some(CursorCliError::Io(e)),
            Err(_) => {}
        }

        if let Some(err) = fatal {
            yield failed_event(&request_id, err);
            return;
        }

        assistant_text = finish_assistant_text(assistant_text, canonical_result);

        if assistant_text.is_empty() {
            let message = if stderr.trim().is_empty() {
                "Cursor Agent CLI returned no assistant text".into()
            } else {
                format!("Cursor Agent CLI returned no assistant text: {}", stderr.trim())
            };
            let auth = is_auth_stderr(&stderr);
            yield SamplingEvent::Failed {
                request_id: request_id.clone(),
                error: SamplingErrorInfo {
                    kind: if auth {
                        SamplingErrorKind::Auth
                    } else {
                        SamplingErrorKind::EmptyResponse
                    },
                    status_code: None,
                    message,
                    // Auth / misconfig must not spin the retry loop forever.
                    is_retryable: !auth,
                    retry_after_secs: None,
                    error_code: None,
                    // No HTTP `x-should-retry` header on the CLI bridge.
                    should_retry: None,
                    model_metadata: None,
                    empty_response_context: None,
                    doom_loop_triggers: None,
                    doom_loop_aborted_at_chunk: None,
                    credential: xai_grok_sampling_types::SentCredential::Unknown,
                },
            };
            return;
        }

        let parsed = parse_assistant_output(&assistant_text);
        let mut extra_tool_calls: Vec<ToolCall> = native_tools.into_values().collect();
        extra_tool_calls.append(&mut native_tools_anon);
        // Prefer text-parsed tools; NDJSON extras fill in when the model only
        // emitted native tool_call events (or unparseable text UI).
        let has_tools = !parsed.tool_calls.is_empty() || !extra_tool_calls.is_empty();
        if !has_tools
            && tools_offered
            && (tools_required
                || looks_like_tool_intent(&assistant_text)
                || native_tool_warned)
        {
            // Soft-complete: protocol mismatch must not become a retry storm.
            warn!(
                tools_required,
                native_tool_warned,
                intent = looks_like_tool_intent(&assistant_text),
                "Cursor CLI tool-shaped output could not be translated; \
                 soft-completing without tools"
            );
        }

        // When tools were offered we never streamed text chunks (buffered for
        // final parse). Force 0 so shell `fallback_text` can emit clean prose
        // without replaying any tool JSON that never reached the UI.
        // Tool-only turns keep message_chunks at 0 as well.
        let message_chunks = if tools_offered || has_tools {
            0
        } else {
            message_chunks
        };

        if let Some(slot) = &opts.session_slot
            && let Some(sid) = cursor_session_id.clone()
            && let Ok(mut guard) = slot.lock()
        {
            *guard = Some(sid);
        }

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);
        let response = build_response(
            &assistant_text,
            &model,
            usage,
            message_chunks,
            cursor_session_id,
            extra_tool_calls,
        );
        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(response),
            metrics,
        };
    })
}

/// Append one streaming assistant delta. Returns the chunk to emit.
///
/// Deltas are new text, including a short token equal to the buffer so far.
fn append_assistant_delta(acc: &mut String, text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    acc.push_str(text);
    Some(text.to_owned())
}

/// Prefer the terminal `result` text over the streamed buffer.
///
/// An empty result keeps the deltas, so a missing field does not drop the turn.
fn finish_assistant_text(streamed: String, canonical: Option<String>) -> String {
    match canonical {
        Some(text) if !text.is_empty() => text,
        _ => streamed,
    }
}

fn is_auth_stderr(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("authentication required")
        || lower.contains("agent login")
        || lower.contains("cursor_api_key")
        || lower.contains("not authenticated")
        || lower.contains("unauthorized")
}

fn classify_cli_stderr(stderr: &str, code: Option<i32>) -> Option<CursorCliError> {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(CursorCliError::Exit {
        code: code.or(Some(1)),
        stderr: trimmed.to_owned(),
    })
}

fn failed_event(request_id: &RequestId, err: CursorCliError) -> SamplingEvent {
    warn!(error = %err, "Cursor CLI sampling failed");
    let (kind, status_code, is_retryable) = match &err {
        CursorCliError::NotFound => (SamplingErrorKind::Api, None, false),
        CursorCliError::Timeout => (SamplingErrorKind::IdleTimeout, None, true),
        CursorCliError::Exit { stderr, .. } if is_auth_stderr(stderr) => {
            (SamplingErrorKind::Auth, Some(401), false)
        }
        CursorCliError::Exit { .. } => (SamplingErrorKind::Api, Some(500), true),
        CursorCliError::Spawn(_) | CursorCliError::Io(_) => (SamplingErrorKind::Http, None, true),
    };
    SamplingEvent::Failed {
        request_id: request_id.clone(),
        error: SamplingErrorInfo {
            kind,
            status_code,
            message: err.to_string(),
            is_retryable,
            retry_after_secs: None,
            error_code: None,
            // No HTTP `x-should-retry` header on the CLI bridge.
            should_retry: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
        },
    }
}

/// Text collected from one ask-mode Cursor CLI spawn.
#[derive(Debug)]
pub struct CursorCliCollectedText {
    pub content: String,
    pub ttft_ms: Option<u64>,
    pub stream_ms: Option<u64>,
    pub delta_count: u64,
    pub itl_max_ms: Option<u64>,
}

/// Failure of [`collect_cursor_cli_summary`] before compaction maps it.
#[derive(Debug)]
pub enum CursorCliCollectError {
    Cancelled,
    Timeout(String),
    Spawn(String),
    Auth(String),
    Failed(String),
    Empty(String),
}

/// Stand-in `agent` transcript for compaction tests. Production leaves it unset.
#[derive(Debug, Clone)]
pub struct CursorCliCompactFixture {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

struct CompactClock {
    start: Instant,
    first_delta: Option<Instant>,
    last_delta: Option<Instant>,
    itl_max: Option<Duration>,
    delta_count: u64,
}

impl CompactClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            first_delta: None,
            last_delta: None,
            itl_max: None,
            delta_count: 0,
        }
    }

    fn note_delta(&mut self) {
        let now = Instant::now();
        if let Some(previous) = self.last_delta {
            let gap = now.saturating_duration_since(previous);
            self.itl_max = Some(match self.itl_max {
                Some(max) => max.max(gap),
                None => gap,
            });
        }
        if self.first_delta.is_none() {
            self.first_delta = Some(now);
        }
        self.last_delta = Some(now);
        self.delta_count += 1;
    }

    fn fields(&self) -> (Option<u64>, Option<u64>, u64, Option<u64>) {
        let ttft_ms = self
            .first_delta
            .map(|first| duration_ms(first.saturating_duration_since(self.start)));
        let stream_ms = self.first_delta.map(|first| duration_ms(first.elapsed()));
        (
            ttft_ms,
            stream_ms,
            self.delta_count,
            self.itl_max.map(duration_ms),
        )
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn wall_clock_exceeded(start: Instant, budget_secs: u64) -> bool {
    budget_secs > 0 && start.elapsed().as_secs() >= budget_secs
}

fn fatal_message(message: String) -> CursorCliCollectError {
    if is_auth_stderr(&message) {
        CursorCliCollectError::Auth(message)
    } else {
        CursorCliCollectError::Failed(message)
    }
}

struct CompactFold {
    streamed: String,
    canonical: Option<String>,
}

/// Fold one Cursor CLI NDJSON line.
///
/// `Ok(true)` when assistant text grew. The terminal `result` replaces the
/// streamed buffer later, and session ids are ignored.
fn apply_compact_line(fold: &mut CompactFold, line: &str) -> Result<bool, String> {
    let Some(event) = parse_ndjson_line(line) else {
        return Ok(false);
    };
    match event {
        CursorNdjsonEvent::AssistantText(text) => {
            Ok(append_assistant_delta(&mut fold.streamed, &text).is_some())
        }
        CursorNdjsonEvent::Result {
            text,
            is_error,
            error_message,
            ..
        } => {
            if is_error {
                return Err(error_message.unwrap_or_else(|| {
                    text.unwrap_or_else(|| "Cursor Agent CLI error".to_owned())
                }));
            }
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                fold.canonical = Some(text);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn finalize_compact_text(
    fold: CompactFold,
    stderr: &str,
    success: bool,
    delta_count: u64,
    ttft_ms: Option<u64>,
    stream_ms: Option<u64>,
    itl_max_ms: Option<u64>,
) -> Result<CursorCliCollectedText, CursorCliCollectError> {
    if !success {
        let message = stderr.trim();
        if is_auth_stderr(message) {
            return Err(CursorCliCollectError::Auth(message.to_owned()));
        }
        let message = if message.is_empty() {
            "Cursor Agent CLI exited non-zero".to_owned()
        } else {
            message.to_owned()
        };
        return Err(CursorCliCollectError::Failed(message));
    }
    let content = finish_assistant_text(fold.streamed, fold.canonical);
    if content.is_empty() {
        if is_auth_stderr(stderr) {
            return Err(CursorCliCollectError::Auth(stderr.trim().to_owned()));
        }
        let message = if stderr.trim().is_empty() {
            "Cursor Agent CLI returned no assistant text".to_owned()
        } else {
            format!(
                "Cursor Agent CLI returned no assistant text: {}",
                stderr.trim()
            )
        };
        return Err(CursorCliCollectError::Empty(message));
    }
    Ok(CursorCliCollectedText {
        content,
        ttft_ms,
        stream_ms,
        delta_count,
        itl_max_ms,
    })
}

fn interpret_compact_fixture(
    fixture: &CursorCliCompactFixture,
) -> Result<CursorCliCollectedText, CursorCliCollectError> {
    let mut fold = CompactFold {
        streamed: String::new(),
        canonical: None,
    };
    let mut delta_count = 0u64;
    for line in fixture.stdout.lines() {
        match apply_compact_line(&mut fold, line) {
            Ok(true) => delta_count += 1,
            Ok(false) => {}
            Err(message) => return Err(fatal_message(message)),
        }
    }
    finalize_compact_text(
        fold,
        &fixture.stderr,
        fixture.success,
        delta_count,
        None,
        None,
        None,
    )
}

async fn stop_child(
    child: &mut tokio::process::Child,
    stderr_task: tokio::task::JoinHandle<String>,
) {
    let _ = child.kill().await;
    let _ = stderr_task.await;
    let _ = child.wait().await;
}

/// Ask-mode summary for compaction.
///
/// Spawns `agent` with no `--resume` and does not record the Cursor session id.
/// `fixture`, when set, supplies stdout instead of spawning.
pub async fn collect_cursor_cli_summary(
    items: &[ConversationItem],
    model: &str,
    workspace: Option<&std::path::Path>,
    idle_timeout: Duration,
    wall_clock_budget_secs: u64,
    cancel: &CancellationToken,
    fixture: Option<Arc<CursorCliCompactFixture>>,
) -> Result<CursorCliCollectedText, CursorCliCollectError> {
    if cancel.is_cancelled() {
        return Err(CursorCliCollectError::Cancelled);
    }
    let prompt = build_compact_prompt(items);
    debug!(prompt_bytes = prompt.len(), "cursor cli compact prompt");
    if let Some(fixture) = fixture {
        return interpret_compact_fixture(&fixture);
    }

    let spawned = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CursorCliCollectError::Cancelled),
        spawned = spawn_ask_stream(model, &prompt, None, workspace) => spawned,
    };
    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(err) => return Err(CursorCliCollectError::Spawn(err.to_string())),
    };

    let mut child = spawned.child;
    let stderr_task = spawned.stderr_task;
    let _temp_workspace = spawned.temp_workspace;
    let mut lines = std::pin::pin!(read_stdout_lines(spawned.stdout));
    let mut fold = CompactFold {
        streamed: String::new(),
        canonical: None,
    };
    let mut clock = CompactClock::new();
    let mut fatal: Option<CursorCliCollectError> = None;

    loop {
        if wall_clock_exceeded(clock.start, wall_clock_budget_secs) {
            stop_child(&mut child, stderr_task).await;
            return Err(CursorCliCollectError::Timeout(format!(
                "exceeded wall-clock budget {wall_clock_budget_secs}s (runaway generation)"
            )));
        }
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                stop_child(&mut child, stderr_task).await;
                return Err(CursorCliCollectError::Cancelled);
            }
            item = tokio::time::timeout(idle_timeout, lines.next()) => item,
        };
        match next {
            Err(_) => {
                let chars = fold.streamed.chars().count();
                stop_child(&mut child, stderr_task).await;
                return Err(CursorCliCollectError::Timeout(format!(
                    "stream idle timeout after {idle_timeout:?} ({chars} chars received)"
                )));
            }
            Ok(None) => break,
            Ok(Some(Err(err))) => {
                stop_child(&mut child, stderr_task).await;
                return Err(CursorCliCollectError::Failed(err.to_string()));
            }
            Ok(Some(Ok(line))) => match apply_compact_line(&mut fold, &line) {
                Ok(true) => clock.note_delta(),
                Ok(false) => {}
                Err(message) => {
                    fatal = Some(fatal_message(message));
                    break;
                }
            },
        }
    }

    let stderr = match stderr_task.await {
        Ok(stderr) => stderr,
        Err(_) => String::new(),
    };
    let wait_status = child.wait().await;
    if let Some(err) = fatal {
        return Err(err);
    }
    let success = match wait_status {
        Ok(status) => status.success(),
        Err(err) => return Err(CursorCliCollectError::Failed(err.to_string())),
    };
    let (ttft_ms, stream_ms, delta_count, itl_max_ms) = clock.fields();
    finalize_compact_text(
        fold,
        &stderr,
        success,
        delta_count,
        ttft_ms,
        stream_ms,
        itl_max_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_assistant_delta_keeps_repeated_tokens() {
        let mut acc = String::new();
        assert_eq!(
            append_assistant_delta(&mut acc, "the ").as_deref(),
            Some("the ")
        );
        assert_eq!(
            append_assistant_delta(&mut acc, "the ").as_deref(),
            Some("the ")
        );
        assert_eq!(append_assistant_delta(&mut acc, "").as_deref(), None);
        assert_eq!(acc, "the the ");
    }

    #[test]
    fn finish_assistant_text_prefers_canonical_result() {
        let streamed = "I'll update the clean description.I'll update the clean description.";
        let finished = finish_assistant_text(
            streamed.to_owned(),
            Some("I'll update the clean description.".to_owned()),
        );
        assert_eq!(finished, "I'll update the clean description.");
        assert_eq!(finish_assistant_text("kept".to_owned(), None), "kept");
        assert_eq!(
            finish_assistant_text("kept".to_owned(), Some(String::new())),
            "kept"
        );
    }

    /// Tool-call and end-of-turn flushes repeat the current segment. Folding
    /// a partial-output stream must keep each segment once, then take
    /// `result` as the finished answer.
    #[test]
    fn partial_output_stream_does_not_repeat_segments() {
        let lines = [
            r#"{"type":"assistant","timestamp_ms":1,"message":{"role":"assistant","content":[{"type":"text","text":"I'll update the clean description. "}]}}"#,
            r#"{"type":"assistant","timestamp_ms":2,"message":{"role":"assistant","content":[{"type":"text","text":"Badges sit under each heading."}]}}"#,
            r#"{"type":"assistant","timestamp_ms":3,"model_call_id":"mc_1","message":{"role":"assistant","content":[{"type":"text","text":"I'll update the clean description. Badges sit under each heading."}]}}"#,
            r#"{"type":"tool_call","subtype":"started","call_id":"c1","name":"Shell","arguments":{"command":"git status"}}"#,
            r#"{"type":"assistant","timestamp_ms":4,"message":{"role":"assistant","content":[{"type":"text","text":" The four package files are clean.cs."}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":" The four package files are clean.cs."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"I'll update the clean description. Badges sit under each heading. The four package files are clean.cs."}"#,
        ];
        let mut streamed = String::new();
        let mut canonical = None;
        for line in lines {
            match parse_ndjson_line(line).expect("ndjson") {
                CursorNdjsonEvent::AssistantText(text) => {
                    append_assistant_delta(&mut streamed, &text);
                }
                CursorNdjsonEvent::Result { text, .. } => {
                    if let Some(t) = text.filter(|s| !s.is_empty()) {
                        canonical = Some(t);
                    }
                }
                CursorNdjsonEvent::NativeToolCall { .. } | CursorNdjsonEvent::Other => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        let finished = finish_assistant_text(streamed, canonical);
        assert_eq!(
            finished,
            "I'll update the clean description. Badges sit under each heading. The four package files are clean.cs."
        );
        assert_eq!(
            finished
                .matches("I'll update the clean description.")
                .count(),
            1
        );
        assert_eq!(
            finished
                .matches("The four package files are clean.cs.")
                .count(),
            1
        );
    }

    fn summary_fixture(summary: &str) -> CursorCliCompactFixture {
        // Only `timestamp_ms` without `model_call_id` is a new delta. The
        // end-of-turn flush repeats the segment and must not be counted.
        let stdout = [
            serde_json::json!({
                "type": "assistant",
                "timestamp_ms": 1,
                "message": {"content": [{"type": "text", "text": "<sum"}]}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp_ms": 2,
                "message": {"content": [{"type": "text", "text": summary.trim_start_matches("<sum")}]}
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": summary}]}
            }),
            serde_json::json!({
                "type": "result",
                "result": summary,
                "session_id": "sess_do_not_resume"
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        CursorCliCompactFixture {
            stdout,
            stderr: String::new(),
            success: true,
        }
    }

    #[test]
    fn compact_fixture_joins_assistant_deltas() {
        let summary = "<summary>ok</summary>";
        let collected = interpret_compact_fixture(&summary_fixture(summary)).expect("summary");
        assert_eq!(collected.content, summary);
        assert!(!collected.content.contains("sess_do_not_resume"));
        assert!(collected.delta_count >= 1);
    }

    #[test]
    fn compact_fixture_auth_stderr_is_auth() {
        let err = interpret_compact_fixture(&CursorCliCompactFixture {
            stdout: String::new(),
            stderr: "Authentication required. Run agent login".into(),
            success: true,
        })
        .unwrap_err();
        assert!(matches!(err, CursorCliCollectError::Auth(_)), "{err:?}");
    }

    #[test]
    fn compact_fixture_context_length_stderr_is_failed() {
        let err = interpret_compact_fixture(&CursorCliCompactFixture {
            stdout: String::new(),
            stderr: "The prompt is too long for this model".into(),
            success: false,
        })
        .unwrap_err();
        match err {
            CursorCliCollectError::Failed(message) => {
                assert!(message.contains("prompt is too long"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}
