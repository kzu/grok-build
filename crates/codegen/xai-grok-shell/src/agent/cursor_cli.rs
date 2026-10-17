//! Cursor Agent CLI: model discovery + paywall policy.
//!
//! Two separate concerns:
//!
//! 1. **Enablement** ([`cursor_models_enabled`]): whether to inject `cursor/<id>`
//!    catalog entries. Auto-detects when `agent` is on `PATH`.
//! 2. **Paywall policy** ([`suppress_supergrok_paywalls`]): whether SuperGrok
//!    access gates / upgrade upsells should be suppressed. True only when the
//!    active (or selected) route is Cursor-billed, or the user set an explicit
//!    `cursor_cli = true` / `GROK_CURSOR_CLI=1` override — **not** merely
//!    because the CLI is installed.
//!
//! Inference always uses ask-mode; Grok still runs client-side tools.

use std::num::NonZeroU64;
use std::sync::OnceLock;

use indexmap::IndexMap;
use xai_grok_sampler::cursor_cli::{
    CURSOR_CLI_API_KEY, CURSOR_CLI_BASE_URL, DiscoveredCursorModel, agent_available, fetch_about,
    list_models,
};

pub use xai_grok_sampler::cursor_cli::CursorAccountInfo;
use xai_grok_sampling_types::{ApiBackend, ReasoningEffort, ReasoningEffortOption, effort_label};

use super::config::{Config, ModelEntry, ModelInfo, ModelVariant};

const DEFAULT_CURSOR_CONTEXT_WINDOW: u64 = 200_000;

/// Fallback model injected when the CLI is present but not yet authenticated,
/// so new users can pick a Cursor route and run `/login cursor`.
const BOOTSTRAP_CURSOR_MODEL_ID: &str = "auto";

/// Env override for Cursor CLI (`GROK_CURSOR_CLI`). `None` = fall through.
fn cursor_cli_env_override() -> Option<bool> {
    let Ok(raw) = std::env::var("GROK_CURSOR_CLI") else {
        return None;
    };
    let v = raw.trim();
    if matches!(v, "1" | "true" | "TRUE" | "yes" | "on") {
        return Some(true);
    }
    if matches!(v, "0" | "false" | "FALSE" | "no" | "off") {
        return Some(false);
    }
    None
}

/// Cached `agent --version` probe. Must not run on every TUI frame — spawning
/// the Node CLI takes hundreds of ms and freezes the pager.
fn agent_available_cached() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(agent_available)
}

/// Resolve whether Cursor CLI models should be injected into the catalog.
///
/// Order: `GROK_CURSOR_CLI` env → `[features].cursor_cli` → auto (`agent` on PATH).
///
/// This is **enablement only**. It does not grant SuperGrok-equivalent access;
/// see [`suppress_supergrok_paywalls`].
pub fn cursor_models_enabled(cfg: &Config) -> bool {
    if let Some(v) = cursor_cli_env_override() {
        return v;
    }
    match cfg.features.cursor_cli {
        Some(v) => v,
        None => agent_available_cached(),
    }
}

/// Alias kept for call sites that mean enablement (catalog injection).
#[inline]
pub fn cursor_cli_enabled(cfg: &Config) -> bool {
    cursor_models_enabled(cfg)
}

/// Explicit power-user opt-in for paywall suppression.
///
/// True only when the user set `GROK_CURSOR_CLI=1`/`true`/… or
/// `[features].cursor_cli = true`. Auto PATH detection does **not** count —
/// installing the CLI must not hide SuperGrok upsells for free-tier xAI users.
pub fn cursor_paywall_override(cfg: &Config) -> bool {
    if let Some(v) = cursor_cli_env_override() {
        return v;
    }
    matches!(cfg.features.cursor_cli, Some(true))
}

/// Disk/config variant of [`cursor_paywall_override`] (no live agent `Config`).
///
/// Process-cached: pager hot paths call this often; re-reading TOML each time
/// is wasteful. Env is read inside the init closure once per process — tests
/// that mutate `GROK_CURSOR_CLI` should prefer [`suppress_supergrok_paywalls`]
/// with an explicit model id, or set the env before first call.
pub fn cursor_paywall_override_from_disk() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        if let Some(v) = cursor_cli_env_override() {
            return v;
        }
        crate::config::load_effective_config()
            .ok()
            .and_then(|root| {
                root.get("features")
                    .and_then(|f| f.get("cursor_cli"))
                    .and_then(|v| v.as_bool())
            })
            .unwrap_or(false)
    })
}

/// Whether a catalog key / model id is a Cursor-billed inference route.
///
/// Auto-discovered entries use the `cursor/<id>` prefix; manual
/// `api_backend = "cursor_cli"` configs should also use that prefix (or set
/// `apiBackend` in ACP meta — see [`is_cursor_billed_model_info`]).
pub fn is_cursor_billed_model(model_id: &str) -> bool {
    let id = model_id.trim();
    id.starts_with("cursor/") || id.eq_ignore_ascii_case("cursor")
}

/// ACP [`acp::ModelInfo`] helper: Cursor-billed if id matches or meta says so.
pub fn is_cursor_billed_model_info(
    model_id: &str,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> bool {
    if is_cursor_billed_model(model_id) {
        return true;
    }
    meta.and_then(|m| m.get("apiBackend").or_else(|| m.get("api_backend")))
        .and_then(|v| v.as_str())
        .is_some_and(|b| b.eq_ignore_ascii_case("cursor_cli"))
}

/// Suppress SuperGrok access gates / upgrade upsells / tier cosmetics.
///
/// - Explicit override ([`cursor_paywall_override_from_disk`]), **or**
/// - Active/selected model is a Cursor-billed route.
///
/// Installing `agent` alone does not suppress paywalls while the user is on
/// xAI free-tier models.
pub fn suppress_supergrok_paywalls(active_model_id: Option<&str>) -> bool {
    if cursor_paywall_override_from_disk() {
        return true;
    }
    active_model_id.is_some_and(is_cursor_billed_model)
}

/// Shell-side variant with live [`Config`] (explicit override from cfg, not disk cache).
pub fn suppress_supergrok_paywalls_cfg(cfg: &Config, active_model_id: Option<&str>) -> bool {
    if cursor_paywall_override(cfg) {
        return true;
    }
    active_model_id.is_some_and(is_cursor_billed_model)
}

/// Deprecated name: prefer [`suppress_supergrok_paywalls_cfg`] with the active model.
///
/// Without a model id this only honors the **explicit** override (not PATH auto).
#[deprecated(note = "pass the active model id to suppress_supergrok_paywalls_cfg")]
pub fn cursor_bypasses_access_gate(cfg: &Config) -> bool {
    suppress_supergrok_paywalls_cfg(cfg, None)
}

/// Deprecated name: prefer [`suppress_supergrok_paywalls`] with the active model.
#[deprecated(note = "pass the active model id to suppress_supergrok_paywalls")]
pub fn cursor_bypasses_access_gate_from_disk() -> bool {
    suppress_supergrok_paywalls(None)
}

/// True when Cursor credentials appear usable (`CURSOR_API_KEY` or `agent --list-models`).
pub fn cursor_authenticated() -> bool {
    if std::env::var_os("CURSOR_API_KEY").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    cached_discovered_models().is_ok()
}

/// Run `agent login` (blocking). Used by `/login cursor`.
pub fn run_agent_login() -> Result<(), String> {
    xai_grok_sampler::cursor_cli::run_agent_login().map_err(|e| e.to_string())
}

/// Run `agent about --format json` (blocking). Used by `/usage` for Cursor account.
pub fn fetch_cursor_account() -> Result<CursorAccountInfo, String> {
    fetch_about().map_err(|e| e.to_string())
}

/// True when the Cursor Agent CLI binary appears runnable (cached).
pub fn cursor_agent_available() -> bool {
    agent_available_cached()
}

/// Invalidate cached discovery so a post-login catalog refresh sees new models.
pub fn invalidate_discovery_cache() {
    DISCOVERY_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut slot) = DISCOVERY_CACHE.lock() {
        *slot = None;
    }
}

static DISCOVERY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
type DiscoveryCacheEntry = (u64, Result<Vec<DiscoveredCursorModel>, String>);
static DISCOVERY_CACHE: std::sync::Mutex<Option<DiscoveryCacheEntry>> = std::sync::Mutex::new(None);

/// Catalog key for a Cursor model id (`composer-2.5` → `cursor/composer-2.5`).
pub fn catalog_key(model_id: &str) -> String {
    format!("cursor/{model_id}")
}

/// Strip an optional `cursor/` prefix from a user-facing model id.
pub fn routing_slug(model_or_key: &str) -> &str {
    model_or_key.strip_prefix("cursor/").unwrap_or(model_or_key)
}

/// ACP model id for a Cursor routing slug, preferring the family catalog key.
///
/// Effort routing rewrites `SamplerConfig.model` to a variant slug
/// (`grok-4.7-low`). Look that slug (and `cursor/<slug>`) up via
/// [`ModelInfo::has_model_id`] so the session keeps the family key
/// (`cursor/grok-4.7`) after `/model … low`.
pub fn catalog_model_id(
    catalog: &IndexMap<String, ModelEntry>,
    routing_model: &str,
) -> agent_client_protocol::ModelId {
    use agent_client_protocol as acp;
    let slug = routing_slug(routing_model);
    let candidates = [slug, routing_model];
    for candidate in candidates {
        if let Some((key, _)) = catalog
            .iter()
            .rev()
            .find(|(key, entry)| key.as_str() == candidate || entry.info.has_model_id(candidate))
        {
            return acp::ModelId::new(key.clone());
        }
    }
    acp::ModelId::new(catalog_key(slug))
}

fn cached_discovered_models() -> Result<Vec<DiscoveredCursorModel>, String> {
    let generation = DISCOVERY_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    if let Ok(guard) = DISCOVERY_CACHE.lock()
        && let Some((cached_gen, cached)) = guard.as_ref()
        && *cached_gen == generation
    {
        return cached.clone();
    }
    let fresh = list_models().map_err(|e| e.to_string());
    if let Ok(mut slot) = DISCOVERY_CACHE.lock() {
        *slot = Some((generation, fresh.clone()));
    }
    fresh
}

/// Display name for a Cursor CLI catalog entry.
///
/// Cursor's `--list-models` often already includes a "Cursor …" brand in the
/// name (e.g. "Cursor Grok 4.5 Low"). Prefixing again would render
/// "Cursor: Cursor Grok …". Only add the provider tag when the CLI name does
/// not already start with "Cursor".
fn cursor_display_name(discovered_name: &str) -> String {
    let trimmed = discovered_name.trim();
    if trimmed.to_ascii_lowercase().starts_with("cursor") {
        trimmed.to_owned()
    } else {
        format!("Cursor: {trimmed}")
    }
}

/// One discovered Cursor CLI row after peeling effort / fast suffixes.
#[derive(Debug, Clone)]
struct ParsedCursorVariant {
    family_id: String,
    /// `None` for bare ids (`composer-2.5`, `auto`) and for `-fast`-only siblings.
    effort: Option<ReasoningEffort>,
    fast: bool,
    raw_id: String,
    raw_name: String,
    context_window: u64,
}

/// Peel `-fast` and a trailing effort token from a Cursor model id.
fn parse_cursor_variant(discovered: &DiscoveredCursorModel) -> ParsedCursorVariant {
    let mut fast = false;
    let mut body = discovered.id.as_str();
    if let Some(stripped) = body.strip_suffix("-fast") {
        fast = true;
        body = stripped;
    }
    let (family_id, effort) = match split_effort_suffix(body) {
        Some((family, effort)) => (family.to_owned(), Some(effort)),
        None => (body.to_owned(), None),
    };
    ParsedCursorVariant {
        family_id,
        effort,
        fast,
        raw_id: discovered.id.clone(),
        raw_name: discovered.name.clone(),
        context_window: parse_context_window_from_name(&discovered.name)
            .unwrap_or(DEFAULT_CURSOR_CONTEXT_WINDOW),
    }
}

fn split_effort_suffix(id: &str) -> Option<(&str, ReasoningEffort)> {
    const SUFFIXES: &[(&str, ReasoningEffort)] = &[
        ("-xhigh", ReasoningEffort::Xhigh),
        ("-minimal", ReasoningEffort::Minimal),
        ("-medium", ReasoningEffort::Medium),
        ("-none", ReasoningEffort::None),
        ("-high", ReasoningEffort::High),
        ("-low", ReasoningEffort::Low),
        ("-max", ReasoningEffort::Max),
    ];
    for &(suffix, effort) in SUFFIXES {
        if let Some(family) = id.strip_suffix(suffix)
            && !family.is_empty()
        {
            return Some((family, effort));
        }
    }
    None
}

/// Read a context size label out of Cursor's display name (`1M`, `200k`, …).
fn parse_context_window_from_name(name: &str) -> Option<u64> {
    let lower = name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &lower[start..i];
        let n: u64 = digits.parse().ok()?;
        let rest = lower[i..].trim_start();
        if rest.starts_with('m')
            && rest
                .chars()
                .nth(1)
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        {
            return Some(n.saturating_mul(1_000_000));
        }
        if rest.starts_with('k')
            && rest
                .chars()
                .nth(1)
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        {
            return Some(n.saturating_mul(1_000));
        }
    }
    None
}

/// Strip effort / fast words so a family keeps one stable display label.
fn family_display_name(raw_name: &str) -> String {
    let mut name = raw_name.trim().to_owned();
    // Preserve a trailing "Thinking" marker so thinking families stay distinct
    // from non-thinking siblings that share the same effort-stripped stem.
    let thinking = if let Some(stem) = name
        .strip_suffix(" Thinking")
        .or_else(|| name.strip_suffix(" thinking"))
    {
        name = stem.trim_end().to_owned();
        true
    } else {
        false
    };
    // Longest phrases first so "Extra High" wins over "High".
    const TRAILING: &[&str] = &[
        " Extra High",
        " Minimal",
        " Medium",
        " High",
        " Low",
        " Max",
        " None",
        " Fast",
    ];
    loop {
        let before = name.len();
        for suffix in TRAILING {
            if let Some(stripped) = name
                .strip_suffix(suffix)
                .or_else(|| name.strip_suffix(&suffix.to_ascii_lowercase()))
            {
                name = stripped.trim_end().to_owned();
                break;
            }
        }
        if name.len() == before {
            break;
        }
    }
    if thinking {
        name.push_str(" Thinking");
    }
    cursor_display_name(name.trim())
}

fn slot_for_effort(
    slots: &[(Option<ReasoningEffort>, ParsedCursorVariant)],
    effort: Option<ReasoningEffort>,
) -> Option<&ParsedCursorVariant> {
    slots.iter().find(|(e, _)| *e == effort).map(|(_, v)| v)
}

fn upsert_slot(
    slots: &mut Vec<(Option<ReasoningEffort>, ParsedCursorVariant)>,
    parsed: ParsedCursorVariant,
) {
    let effort = parsed.effort;
    if let Some((_, existing)) = slots.iter_mut().find(|(e, _)| *e == effort) {
        // Prefer the non-fast slug when both exist for the same effort.
        if existing.fast && !parsed.fast {
            *existing = parsed;
        }
        return;
    }
    slots.push((effort, parsed));
}

/// Collapse Cursor's flat effort/fast variants into one catalog family so
/// `/model` can chain into the same window/effort pickers as built-in models.
fn model_entries_from_discovered(models: &[DiscoveredCursorModel]) -> IndexMap<String, ModelEntry> {
    // family_id → preferred variant per effort (None = bare id). Prefer non-fast.
    let mut families: IndexMap<String, Vec<(Option<ReasoningEffort>, ParsedCursorVariant)>> =
        IndexMap::new();
    // Fast-only siblings with no effort stay as their own catalog rows.
    let mut fast_only: Vec<ParsedCursorVariant> = Vec::new();

    for discovered in models {
        let parsed = parse_cursor_variant(discovered);
        if parsed.effort.is_none() && parsed.fast {
            fast_only.push(parsed);
            continue;
        }
        upsert_slot(
            families.entry(parsed.family_id.clone()).or_default(),
            parsed,
        );
    }

    let mut out = IndexMap::new();
    for (family_id, mut slots) in families {
        // Bare id alongside effort siblings → treat as Medium (Cursor's unlabeled default).
        if slots.len() > 1
            && let Some(idx) = slots.iter().position(|(e, _)| e.is_none())
        {
            let (_, bare) = slots.remove(idx);
            if slot_for_effort(&slots, Some(ReasoningEffort::Medium)).is_none() {
                let mut bare = bare;
                bare.effort = Some(ReasoningEffort::Medium);
                slots.push((Some(ReasoningEffort::Medium), bare));
            }
        }

        let mut efforts: Vec<ReasoningEffort> = slots.iter().filter_map(|(e, _)| *e).collect();
        efforts.sort_by_key(|e| match e {
            ReasoningEffort::None => 0u8,
            ReasoningEffort::Minimal => 1,
            ReasoningEffort::Low => 2,
            ReasoningEffort::Medium => 3,
            ReasoningEffort::High => 4,
            ReasoningEffort::Xhigh => 5,
            ReasoningEffort::Max => 6,
        });
        efforts.dedup();

        let (key, entry) = if efforts.len() > 1 {
            family_entry_with_efforts(&family_id, &slots, &efforts)
        } else {
            // Single row (auto, composer-2.5, lone `-high` marketing slug).
            let variant = slots
                .into_iter()
                .next()
                .map(|(_, v)| v)
                .expect("family always has at least one variant");
            single_variant_entry(&variant)
        };
        out.insert(key, entry);
    }

    for variant in fast_only {
        let (key, entry) = single_variant_entry(&variant);
        out.entry(key).or_insert(entry);
    }
    out
}

fn preferred_default_effort(efforts: &[ReasoningEffort]) -> ReasoningEffort {
    for candidate in [
        ReasoningEffort::High,
        ReasoningEffort::Medium,
        ReasoningEffort::Xhigh,
        ReasoningEffort::Low,
        ReasoningEffort::Max,
        ReasoningEffort::Minimal,
        ReasoningEffort::None,
    ] {
        if efforts.contains(&candidate) {
            return candidate;
        }
    }
    efforts[0]
}

fn family_entry_with_efforts(
    family_id: &str,
    slots: &[(Option<ReasoningEffort>, ParsedCursorVariant)],
    efforts: &[ReasoningEffort],
) -> (String, ModelEntry) {
    let default_effort = preferred_default_effort(efforts);
    let default_variant = slot_for_effort(slots, Some(default_effort))
        .or_else(|| slots.first().map(|(_, v)| v))
        .expect("default effort present");

    // Cursor flat ids already bake context into the slug / label. Until spawn
    // can rewrite `--model` with bracket `context=` overrides, only advertise a
    // single window so `/model` does not offer a no-op window sub-menu.
    let context_window =
        NonZeroU64::new(default_variant.context_window).expect("non-zero context window");
    let context_windows = Vec::new();

    let reasoning_efforts: Vec<ReasoningEffortOption> = efforts
        .iter()
        .copied()
        .map(|value| ReasoningEffortOption {
            id: value.as_ref().to_string(),
            value,
            label: effort_label(value),
            description: None,
            default: value == default_effort,
        })
        .collect();
    let variants: Vec<ModelVariant> = efforts
        .iter()
        .filter_map(|&effort| {
            slot_for_effort(slots, Some(effort)).map(|v| ModelVariant {
                effort,
                model_id: v.raw_id.clone(),
            })
        })
        .collect();

    let key = catalog_key(family_id);
    let entry = cursor_model_entry(
        key.clone(),
        default_variant.raw_id.clone(),
        family_display_name(&default_variant.raw_name),
        context_window,
        context_windows,
        Some(default_effort),
        true,
        reasoning_efforts,
        variants,
    );
    (key, entry)
}

fn single_variant_entry(variant: &ParsedCursorVariant) -> (String, ModelEntry) {
    let catalog_id = if variant.fast && variant.effort.is_none() {
        format!("{}-fast", variant.family_id)
    } else if let Some(effort) = variant.effort {
        // Preserve a lone effort slug (`cursor-grok-4.5-high`) so existing
        // `-m cursor/…-high` configs keep resolving after grouping lands.
        format!("{}-{}", variant.family_id, effort.as_ref())
    } else {
        variant.family_id.clone()
    };
    let key = catalog_key(&catalog_id);
    let cw = NonZeroU64::new(variant.context_window).expect("non-zero");
    let entry = cursor_model_entry(
        key.clone(),
        variant.raw_id.clone(),
        family_display_name(&variant.raw_name),
        cw,
        Vec::new(),
        None,
        false,
        Vec::new(),
        Vec::new(),
    );
    (key, entry)
}

fn cursor_model_entry(
    key: String,
    routing_model: String,
    name: String,
    context_window: NonZeroU64,
    context_windows: Vec<NonZeroU64>,
    reasoning_effort: Option<ReasoningEffort>,
    supports_reasoning_effort: bool,
    reasoning_efforts: Vec<ReasoningEffortOption>,
    variants: Vec<ModelVariant>,
) -> ModelEntry {
    ModelEntry {
        info: ModelInfo {
            id: Some(key),
            model: routing_model,
            base_url: CURSOR_CLI_BASE_URL.to_owned(),
            name: Some(name),
            description: Some(
                "Routed through Cursor Agent CLI. Uses your Cursor subscription.".into(),
            ),
            max_completion_tokens: Some(32_768),
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::CursorCli,
            auth_scheme: Default::default(),
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window,
            context_windows,
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: super::config::default_agent_type(),
            inference_idle_timeout_secs: Some(600),
            max_retries: Some(2),
            hidden: false,
            user_selectable: true,
            supported_in_api: true,
            reasoning_effort,
            // Cursor bakes effort into variant ids (`…-low` / `…-high`). Expose
            // them as Grok effort options + `variants` so `/model` chains into
            // the same effort picker as built-in models, and `apply_supported_effort`
            // rewrites `--model` to the matching Cursor slug.
            supports_reasoning_effort,
            reasoning_efforts,
            variants,
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: Some(false),
            laziness_detector: Default::default(),
            ..Default::default()
        },
        mtls_cert_dir: None,
        // Sentinel key so has_own_credentials() is true without Grok login.
        api_key: Some(CURSOR_CLI_API_KEY.to_owned()),
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

fn bootstrap_cursor_entry() -> (String, ModelEntry) {
    single_variant_entry(&parse_cursor_variant(&DiscoveredCursorModel {
        id: BOOTSTRAP_CURSOR_MODEL_ID.to_owned(),
        name: "Auto".into(),
    }))
}

/// Build catalog entries for discovered Cursor models.
///
/// Effort/fast suffixes are collapsed into family rows so `/model` can offer
/// the same `[window] [effort]` completion chain as built-in models. When the
/// CLI is present but `agent --list-models` fails (typically not logged in),
/// injects a bootstrap `cursor/auto` entry so `/login cursor` and model
/// selection remain discoverable for new users.
pub fn discover_cursor_model_entries() -> IndexMap<String, ModelEntry> {
    match cached_discovered_models() {
        Ok(models) if !models.is_empty() => {
            let out = model_entries_from_discovered(&models);
            tracing::info!(
                count = out.len(),
                "cursor_cli: injected Cursor models into catalog"
            );
            out
        }
        Ok(_) => {
            tracing::warn!(
                "cursor_cli: list-models returned empty; injecting bootstrap cursor/auto"
            );
            let (key, entry) = bootstrap_cursor_entry();
            IndexMap::from([(key, entry)])
        }
        Err(err) => {
            if agent_available_cached() {
                tracing::warn!(
                    error = %err,
                    "cursor_cli: list-models failed (login required?); injecting bootstrap cursor/auto"
                );
                let (key, entry) = bootstrap_cursor_entry();
                IndexMap::from([(key, entry)])
            } else {
                tracing::warn!(error = %err, "cursor_cli: failed to list models; skipping injection");
                IndexMap::new()
            }
        }
    }
}

/// Fresh discovery bypassing the process cache (post `/login cursor`).
pub fn rediscover_cursor_model_entries() -> IndexMap<String, ModelEntry> {
    match list_models() {
        Ok(models) if !models.is_empty() => model_entries_from_discovered(&models),
        Ok(_) | Err(_) => {
            if agent_available_cached() {
                let (key, entry) = bootstrap_cursor_entry();
                IndexMap::from([(key, entry)])
            } else {
                IndexMap::new()
            }
        }
    }
}

/// Merge Cursor models into an existing catalog (does not overwrite keys).
pub fn merge_cursor_models(catalog: &mut IndexMap<String, ModelEntry>) {
    for (key, entry) in discover_cursor_model_entries() {
        catalog.entry(key).or_insert(entry);
    }
}

/// Merge a fresh discovery result (overwrites matching `cursor/` keys).
pub fn remesh_cursor_models(catalog: &mut IndexMap<String, ModelEntry>) {
    for (key, entry) in rediscover_cursor_model_entries() {
        catalog.insert(key, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_key_and_slug() {
        assert_eq!(catalog_key("auto"), "cursor/auto");
        assert_eq!(routing_slug("cursor/auto"), "auto");
        assert_eq!(routing_slug("auto"), "auto");
    }

    #[test]
    fn cursor_billed_model_prefix() {
        assert!(is_cursor_billed_model("cursor/auto"));
        assert!(is_cursor_billed_model("cursor/composer-2.5"));
        assert!(!is_cursor_billed_model("grok-4"));
        assert!(!is_cursor_billed_model("composer-2.5"));
    }

    #[test]
    fn paywalls_not_suppressed_without_model_or_override() {
        // No explicit override in this process (tests must not rely on PATH).
        // suppress with None only true when override is set.
        let _ = cursor_paywall_override_from_disk(); // warm cache
        // Active Cursor model always suppresses regardless of override.
        assert!(suppress_supergrok_paywalls(Some("cursor/auto")));
        assert!(!suppress_supergrok_paywalls(Some("grok-4")));
    }

    #[test]
    fn model_info_meta_api_backend() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "apiBackend".into(),
            serde_json::Value::String("cursor_cli".into()),
        );
        assert!(is_cursor_billed_model_info("my-cursor-model", Some(&meta)));
        assert!(!is_cursor_billed_model_info("grok-4", None));
    }

    #[test]
    fn cursor_display_name_dedupes_leading_cursor_brand() {
        assert_eq!(
            cursor_display_name("Cursor Grok 4.5 Low"),
            "Cursor Grok 4.5 Low"
        );
        assert_eq!(cursor_display_name("cursor grok"), "cursor grok");
        assert_eq!(cursor_display_name("Auto"), "Cursor: Auto");
        assert_eq!(cursor_display_name("Composer 2"), "Cursor: Composer 2");
    }

    #[test]
    fn cursor_families_expose_effort_like_builtin_models() {
        let entries = model_entries_from_discovered(&[
            DiscoveredCursorModel {
                id: "grok-4.7-low".into(),
                name: "Grok 4.7 Low".into(),
            },
            DiscoveredCursorModel {
                id: "grok-4.7-low-fast".into(),
                name: "Grok 4.7 Low Fast".into(),
            },
            DiscoveredCursorModel {
                id: "grok-4.7-high".into(),
                name: "Grok 4.7 High".into(),
            },
            DiscoveredCursorModel {
                id: "grok-4.7-xhigh".into(),
                name: "Grok 4.7 Extra High".into(),
            },
            DiscoveredCursorModel {
                id: "auto".into(),
                name: "Auto".into(),
            },
            DiscoveredCursorModel {
                id: "composer-2.5".into(),
                name: "Composer 2.5".into(),
            },
            DiscoveredCursorModel {
                id: "composer-2.5-fast".into(),
                name: "Composer 2.5 Fast".into(),
            },
            DiscoveredCursorModel {
                id: "cursor-grok-4.5-high".into(),
                name: "Cursor Grok 4.5".into(),
            },
        ]);

        let grok = entries
            .get("cursor/grok-4.7")
            .expect("effort siblings collapse to one family");
        assert_eq!(grok.info.name.as_deref(), Some("Cursor: Grok 4.7"));
        assert!(
            grok.info.supports_reasoning_effort,
            "multi-effort Cursor families must chain `/model` into the effort picker"
        );
        assert_eq!(
            grok.info.reasoning_effort,
            Some(ReasoningEffort::High),
            "default effort prefers high when present"
        );
        assert_eq!(grok.info.model, "grok-4.7-high");
        assert_eq!(
            grok.info.model_at(ReasoningEffort::Low),
            "grok-4.7-low",
            "effort routing must prefer the non-fast slug"
        );
        assert_eq!(grok.info.model_at(ReasoningEffort::Xhigh), "grok-4.7-xhigh");

        let auto = entries.get("cursor/auto").expect("auto stays a single row");
        assert_eq!(auto.info.name.as_deref(), Some("Cursor: Auto"));
        assert!(!auto.info.supports_reasoning_effort);

        let composer = entries
            .get("cursor/composer-2.5")
            .expect("composer without effort stays a single row");
        assert!(!composer.info.supports_reasoning_effort);
        assert!(
            entries.contains_key("cursor/composer-2.5-fast"),
            "fast-only siblings remain selectable on their own"
        );

        let lone = entries
            .get("cursor/cursor-grok-4.5-high")
            .expect("a lone effort slug keeps a stable catalog key");
        assert!(
            !lone.info.supports_reasoning_effort,
            "a single effort level must not open an effort sub-menu"
        );
        assert_eq!(
            lone.info.name.as_deref(),
            Some("Cursor Grok 4.5"),
            "must not render as 'Cursor: Cursor Grok …'"
        );
    }

    #[test]
    fn bare_codex_id_joins_effort_family_as_medium() {
        let entries = model_entries_from_discovered(&[
            DiscoveredCursorModel {
                id: "gpt-5.3-codex-low".into(),
                name: "Codex 5.3 Low".into(),
            },
            DiscoveredCursorModel {
                id: "gpt-5.3-codex".into(),
                name: "Codex 5.3".into(),
            },
            DiscoveredCursorModel {
                id: "gpt-5.3-codex-high".into(),
                name: "Codex 5.3 High".into(),
            },
        ]);
        let family = entries
            .get("cursor/gpt-5.3-codex")
            .expect("bare + effort siblings share one family");
        assert!(family.info.supports_reasoning_effort);
        assert_eq!(
            family.info.model_at(ReasoningEffort::Medium),
            "gpt-5.3-codex"
        );
        assert_eq!(
            family.info.model_at(ReasoningEffort::Low),
            "gpt-5.3-codex-low"
        );
    }

    #[test]
    fn context_window_parsed_from_display_name() {
        assert_eq!(
            parse_context_window_from_name("Claude Opus 5.5 1M High"),
            Some(1_000_000)
        );
        assert_eq!(
            parse_context_window_from_name("Composer 2.5"),
            None,
            "no size label keeps the Cursor default"
        );
    }

    #[test]
    fn thinking_families_keep_thinking_in_display_name() {
        let entries = model_entries_from_discovered(&[
            DiscoveredCursorModel {
                id: "claude-opus-5-high".into(),
                name: "Claude Opus 5 1M".into(),
            },
            DiscoveredCursorModel {
                id: "claude-opus-5-thinking-high".into(),
                name: "Claude Opus 5 1M Thinking".into(),
            },
            DiscoveredCursorModel {
                id: "claude-opus-5-thinking-xhigh".into(),
                name: "Claude Opus 5 1M Extra High Thinking".into(),
            },
        ]);
        assert_eq!(
            entries
                .get("cursor/claude-opus-5-high")
                .and_then(|e| e.info.name.as_deref()),
            Some("Cursor: Claude Opus 5 1M"),
            "a lone non-thinking slug keeps its effort in the catalog key"
        );
        let thinking = entries
            .get("cursor/claude-opus-5-thinking")
            .expect("thinking siblings form their own family");
        assert_eq!(
            thinking.info.name.as_deref(),
            Some("Cursor: Claude Opus 5 1M Thinking")
        );
        assert!(thinking.info.supports_reasoning_effort);
    }

    #[test]
    fn catalog_model_id_keeps_family_key_after_effort_routing() {
        let entries = model_entries_from_discovered(&[
            DiscoveredCursorModel {
                id: "grok-4.7-low".into(),
                name: "Grok 4.7 Low".into(),
            },
            DiscoveredCursorModel {
                id: "grok-4.7-high".into(),
                name: "Grok 4.7 High".into(),
            },
        ]);
        assert_eq!(
            catalog_model_id(&entries, "grok-4.7-low").0.as_ref(),
            "cursor/grok-4.7"
        );
        assert_eq!(
            catalog_model_id(&entries, "grok-4.7-high").0.as_ref(),
            "cursor/grok-4.7"
        );
        assert_eq!(
            catalog_model_id(&entries, "cursor/grok-4.7").0.as_ref(),
            "cursor/grok-4.7"
        );
    }
}
