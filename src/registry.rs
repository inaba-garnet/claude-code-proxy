use crate::{
    anthropic::{json_error, schema::MessagesRequest},
    config::AliasProvider,
    provider::{CliHandlers, Provider, RequestContext},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use axum::{http::StatusCode, response::Response};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "fable",
    "claude-fable-5",
];

pub const CURSOR_PREFIXES: &[&str] = &["cursor:", "cursor-plan:", "cursor-ask:"];

const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

pub(crate) const CODEX_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
];

pub(crate) const KIMI_MODELS: &[&str] = &["kimi-for-coding", "kimi-k2.6", "k2.6"];
pub(crate) const GROK_MODELS: &[&str] = &["grok-composer-2.5-fast", "grok-4.5"];

/// Models routed to the OpenAI-compatible provider when nothing is configured:
/// the full OpenCode Go catalog. Overridable via `CCP_OPENCODE_MODELS` or
/// `opencode.models` in config.json.
///
/// `grok-4.5` and `kimi-k2.6` are also native provider model ids and lose to
/// them on a bare lookup; reach those through `OPENCODE_PREFIX`.
pub const OPENCODE_DEFAULT_MODELS: &[&str] = &[
    "grok-4.5",
    "glm-5.2",
    "glm-5.1",
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.6",
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "mimo-v2.5",
    "mimo-v2.5-pro",
    "minimax-m3",
    "minimax-m2.7",
    "minimax-m2.5",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.6-plus",
];

/// Forces a model to the OpenAI-compatible provider regardless of which other
/// provider claims the same id. Mirrors the `opencode-go/<model>` form used in
/// OpenCode's own configuration.
pub const OPENCODE_PREFIX: &str = "opencode-go/";

pub struct Registry {
    alias_provider: AliasProvider,
    models: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Registry {
    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert("codex".into(), expand_codex_models());
        models.insert(
            "kimi".into(),
            KIMI_MODELS.iter().map(|m| (*m).to_string()).collect(),
        );
        models.insert("cursor".into(), build_cursor_models());
        // Anthropic owns no model IDs of its own: it is only reachable as the
        // alias provider, and `supported_models_for` adds the aliases there.
        models.insert("anthropic".into(), Vec::new());
        models.insert("opencode".into(), crate::config::opencode_models());
        models.insert(
            "grok".into(),
            GROK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
        );

        let mut handlers = BTreeMap::new();
        for (name, entries) in &models {
            let handler: Arc<dyn Provider> = match name.as_str() {
                "codex" => Arc::new(crate::providers::codex::CodexProvider::new()),
                "kimi" => Arc::new(crate::providers::kimi::KimiProvider::new()),
                "cursor" => Arc::new(crate::providers::cursor::CursorProvider::new()),
                "grok" => Arc::new(crate::providers::grok::GrokProvider::new()),
                "anthropic" => Arc::new(AnthropicProvider),
                "opencode" => Arc::new(crate::providers::opencode::OpenCodeProvider::new()),
                _ => Arc::new(PlaceholderProvider::new(name, entries.clone())),
            };
            handlers.insert(name.clone(), handler);
        }

        Self {
            alias_provider,
            models,
            handlers,
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.models.get(provider).cloned().unwrap_or_default();
        if provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.handlers.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        if is_anthropic_alias(&normalized) {
            // Session affinity keeps a session pinned to whichever backend its
            // real models used. Passthrough is the exception: choosing it says
            // "Claude models stay on Claude", so a Codex or Kimi turn elsewhere
            // in the session must not drag the aliases along with it.
            let target = if self.alias_provider == AliasProvider::Anthropic {
                &self.alias_provider
            } else {
                session_affinity.unwrap_or(&self.alias_provider)
            };
            return self.handlers.get(target.as_str()).cloned();
        }
        // In passthrough mode relay every `claude-*` id verbatim — including
        // dated and newly-released ids that are not in the fixed alias list — so
        // routing keeps working across Claude Code model bumps. Gated on the
        // Anthropic alias provider so codex/kimi users are unaffected.
        if self.alias_provider == AliasProvider::Anthropic && normalized.starts_with("claude-") {
            return self.handlers.get("anthropic").cloned();
        }
        if is_cursor_model(&normalized) {
            return self.handlers.get("cursor").cloned();
        }
        // Checked before the generic lookup so the prefix wins over a native
        // provider that claims the same bare id.
        if normalized.starts_with(OPENCODE_PREFIX) {
            return self.handlers.get("opencode").cloned();
        }

        for (name, models) in &self.models {
            if models.iter().any(|candidate| candidate == &normalized) {
                return self.handlers.get(name).cloned();
            }
        }

        None
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            if models.is_empty() {
                continue;
            }
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Supported: {}.", parts.join("; "))
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

pub fn is_cursor_model(model: &str) -> bool {
    if CURSOR_LEGACY_MODELS.contains(&model) {
        return true;
    }

    CURSOR_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

/// Marker for the verbatim Anthropic route.
///
/// Requests for this provider never reach the trait methods: `dispatch_request`
/// forwards them from the raw bytes before any JSON parsing happens, which is
/// what keeps the relay byte-exact. The entry exists so that provider naming,
/// model listing, and monitoring treat `anthropic` like any other provider.
struct AnthropicProvider;

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<String> {
        Vec::new()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &ANTHROPIC_CLI
    }

    async fn handle_messages(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        anthropic_route_bug()
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
        anthropic_route_bug()
    }
}

fn anthropic_route_bug() -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        "anthropic passthrough reached the translated path; this is a routing bug",
    )
}

/// Anthropic stores no credentials of its own: whatever the client sends is
/// relayed, so there is nothing to log in to, inspect, or delete.
#[derive(Clone, Copy)]
struct AnthropicCli;

impl CliHandlers for AnthropicCli {
    fn login(&self) -> Result<()> {
        Err(anyhow!(
            "anthropic: no login needed; the proxy relays the credentials Claude Code already sends"
        ))
    }

    fn device(&self) -> Result<()> {
        self.login()
    }

    fn status(&self) -> Result<()> {
        Ok(())
    }

    fn logout(&self) -> Result<()> {
        Ok(())
    }
}

const ANTHROPIC_CLI: AnthropicCli = AnthropicCli;

struct PlaceholderProvider {
    name: &'static str,
    models: Vec<String>,
}

impl PlaceholderProvider {
    fn new(name: &str, models: Vec<String>) -> Self {
        let name = match name {
            "codex" => "codex",
            "kimi" => "kimi",
            "cursor" => "cursor",
            "grok" => "grok",
            _ => "codex",
        };
        Self { name, models }
    }
}

#[async_trait]
impl Provider for PlaceholderProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supported_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        match self.name {
            "codex" => &CODEX_CLI,
            "kimi" => &KIMI_CLI,
            "cursor" => &CURSOR_CLI,
            "grok" => &GROK_CLI,
            _ => &CODEX_CLI,
        }
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("messages", &ctx.provider)
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        placeholder_provider_response("count_tokens", &ctx.provider)
    }
}

fn placeholder_provider_response(route: &str, provider: &str) -> Response {
    let _ = route;
    json_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_provider_error",
        format!("provider '{}' is not yet implemented", provider),
    )
}

#[derive(Clone, Copy)]
struct PlaceholderCli {
    provider: &'static str,
}

impl CliHandlers for PlaceholderCli {
    fn login(&self) -> Result<()> {
        Err(anyhow!("{}: browser login not supported", self.provider))
    }

    fn device(&self) -> Result<()> {
        Err(anyhow!("{}: device login not supported", self.provider))
    }

    fn status(&self) -> Result<()> {
        use serde_json::Value;
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        if crate::auth::load_auth_file_with_legacy::<Value>(&path, &legacy).is_some() {
            Ok(())
        } else {
            Err(anyhow!("Not authenticated"))
        }
    }

    fn logout(&self) -> Result<()> {
        let path = crate::paths::provider_auth_file(self.provider);
        let legacy = crate::paths::provider_legacy_auth_file(self.provider);
        let _ = crate::auth::delete_auth_file(&path, &legacy);
        Ok(())
    }
}

const CODEX_CLI: PlaceholderCli = PlaceholderCli { provider: "codex" };
const KIMI_CLI: PlaceholderCli = PlaceholderCli { provider: "kimi" };
const CURSOR_CLI: PlaceholderCli = PlaceholderCli { provider: "cursor" };
const GROK_CLI: PlaceholderCli = PlaceholderCli { provider: "grok" };

fn expand_codex_models() -> Vec<String> {
    let mut set = HashSet::new();
    let mut out = Vec::new();
    for model in CODEX_MODELS {
        if set.insert((*model).to_string()) {
            out.push((*model).to_string());
        }
        let fast = format!("{model}-fast");
        if set.insert(fast.clone()) {
            out.push(fast);
        }
    }
    out.sort_unstable();
    out
}

fn build_cursor_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
    }

    #[test]
    fn alias_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Kimi);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "kimi");
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in ["claude-sonnet-5", "fable", "claude-fable-5"] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn anthropic_alias_provider_routes_aliases_to_passthrough() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for model in ["sonnet", "claude-sonnet-5", "haiku", "claude-haiku-4-5"] {
            assert_eq!(
                registry
                    .provider_for_model(model, None)
                    .expect("provider")
                    .name(),
                "anthropic",
                "{model} should pass through"
            );
        }
    }

    /// Mixing providers in one session must not move Claude models off Claude:
    /// a Codex subagent would otherwise capture the main conversation.
    #[test]
    fn passthrough_aliases_ignore_session_affinity() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for affinity in [AliasProvider::Codex, AliasProvider::Kimi] {
            assert_eq!(
                registry
                    .provider_for_model("claude-sonnet-5", Some(&affinity))
                    .expect("provider")
                    .name(),
                "anthropic",
                "affinity to {} must not capture the alias",
                affinity.as_str()
            );
        }
    }

    /// The affinity behaviour itself is unchanged for the translating providers.
    #[test]
    fn translating_alias_providers_still_honour_session_affinity() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("claude-sonnet-5", Some(&AliasProvider::Kimi))
                .expect("provider")
                .name(),
            "kimi"
        );
    }

    #[test]
    fn passthrough_relays_unlisted_claude_models() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for model in [
            "claude-sonnet-5-20260101",
            "claude-opus-5",
            "claude-haiku-9-latest",
            "claude-sonnet-5[1m]",
        ] {
            assert_eq!(
                registry
                    .provider_for_model(model, None)
                    .expect("provider")
                    .name(),
                "anthropic",
                "{model} should relay verbatim in passthrough mode"
            );
        }
    }

    #[test]
    fn non_passthrough_alias_provider_ignores_unlisted_claude_models() {
        // Unchanged behavior: for a translating alias provider, a dated/unknown
        // claude id is not an alias and belongs to no provider list, so the
        // caller still returns its 400.
        let registry = Registry::new(AliasProvider::Codex);
        assert!(
            registry
                .provider_for_model("claude-sonnet-5-20260101", None)
                .is_none()
        );
    }

    #[test]
    fn anthropic_alias_provider_leaves_other_providers_alone() {
        let registry = Registry::new(AliasProvider::Anthropic);
        assert_eq!(
            registry
                .provider_for_model("gpt-5.4", None)
                .expect("provider")
                .name(),
            "codex"
        );
        assert_eq!(
            registry
                .provider_for_model("kimi-k2.6", None)
                .expect("provider")
                .name(),
            "kimi"
        );
        assert!(registry.provider_for_model("no-such-model", None).is_none());
    }

    /// `grok-4.5` and `kimi-k2.6` exist in both the OpenCode Go catalog and a
    /// native provider. The bare id keeps going to the native provider; the
    /// prefix is how you reach OpenCode's copy.
    #[test]
    fn opencode_prefix_wins_over_a_colliding_native_model() {
        let registry = Registry::new(AliasProvider::Codex);
        for (bare, native) in [("grok-4.5", "grok"), ("kimi-k2.6", "kimi")] {
            assert_eq!(
                registry
                    .provider_for_model(bare, None)
                    .expect("provider")
                    .name(),
                native,
                "{bare} should stay on its native provider"
            );
            assert_eq!(
                registry
                    .provider_for_model(&format!("{OPENCODE_PREFIX}{bare}"), None)
                    .expect("provider")
                    .name(),
                "opencode",
                "{OPENCODE_PREFIX}{bare} should reach opencode"
            );
        }
    }

    #[test]
    fn opencode_catalog_models_route_without_a_prefix() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in ["glm-5.2", "kimi-k3", "minimax-m3", "qwen3.7-max"] {
            assert_eq!(
                registry
                    .provider_for_model(model, None)
                    .expect("provider")
                    .name(),
                "opencode",
                "{model} should route to opencode"
            );
        }
    }

    #[test]
    fn cursor_prefix_routes() {
        let registry = Registry::new(AliasProvider::Codex);
        assert_eq!(
            registry
                .provider_for_model("cursor:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-plan:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
        assert_eq!(
            registry
                .provider_for_model("cursor-ask:gpt-5.5", None)
                .unwrap()
                .name(),
            "cursor"
        );
    }
}
