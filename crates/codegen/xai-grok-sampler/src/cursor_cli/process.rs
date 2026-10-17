//! Resolve and spawn the Cursor Agent CLI.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tracing::debug;

/// Sentinel base URL stored on Cursor CLI catalog entries (not used for HTTP).
pub const CURSOR_CLI_BASE_URL: &str = "cursor-cli://local";

/// Sentinel API key so BYOK-style credential checks treat Cursor models as
/// authenticated via the Cursor CLI login / `CURSOR_API_KEY`.
pub const CURSOR_CLI_API_KEY: &str = "cursor-cli";

#[derive(Debug, thiserror::Error)]
pub enum CursorCliError {
    #[error("Cursor Agent CLI not found (set CURSOR_AGENT_PATH or install `agent`)")]
    NotFound,
    #[error("failed to spawn Cursor Agent CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("Cursor Agent CLI exited with status {code:?}: {stderr}")]
    Exit { code: Option<i32>, stderr: String },
    #[error("Cursor Agent CLI I/O error: {0}")]
    Io(#[source] std::io::Error),
    #[error("timed out talking to Cursor Agent CLI")]
    Timeout,
}

/// Resolve the Cursor Agent binary path.
///
/// Order: `CURSOR_AGENT_PATH` / `AGENT_PATH` → `cursor-agent` on `PATH` →
/// `agent` on `PATH` when it looks like Cursor (not a different CLI that also
/// happens to be named `agent`, e.g. an installed `grok` binary).
///
/// On Windows, `CreateProcess` only appends `.exe` during a `PATH` search.
/// Cursor's installer ships `cursor-agent.cmd` / `agent.cmd` (the `.ps1` beside
/// them cannot be spawned: Win32 error 193). A Grok `agent.exe` earlier on
/// `PATH` therefore hides the Cursor shim entirely. Walk `PATH` and probe
/// `.exe`, `.bat`, and `.cmd` ourselves, and keep going when a hit is some
/// other CLI.
pub fn agent_bin() -> PathBuf {
    static RESOLVED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    RESOLVED.get_or_init(resolve_agent_bin).clone()
}

fn resolve_agent_bin() -> PathBuf {
    resolve_agent_bin_from(
        std::env::var_os("CURSOR_AGENT_PATH").or_else(|| std::env::var_os("AGENT_PATH")),
        std::env::var_os("PATH"),
        looks_like_cursor_cli,
    )
}

fn resolve_agent_bin_from(
    explicit: Option<std::ffi::OsString>,
    path_env: Option<std::ffi::OsString>,
    probe: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(p) = explicit {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        if let Some(path_env) = path_env {
            for candidate in windows_path_candidates(&path_env) {
                if probe(&candidate) {
                    return candidate;
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        // `Command` searches the process PATH, including shebang scripts.
        let _ = path_env;
        for name in ["cursor-agent", "agent"] {
            let path = PathBuf::from(name);
            if probe(&path) {
                return path;
            }
        }
    }
    PathBuf::from("agent")
}

/// Absolute `cursor-agent` / `agent` hits on Windows `PATH`.
///
/// Extensions follow `cmd.exe` order for files `CreateProcess` can run.
/// The bare name is last, for a no-extension binary dropped on `PATH`.
/// `.ps1` is omitted on purpose.
#[cfg(windows)]
fn windows_path_candidates(path_env: &std::ffi::OsStr) -> Vec<PathBuf> {
    const NAMES: &[&str] = &["cursor-agent", "agent"];
    const EXTS: &[&str] = &[".exe", ".bat", ".cmd", ""];
    let mut out = Vec::new();
    for name in NAMES {
        for dir in std::env::split_paths(path_env) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for ext in EXTS {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

/// True when the resolved binary looks like the Cursor Agent CLI.
pub fn agent_available() -> bool {
    looks_like_cursor_cli(&agent_bin())
}

/// Cursor's help advertises `--list-models`; other CLIs named `agent` typically do not.
fn looks_like_cursor_cli(bin: &Path) -> bool {
    let output = std::process::Command::new(bin)
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let Ok(output) = output else {
        return false;
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.contains("--list-models") || text.contains("list-models")
}

/// A model discovered from `agent --list-models`.
#[derive(Debug, Clone)]
pub struct DiscoveredCursorModel {
    pub id: String,
    pub name: String,
}

/// List models available to the authenticated Cursor account.
pub fn list_models() -> Result<Vec<DiscoveredCursorModel>, CursorCliError> {
    let bin = agent_bin();
    let output = std::process::Command::new(&bin)
        .arg("--list-models")
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CursorCliError::NotFound
            } else {
                CursorCliError::Spawn(e)
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CursorCliError::Exit {
            code: output.status.code(),
            stderr,
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_list_models(&stdout))
}

/// Account / subscription snapshot from `agent about --format json`.
///
/// Used by `/usage` to show Cursor billing alongside SuperGrok credits.
/// Field names match Cursor CLI JSON (`subscriptionTier`, `userEmail`, …).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CursorAccountInfo {
    pub subscription_tier: Option<String>,
    pub user_email: Option<String>,
    pub cli_version: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Run `agent about --format json` and parse the account/subscription fields.
pub fn fetch_about() -> Result<CursorAccountInfo, CursorCliError> {
    let bin = agent_bin();
    let output = std::process::Command::new(&bin)
        .arg("about")
        .arg("--format")
        .arg("json")
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CursorCliError::NotFound
            } else {
                CursorCliError::Spawn(e)
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CursorCliError::Exit {
            code: output.status.code(),
            stderr,
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_about_json(&stdout).ok_or_else(|| CursorCliError::Exit {
        code: Some(1),
        stderr: format!("failed to parse agent about JSON: {}", stdout.trim()),
    })
}

fn parse_about_json(stdout: &str) -> Option<CursorAccountInfo> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    // CLI sometimes prints banners before JSON — take the outermost object.
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

/// Run interactive `agent login` (opens Cursor's browser / device flow).
///
/// Stdio is inherited so the user can complete the flow in the same terminal
/// when Grok briefly yields, or in an alternate terminal if the TUI holds the
/// tty — Cursor's login primarily drives a browser URL.
pub fn run_agent_login() -> Result<(), CursorCliError> {
    let bin = agent_bin();
    let status = std::process::Command::new(&bin)
        .arg("login")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CursorCliError::NotFound
            } else {
                CursorCliError::Spawn(e)
            }
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(CursorCliError::Exit {
            code: status.code(),
            stderr: "agent login failed".to_owned(),
        })
    }
}

fn parse_list_models(stdout: &str) -> Vec<DiscoveredCursorModel> {
    let mut models = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("Available models") {
            continue;
        }
        // `id - Display Name` or `id - Display Name (current, default)`
        let Some((id, rest)) = line.split_once(" - ") else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || id.contains(' ') {
            continue;
        }
        let name = rest.split(" (").next().unwrap_or(rest).trim().to_owned();
        models.push(DiscoveredCursorModel {
            id: id.to_owned(),
            name,
        });
    }
    models
}

pub struct SpawnedCursorAgent {
    pub child: Child,
    pub stdout: ChildStdout,
    /// Concurrent stderr drain started at spawn time. Avoids the classic
    /// deadlock where a chatty CLI fills the OS stderr pipe while the parent
    /// is still blocked reading stdout.
    pub stderr_task: tokio::task::JoinHandle<String>,
    /// Keeps a fallback temp workspace alive for the child lifetime when no
    /// real project cwd was supplied.
    pub(crate) temp_workspace: Option<tempfile::TempDir>,
}

/// Legacy config field for Cursor CLI mode. Sampling always uses `--mode ask`
/// so Grok owns tools; this enum is retained only for `SamplerConfig` compat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorCliMode {
    /// Read-only Q&A — the only mode used for Grok model-backend sampling.
    #[default]
    Ask,
    /// Unused for sampling (would enable Cursor-native tools).
    Agent,
    /// Unused for sampling.
    Plan,
}

/// Spawn `agent --print --mode ask --output-format stream-json ...`.
///
/// Always ask-mode: Cursor is a model provider only. Grok executes tools.
/// Optional `--resume` continues a prior Cursor CLI session for tool-result
/// follow-ups without re-sending the full transcript.
///
/// Prefer passing the real project `workspace` so the model sees the correct
/// tree in ask mode (read-only). An empty tempdir is only used as a fallback
/// when no cwd is available.
///
/// The prompt is written on stdin (not argv). Linux caps a single argument at
/// ~128 KiB (`MAX_ARG_STRLEN`); full Grok transcripts + tool schemas routinely
/// exceed that and used to fail with `Argument list too long (os error 7)`.
pub async fn spawn_ask_stream(
    model: &str,
    prompt: &str,
    resume: Option<&str>,
    workspace: Option<&Path>,
) -> Result<SpawnedCursorAgent, CursorCliError> {
    let bin = agent_bin();
    let (workspace_path, temp_hold) = match workspace {
        Some(path) if !path.as_os_str().is_empty() => (path.to_path_buf(), None),
        _ => {
            let td = tempfile::tempdir().map_err(CursorCliError::Io)?;
            let path = td.path().to_path_buf();
            (path, Some(td))
        }
    };

    let mut cmd = Command::new(&bin);
    cmd.arg("--print")
        .arg("--mode")
        .arg("ask")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--stream-partial-output")
        .arg("--trust")
        .arg("--workspace")
        .arg(&workspace_path);
    if let Some(sid) = resume.filter(|s| !s.is_empty()) {
        cmd.arg("--resume").arg(sid);
    }
    // No prompt on argv — pipe it on stdin below (avoids E2BIG / os error 7).
    cmd.arg("--model")
        .arg(model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Keep the real HOME / Cursor config so `agent login` credentials under
    // ~/.config/cursor/ remain visible.

    debug!(
        agent = %bin.display(),
        model = %model,
        workspace = %workspace_path.display(),
        resume = resume.unwrap_or(""),
        prompt_bytes = prompt.len(),
        "spawning Cursor Agent CLI (ask-mode model backend)"
    );

    #[allow(clippy::disallowed_methods)] // waited on by the stream; killed on drop (cancellation)
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CursorCliError::NotFound
        } else {
            CursorCliError::Spawn(e)
        }
    })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        CursorCliError::Io(std::io::Error::other("missing Cursor Agent CLI stdin"))
    })?;
    let prompt_owned = prompt.to_owned();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(prompt_owned.as_bytes()).await {
            debug!(error = %e, "failed writing prompt to Cursor Agent CLI stdin");
        }
        // Closing stdin (drop) signals EOF so `agent` starts the turn.
        let _ = stdin.shutdown().await;
    });

    let stdout = child.stdout.take().ok_or_else(|| {
        CursorCliError::Io(std::io::Error::other("missing Cursor Agent CLI stdout"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        CursorCliError::Io(std::io::Error::other("missing Cursor Agent CLI stderr"))
    })?;
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        let mut err = stderr;
        let _ = err.read_to_string(&mut buf).await;
        buf
    });

    Ok(SpawnedCursorAgent {
        child,
        stdout,
        stderr_task,
        temp_workspace: temp_hold,
    })
}

/// NDJSON events we care about from Cursor CLI stream-json.
#[derive(Debug, Clone)]
pub enum CursorNdjsonEvent {
    ThinkingDelta(String),
    /// Full or partial assistant message text from `type: assistant` events.
    AssistantText(String),
    Result {
        text: Option<String>,
        usage: Option<CursorUsage>,
        is_error: bool,
        error_message: Option<String>,
        session_id: Option<String>,
    },
    SessionId(String),
    /// Cursor-native tool call (should not appear in ask mode).
    ///
    /// Ask mode still *emits* these; Grok translates recoverable payloads.
    /// `subtype: started` often has only `call_id` — later subtypes may carry
    /// name/args. Keep the full JSON so the stream can merge by `call_id`.
    NativeToolCall {
        subtype: String,
        call_id: Option<String>,
        raw: serde_json::Value,
    },
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct CursorUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

pub fn parse_ndjson_line(line: &str) -> Option<CursorNdjsonEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let ty = value.get("type")?.as_str()?;
    match ty {
        "thinking" => {
            let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            if subtype == "delta" {
                let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    Some(CursorNdjsonEvent::Other)
                } else {
                    Some(CursorNdjsonEvent::ThinkingDelta(text.to_owned()))
                }
            } else {
                Some(CursorNdjsonEvent::Other)
            }
        }
        "assistant" => {
            let message = value.get("message")?;
            let content = message.get("content")?.as_array()?;
            let mut text = String::new();
            for block in content {
                if block.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(t) = block.get("text").and_then(|v| v.as_str())
                {
                    text.push_str(t);
                }
            }
            if text.is_empty() {
                Some(CursorNdjsonEvent::Other)
            } else {
                Some(CursorNdjsonEvent::AssistantText(text))
            }
        }
        "tool_call" => {
            let subtype = value
                .get("subtype")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let call_id = value
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| value.get("id").and_then(|v| v.as_str()).map(str::to_owned));
            Some(CursorNdjsonEvent::NativeToolCall {
                subtype,
                call_id,
                raw: value,
            })
        }
        "result" => {
            let is_error = value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let text = value
                .get("result")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let error_message = value
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| {
                    value
                        .pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                });
            let session_id = value
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let usage = value.get("usage").map(|u| CursorUsage {
                input_tokens: u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                output_tokens: u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                cache_read_tokens: u
                    .get("cacheReadTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                cache_write_tokens: u
                    .get("cacheWriteTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            });
            Some(CursorNdjsonEvent::Result {
                text,
                usage,
                is_error,
                error_message,
                session_id,
            })
        }
        "system" => {
            if let Some(sid) = value.get("session_id").and_then(|v| v.as_str()) {
                Some(CursorNdjsonEvent::SessionId(sid.to_owned()))
            } else {
                Some(CursorNdjsonEvent::Other)
            }
        }
        _ => Some(CursorNdjsonEvent::Other),
    }
}

/// Read stdout line-by-line until the child exits or cancel fires.
pub fn read_stdout_lines(
    stdout: ChildStdout,
) -> impl futures_util::Stream<Item = Result<String, std::io::Error>> {
    let reader = BufReader::new(stdout);
    let lines = reader.lines();
    futures_util::stream::unfold(lines, |mut lines| async move {
        match lines.next_line().await {
            Ok(Some(line)) => Some((Ok(line), lines)),
            Ok(None) => None,
            Err(e) => Some((Err(e), lines)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_cli_mode_defaults_to_ask() {
        assert_eq!(CursorCliMode::default(), CursorCliMode::Ask);
    }

    #[test]
    fn parses_about_json_camel_case() {
        let sample = r#"{
  "subscriptionTier": "pro",
  "userEmail": "user@example.com",
  "cliVersion": "2026.01.01",
  "model": "auto"
}"#;
        let info = parse_about_json(sample).expect("parse");
        assert_eq!(info.subscription_tier.as_deref(), Some("pro"));
        assert_eq!(info.user_email.as_deref(), Some("user@example.com"));
        assert_eq!(info.cli_version.as_deref(), Some("2026.01.01"));
        assert_eq!(info.model.as_deref(), Some("auto"));
    }

    #[test]
    fn parses_about_json_with_leading_banner() {
        let sample = "Some banner\n{\"subscriptionTier\":\"free\",\"userEmail\":\"a@b.c\"}\n";
        let info = parse_about_json(sample).expect("parse");
        assert_eq!(info.subscription_tier.as_deref(), Some("free"));
        assert_eq!(info.user_email.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn parses_list_models_output() {
        let sample = "\
Available models

auto - Auto (current, default)
composer-2.5 - Composer 2.5
claude-opus-5-high - Opus 5 1M
";
        let models = parse_list_models(sample);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "auto");
        assert_eq!(models[0].name, "Auto");
        assert_eq!(models[1].id, "composer-2.5");
        assert_eq!(models[2].name, "Opus 5 1M");
    }

    #[test]
    fn looks_like_cursor_cli_requires_list_models_help() {
        // The resolved binary must advertise Cursor's model list flag. A
        // different CLI also named `agent` (for example an installed grok
        // symlink) must not count as available.
        assert!(
            looks_like_cursor_cli(&agent_bin()) || !agent_available(),
            "agent_available and looks_like_cursor_cli must agree for the resolved bin"
        );
    }

    #[test]
    fn explicit_agent_path_skips_search() {
        let resolved =
            resolve_agent_bin_from(Some(std::ffi::OsString::from("custom-agent")), None, |_| {
                panic!("probe should not run when CURSOR_AGENT_PATH is set")
            });
        assert_eq!(resolved, PathBuf::from("custom-agent"));
    }

    #[test]
    fn falls_back_to_agent_when_nothing_matches() {
        let resolved = resolve_agent_bin_from(None, Some(std::ffi::OsString::from("")), |_| false);
        assert_eq!(resolved, PathBuf::from("agent"));
    }

    /// Grok's Windows install is `agent.exe`. Cursor's installer only publishes
    /// `cursor-agent.cmd` / `agent.cmd`. `CreateProcess` PATH search finds the
    /// `.exe` and never the `.cmd`, so model discovery must walk `PATH` itself.
    #[cfg(windows)]
    #[test]
    fn windows_prefers_cursor_cmd_over_shadowing_grok_agent_exe() {
        let root = tempfile::tempdir().expect("tempdir");
        let grok_bin = root.path().join("grok-bin");
        let cursor_bin = root.path().join("cursor-agent");
        std::fs::create_dir(&grok_bin).expect("grok bin");
        std::fs::create_dir(&cursor_bin).expect("cursor bin");
        std::fs::write(grok_bin.join("agent.exe"), b"MZ").expect("agent.exe");
        let cursor_cmd = cursor_bin.join("cursor-agent.cmd");
        std::fs::write(&cursor_cmd, b"@echo off\r\necho --list-models\r\n").expect("cursor cmd");
        std::fs::write(
            cursor_bin.join("agent.cmd"),
            b"@echo off\r\necho --list-models\r\n",
        )
        .expect("agent cmd");
        let path_env = std::env::join_paths([&grok_bin, &cursor_bin]).expect("PATH");
        let resolved = resolve_agent_bin_from(None, Some(path_env), looks_like_cursor_cli);
        assert_eq!(resolved, cursor_cmd);
    }

    #[test]
    fn parses_thinking_and_assistant_ndjson() {
        let thinking = r#"{"type":"thinking","subtype":"delta","text":"hmm"}"#;
        match parse_ndjson_line(thinking) {
            Some(CursorNdjsonEvent::ThinkingDelta(t)) => assert_eq!(t, "hmm"),
            other => panic!("unexpected {other:?}"),
        }
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"PONG"}]}}"#;
        match parse_ndjson_line(assistant) {
            Some(CursorNdjsonEvent::AssistantText(t)) => assert_eq!(t, "PONG"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_native_tool_call_preserves_payload() {
        let line = r#"{"type":"tool_call","subtype":"started","call_id":"x"}"#;
        match parse_ndjson_line(line) {
            Some(CursorNdjsonEvent::NativeToolCall {
                subtype,
                call_id,
                raw,
            }) => {
                assert_eq!(subtype, "started");
                assert_eq!(call_id.as_deref(), Some("x"));
                assert_eq!(raw.get("type").and_then(|v| v.as_str()), Some("tool_call"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_native_tool_call_with_tool_fields() {
        let line = r#"{"type":"tool_call","subtype":"completed","call_id":"c1","name":"Shell","arguments":{"command":"git status"}}"#;
        match parse_ndjson_line(line) {
            Some(CursorNdjsonEvent::NativeToolCall {
                subtype,
                call_id,
                raw,
            }) => {
                assert_eq!(subtype, "completed");
                assert_eq!(call_id.as_deref(), Some("c1"));
                assert_eq!(raw.get("name").and_then(|v| v.as_str()), Some("Shell"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
