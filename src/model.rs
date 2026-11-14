//! Per-role model selection, fallback, and resolved-model capture.
//!
//! Configured via env vars (unset → server-side auto, as before):
//!   ACTMATON_MODEL / ACTMATON_FUSION_TIER                  (both roles)
//!   ACTMATON_RUN_MODEL / ACTMATON_RUN_FUSION_TIER          (run only, wins)
//!   ACTMATON_PROPOSE_MODEL / ACTMATON_PROPOSE_FUSION_TIER  (propose only, wins)

use std::future::Future;

use github_copilot_sdk::{CapiSessionOptions, FusionTier, SessionConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Run,
    Propose,
}

impl Role {
    fn prefix(self) -> &'static str {
        match self {
            Role::Run => "ACTMATON_RUN",
            Role::Propose => "ACTMATON_PROPOSE",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelConfig {
    pub model: Option<String>,
    pub fusion_tier: Option<String>,
}

const TIERS: [&str; 3] = ["standard", "high", "max"];

impl ModelConfig {
    /// Parse raw values. An invalid tier is ignored and reported in the warnings.
    pub fn parse(model: Option<&str>, tier: Option<&str>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let clean = |v: Option<&'_ str>| -> Option<String> {
            v.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        };
        let fusion_tier = clean(tier).and_then(|t| {
            let t = t.to_ascii_lowercase();
            if TIERS.contains(&t.as_str()) {
                Some(t)
            } else {
                warnings.push(format!(
                    "ignoring invalid fusion tier {t:?} (expected one of {})",
                    TIERS.join(", ")
                ));
                None
            }
        });
        let model = clean(model);
        (Self { model, fusion_tier }, warnings)
    }

    pub fn from_env(role: Role) -> (Self, Vec<String>) {
        Self::from_lookup(role, |k| std::env::var(k).ok())
    }

    fn from_lookup(role: Role, get: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let pick = |suffix: &str| {
            get(&format!("{}_{suffix}", role.prefix())).or_else(|| get(&format!("ACTMATON_{suffix}")))
        };
        Self::parse(pick("MODEL").as_deref(), pick("FUSION_TIER").as_deref())
    }

    pub fn is_default(&self) -> bool {
        self.model.is_none() && self.fusion_tier.is_none()
    }

    pub fn describe(&self) -> String {
        if self.is_default() {
            return "default (auto)".to_string();
        }
        format!(
            "model={} fusion_tier={}",
            self.model.as_deref().unwrap_or("(auto)"),
            self.fusion_tier.as_deref().unwrap_or("(none)")
        )
    }

    pub fn apply(&self, mut cfg: SessionConfig) -> SessionConfig {
        if let Some(m) = &self.model {
            cfg = cfg.with_model(m.clone());
        }
        if let Some(t) = &self.fusion_tier {
            let tier = match t.as_str() {
                "standard" => FusionTier::Standard,
                "max" => FusionTier::Max,
                _ => FusionTier::High,
            };
            cfg = cfg.with_capi(CapiSessionOptions::default().with_fusion_tier(tier));
        }
        cfg
    }
}

/// Heuristic: does this error look like a model-availability/entitlement failure?
pub fn is_model_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("model")
        && (e.contains("not available") || e.contains("unavailable") || e.contains("not supported"))
        || e.contains("auto preference")
        || e.contains("fusion")
        || e.contains("not available for the user")
        || e.contains("plan")
            && (e.contains("not available") || e.contains("not included") || e.contains("upgrade"))
}

/// Model named by a `session.model_change` event, if that is what this is.
pub fn resolved_model(event: &github_copilot_sdk::SessionEvent) -> Option<String> {
    if !format!("{:?}", event.parsed_type()).contains("ModelChange") {
        return None;
    }
    event
        .data
        .get("newModel")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Outcome of an attempt: the value plus the model the session resolved to.
pub struct Attempt<T> {
    pub value: T,
    pub resolved_model: Option<String>,
}

/// Run `attempt` with the configured model; on a model-availability error,
/// note it prominently and retry once with the default config.
pub async fn with_fallback<T, F, Fut>(
    config: &ModelConfig,
    notes: &mut Vec<String>,
    mut attempt: F,
) -> Result<Attempt<T>, Box<dyn std::error::Error>>
where
    F: FnMut(ModelConfig) -> Fut,
    Fut: Future<Output = Result<Attempt<T>, Box<dyn std::error::Error>>>,
{
    notes.push(format!("model config: {}", config.describe()));
    match attempt(config.clone()).await {
        Err(e) if !config.is_default() && is_model_error(&e.to_string()) => {
            let msg = format!(
                "!! MODEL FALLBACK: configured {} failed ({e}); retrying with default (auto) config",
                config.describe()
            );
            eprintln!("{msg}");
            notes.push(msg);
            attempt(ModelConfig::default()).await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn unset_is_default() {
        let (c, w) = ModelConfig::from_lookup(Role::Run, lookup(&[]));
        assert!(c.is_default() && w.is_empty());
    }

    #[test]
    fn model_only_and_model_plus_tier() {
        let (c, _) = ModelConfig::from_lookup(Role::Run, lookup(&[("ACTMATON_MODEL", "m1")]));
        assert_eq!(c.model.as_deref(), Some("m1"));
        assert_eq!(c.fusion_tier, None);
        let (c, w) = ModelConfig::parse(Some("m1"), Some("High"));
        assert_eq!(c.fusion_tier.as_deref(), Some("high"));
        assert!(w.is_empty());
    }

    #[test]
    fn invalid_tier_ignored_with_warning() {
        let (c, w) = ModelConfig::parse(Some("m1"), Some("ultra"));
        assert_eq!(c.fusion_tier, None);
        assert_eq!(c.model.as_deref(), Some("m1"));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn role_specific_overrides_shared() {
        let env = [("ACTMATON_MODEL", "a"), ("ACTMATON_PROPOSE_MODEL", "b")];
        let (run, _) = ModelConfig::from_lookup(Role::Run, lookup(&env));
        let (prop, _) = ModelConfig::from_lookup(Role::Propose, lookup(&env));
        assert_eq!(run.model.as_deref(), Some("a"));
        assert_eq!(prop.model.as_deref(), Some("b"));
    }

    #[test]
    fn detects_model_errors() {
        assert!(is_model_error("Model claude-opus-4.5 is not available"));
        assert!(is_model_error("Auto preference \"hydrafusion\" is disabled: This tier is not available for the user's Copilot plan"));
        assert!(!is_model_error("connection reset"));
    }

    #[tokio::test]
    async fn falls_back_once_on_model_error() {
        let cfg = ModelConfig::parse(Some("claude-opus-4.5"), None).0;
        let mut notes = Vec::new();
        let r = with_fallback(&cfg, &mut notes, |c| async move {
            if c.is_default() {
                Ok(Attempt { value: 1, resolved_model: Some("auto-x".into()) })
            } else {
                Err("Model claude-opus-4.5 is not available".into())
            }
        })
        .await
        .unwrap();
        assert_eq!(r.resolved_model.as_deref(), Some("auto-x"));
        assert!(notes.iter().any(|n| n.contains("MODEL FALLBACK")));
    }
}
