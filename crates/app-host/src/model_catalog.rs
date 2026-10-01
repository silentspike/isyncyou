//! Account-generation-bound discovery. Only model capabilities survive the wire boundary;
//! provider prompts, tools, descriptions and other client harness metadata are discarded.

use super::*;
use serde::{Deserialize, Serialize};

const MAX_CATALOG_WIRE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SAVED_BYTES: usize = 64 * 1024;
const MAX_MODELS: usize = 128;
const FRESH_MS: u64 = 60_000;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Model {
    pub id: String,
    pub label: String,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub context_window_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
    pub use_responses_lite: Option<bool>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    version: u32,
    generation: String,
    fetched_at_ms: u64,
    #[serde(default)]
    client_version: Option<String>,
    pub models: Vec<Model>,
}

fn store_id(provider: ProductProviderId) -> String {
    format!("model-catalog-v1-{}", provider.wire())
}

fn credential_id(provider: ProductProviderId) -> &'static str {
    match provider {
        ProductProviderId::Claude => SUBSCRIPTION_CREDENTIAL_ID,
        ProductProviderId::Codex => CODEX_CREDENTIAL_ID,
    }
}

pub(super) fn load(root: &Path, provider: ProductProviderId) -> Option<Catalog> {
    let meta = load_product_bundle_meta(root, credential_id(provider))?;
    if meta.lifecycle != CredentialLifecycle::Active
        || meta.policy_fingerprint != oauth_policy_fingerprint(provider)
    {
        return None;
    }
    let raw = agent_credential_store(root)
        .ok()?
        .get_bounded(
            isyncyou_agent::SecretClass::ProductSettings,
            &store_id(provider),
            MAX_SAVED_BYTES + 4096,
            MAX_SAVED_BYTES,
        )
        .ok()??;
    let catalog: Catalog = serde_json::from_slice(raw.expose()).ok()?;
    (catalog.version == 1
        && catalog.generation == meta.generation
        && catalog.client_version == catalog_client_version(provider)
        && valid_models(&catalog.models, provider))
    .then_some(catalog)
}

fn catalog_client_version(provider: ProductProviderId) -> Option<String> {
    (provider == ProductProviderId::Codex)
        .then(|| isyncyou_agent::CodexConfig::default().cli_version)
}

pub(super) fn model(root: &Path, provider: ProductProviderId, id: &str) -> Option<Model> {
    load(root, provider)?
        .models
        .into_iter()
        .find(|model| model.id == id)
}

fn valid_id(value: &str, prefix: &str) -> bool {
    !value.is_empty()
        && value.starts_with(prefix)
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_models(models: &[Model], provider: ProductProviderId) -> bool {
    let mut ids = BTreeSet::new();
    !models.is_empty()
        && models.len() <= MAX_MODELS
        && models.iter().all(|model| {
            valid_id(
                &model.id,
                if provider == ProductProviderId::Claude {
                    "claude-"
                } else {
                    ""
                },
            ) && ids.insert(&model.id)
                && !model.label.is_empty()
                && model.label.len() <= 128
                && !model.label.chars().any(char::is_control)
                && match provider {
                    ProductProviderId::Claude => {
                        model.reasoning_efforts.is_empty()
                            && model.default_reasoning_effort.is_none()
                            && model.use_responses_lite.is_none()
                    }
                    ProductProviderId::Codex => {
                        !model.reasoning_efforts.is_empty()
                            && model.default_reasoning_effort.is_some()
                            && model.use_responses_lite.is_some()
                    }
                }
                && model.reasoning_efforts.len() <= 9
                && model
                    .reasoning_efforts
                    .iter()
                    .all(|value| isyncyou_agent::CodexReasoningEffort::parse(value).is_some())
                && model
                    .default_reasoning_effort
                    .as_ref()
                    .is_none_or(|value| model.reasoning_efforts.contains(value))
                && model
                    .context_window_tokens
                    .is_none_or(|value| (4096..=4_000_000).contains(&value))
                && model
                    .max_output_tokens
                    .is_none_or(|value| (1024..=512_000).contains(&value))
        })
}

fn token_limit(value: &serde_json::Value) -> Option<usize> {
    value
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (4096..=4_000_000).contains(n))
}

fn parse(provider: ProductProviderId, body: &[u8]) -> Result<Vec<Model>, String> {
    if body.len() > MAX_CATALOG_WIRE_BYTES {
        return Err("model_catalog_size_limit".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "model_catalog_invalid")?;
    if provider == ProductProviderId::Claude && value["has_more"].as_bool() != Some(false) {
        return Err("model_catalog_incomplete".into());
    }
    let entries = value[if provider == ProductProviderId::Claude {
        "data"
    } else {
        "models"
    }]
    .as_array()
    .ok_or("model_catalog_invalid")?;
    if entries.len() > MAX_MODELS {
        return Err("model_catalog_size_limit".into());
    }
    let mut models = Vec::new();
    for entry in entries {
        if provider == ProductProviderId::Codex && entry["visibility"].as_str() != Some("list") {
            continue;
        }
        let (id_key, label_key) = if provider == ProductProviderId::Claude {
            ("id", "display_name")
        } else {
            ("slug", "display_name")
        };
        let id = entry[id_key].as_str().ok_or("model_catalog_invalid")?;
        if provider == ProductProviderId::Codex
            && entry["input_modalities"]
                .as_array()
                .is_some_and(|modalities| {
                    !modalities
                        .iter()
                        .any(|value| value.as_str() == Some("text"))
                })
        {
            continue;
        }
        let mut efforts = Vec::new();
        if provider == ProductProviderId::Codex {
            let levels = entry["supported_reasoning_levels"]
                .as_array()
                .ok_or("model_catalog_invalid")?;
            for level in levels {
                let effort = level["effort"].as_str().ok_or("model_catalog_invalid")?;
                if isyncyou_agent::CodexReasoningEffort::parse(effort).is_some()
                    && !efforts.iter().any(|value| value == effort)
                {
                    efforts.push(effort.to_owned());
                }
            }
            if efforts.is_empty() {
                continue;
            }
        }
        let default = if provider == ProductProviderId::Codex {
            entry["default_reasoning_level"]
                .as_str()
                .filter(|value| efforts.iter().any(|effort| effort == value))
                .map(str::to_owned)
                .or_else(|| efforts.first().cloned())
        } else {
            None
        };
        models.push(Model {
            id: id.to_owned(),
            label: entry[label_key]
                .as_str()
                .ok_or("model_catalog_invalid")?
                .to_owned(),
            reasoning_efforts: efforts,
            default_reasoning_effort: default,
            // Keep conservative #628 budgets when the catalog does not specify both limits.
            context_window_tokens: token_limit(&entry["context_window"]),
            max_output_tokens: if provider == ProductProviderId::Claude {
                Some(4096)
            } else {
                token_limit(&entry["max_output_tokens"])
            },
            use_responses_lite: if provider == ProductProviderId::Codex {
                Some(entry["use_responses_lite"].as_bool().unwrap_or(false))
            } else {
                None
            },
        });
    }
    if !valid_models(&models, provider) {
        return Err("model_catalog_invalid".into());
    }
    Ok(models)
}

pub(super) fn public(catalog: &Catalog, state: &str) -> serde_json::Value {
    serde_json::json!({"state": state, "fetched_at_ms": catalog.fetched_at_ms, "models": catalog.models.iter().map(|model| {
        serde_json::json!({"id": model.id, "label": model.label, "reasoning_efforts": model.reasoning_efforts.iter().map(|id| {
            serde_json::json!({"id": id, "label": effort_label(id)})
        }).collect::<Vec<_>>(), "default_reasoning_effort": model.default_reasoning_effort})
    }).collect::<Vec<_>>()})
}

fn effort_label(id: &str) -> &'static str {
    match id {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Light",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Maximum",
        "ultra" => "Ultra",
        "persistent" => "Persistent",
        _ => "Medium",
    }
}

impl DaemonAgent {
    pub(super) fn refresh_model_catalog(
        &self,
        provider: ProductProviderId,
    ) -> Result<String, String> {
        let _refresh = self.model_catalog_gates[if provider == ProductProviderId::Claude {
            0
        } else {
            1
        }]
        .try_lock()
        .map_err(|_| "model_catalog_busy")?;
        let lease_id =
            account_lifecycle::mint_operation_id().map_err(|_| "model_catalog_unavailable")?;
        let _lease = self
            .provider_leases
            .acquire_shared(
                &self.oauth_dir,
                provider,
                lease_id,
                account_lifecycle::ProviderOperationKind::Turn,
            )
            .map_err(|_| "model_catalog_busy")?;
        let (token, headers, meta) = {
            let _runtime = self
                .product_runtime_gate
                .lock()
                .map_err(|_| "model_catalog_busy")?;
            let _file = acquire_product_runtime_file_lock(&self.oauth_dir)?;
            match provider {
                ProductProviderId::Claude => match self.claude_product_bundle_state() {
                    ProductCredentialState::PresentValid((credential, meta))
                        if self.provider_activation_valid_for_meta(provider, &meta) =>
                    {
                        (
                            credential.access_token,
                            vec![
                                ("anthropic-version".into(), "2023-06-01".into()),
                                (
                                    "anthropic-beta".into(),
                                    "claude-code-20250219,oauth-2025-04-20".into(),
                                ),
                            ],
                            meta,
                        )
                    }
                    _ => return Err("model_catalog_not_ready".into()),
                },
                ProductProviderId::Codex => match self.codex_product_bundle_state() {
                    ProductCredentialState::PresentValid((credential, meta))
                        if self.provider_activation_valid_for_meta(provider, &meta) =>
                    {
                        (
                            credential.access_token,
                            vec![
                                ("chatgpt-account-id".into(), credential.account_id),
                                (
                                    "originator".into(),
                                    isyncyou_agent::oauth::CODEX_OAUTH_ORIGINATOR.into(),
                                ),
                                (
                                    "user-agent".into(),
                                    format!(
                                        "{}/{}",
                                        isyncyou_agent::oauth::CODEX_OAUTH_ORIGINATOR,
                                        isyncyou_agent::CodexConfig::default().cli_version
                                    ),
                                ),
                            ],
                            meta,
                        )
                    }
                    _ => return Err("model_catalog_not_ready".into()),
                },
            }
        };
        if let Some(catalog) = load(&self.oauth_dir, provider).filter(|catalog| {
            let now = (self.credential_now_ms)();
            catalog.fetched_at_ms <= now && now - catalog.fetched_at_ms < FRESH_MS
        }) {
            return Ok(public(&catalog, "ready").to_string());
        }
        let mut headers = headers;
        headers.push(("authorization".into(), format!("Bearer {token}")));
        let url = match provider {
            ProductProviderId::Claude => {
                "https://api.anthropic.com/v1/models?limit=1000".to_string()
            }
            ProductProviderId::Codex => format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                isyncyou_agent::CodexConfig::default().cli_version
            ),
        };
        let response = isyncyou_agent::http::HttpTransport::shared()
            .and_then(|http| http.get_catalog_json(&url, &headers, MAX_CATALOG_WIRE_BYTES))
            .map_err(|error| match error {
                isyncyou_agent::AgentError::Transport(code)
                    if code == "provider_metadata_size_limit" =>
                {
                    "model_catalog_size_limit"
                }
                isyncyou_agent::AgentError::Transport(code)
                    if code == "provider_connect_timed_out" =>
                {
                    "model_catalog_timeout"
                }
                _ => "model_catalog_unavailable",
            })?;
        if response.status != 200 {
            return Err(match response.status {
                401 => "model_catalog_auth_unavailable",
                403 => "model_catalog_access_unavailable",
                429 => "model_catalog_rate_limited",
                _ => "model_catalog_unavailable",
            }
            .into());
        }
        if response.redirected
            || !response
                .content_type
                .to_ascii_lowercase()
                .starts_with("application/json")
        {
            return Err("model_catalog_unavailable".into());
        }
        let models = parse(provider, &response.body)?;
        let catalog = Catalog {
            version: 1,
            generation: meta.generation,
            fetched_at_ms: (self.credential_now_ms)(),
            client_version: catalog_client_version(provider),
            models,
        };
        let raw = serde_json::to_vec(&catalog).map_err(|_| "model_catalog_invalid")?;
        if raw.len() > MAX_SAVED_BYTES {
            return Err("model_catalog_size_limit".into());
        }
        // Serialize catalog publication with model selection; the provider lease keeps
        // lifecycle authority stable, while no runtime/file lock spans network I/O.
        let _runtime = self
            .product_runtime_gate
            .lock()
            .map_err(|_| "model_catalog_busy")?;
        let _file = acquire_product_runtime_file_lock(&self.oauth_dir)?;
        agent_credential_store(&self.oauth_dir)?
            .put(
                isyncyou_agent::SecretClass::ProductSettings,
                &store_id(provider),
                &isyncyou_agent::Secret::new(raw),
            )
            .map_err(|_| "model_catalog_unavailable")?;
        Ok(public(&catalog, "ready").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_catalog_accepts_new_models_and_discards_provider_harness() {
        let raw = serde_json::to_vec(&serde_json::json!({"models": [{
            "slug": "gpt-6.1-sol", "display_name": "GPT-6.1 Sol", "visibility": "list",
            "supported_reasoning_levels": [{"effort":"low"},{"effort":"ultra"}], "default_reasoning_level": "ultra", "use_responses_lite": true,
            "base_instructions": "private-default-harness", "model_messages": {"instructions_template":"do not install"}
        }, {"slug":"gpt-hidden", "visibility":"hide"}]})).unwrap();
        let models = parse(ProductProviderId::Codex, &raw).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].reasoning_efforts, ["low", "ultra"]);
        assert_eq!(models[0].use_responses_lite, Some(true));
        let saved = serde_json::to_string(&models).unwrap();
        assert!(!saved.contains("private-default-harness"));
        assert!(!saved.contains("instructions_template"));
    }

    #[test]
    fn model_catalog_claude_is_complete_and_bounded() {
        let raw = br#"{"has_more":false,"data":[{"id":"claude-fable-5-1","display_name":"Fable 5.1"},{"id":"claude-opus-5-5","display_name":"Opus 5.5"}]}"#;
        assert_eq!(parse(ProductProviderId::Claude, raw).unwrap().len(), 2);
        assert!(parse(ProductProviderId::Claude, br#"{"has_more":true,"data":[]}"#).is_err());
        let mut models = parse(ProductProviderId::Claude, raw).unwrap();
        models[0].label = "x".repeat(129);
        assert!(!valid_models(&models, ProductProviderId::Claude));
        assert!(!valid_id("gpt-6?token=private", "gpt-"));
    }

    #[test]
    fn model_catalog_rejects_duplicate_ids_and_oversized_response() {
        let entry = serde_json::json!({"id":"claude-sonnet-5-5","display_name":"Sonnet 5.5"});
        let raw =
            serde_json::to_vec(&serde_json::json!({"has_more":false,"data":[entry.clone(),entry]}))
                .unwrap();
        assert!(parse(ProductProviderId::Claude, &raw).is_err());
        assert!(parse(
            ProductProviderId::Claude,
            &vec![b' '; MAX_CATALOG_WIRE_BYTES + 1]
        )
        .is_err());
    }
}
