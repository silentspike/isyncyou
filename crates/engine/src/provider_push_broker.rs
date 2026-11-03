//! Bounded authenticated transport for a configured, same-origin push broker.
//! Payloads and native responses can contain capability values and must never be logged.

use isyncyou_core::{config::ProviderConfig, Config};
use serde_json::Value;
use std::io::Read;
use std::time::Duration;

/// POST one fixed operation to an explicitly configured broker base URL.
/// An ambiguous mutation result is returned to the caller without retrying it.
pub fn post(
    cfg: &Config,
    account: &str,
    operation: &str,
    request: &Value,
) -> Result<Value, &'static str> {
    post_inner(cfg, account, operation, request, None)
}

/// Engine-internal transport for a token already protected by the account lock.
pub(crate) fn post_with_token(cfg: &Config, account: &str, operation: &str, request: &Value, token: &str) -> Result<Value, &'static str> {
    if token.is_empty() || token.chars().any(char::is_control) { return Err("auth_unavailable"); }
    post_inner(cfg, account, operation, request, Some(token))
}

fn post_inner(cfg: &Config, account: &str, operation: &str, request: &Value, supplied_token: Option<&str>) -> Result<Value, &'static str> {
    if !matches!(
        operation,
        "challenge" | "prepare" | "deliver" | "confirm" | "revoke"
    ) || !request.is_object()
    {
        return Err("invalid_request");
    }
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin,
        None => return Err("invalid_account"),
    };
    let origin = reqwest::Url::parse(origin).map_err(|_| "invalid_origin")?;
    let base = cfg
        .push_broker_bases
        .get(account)
        .ok_or("broker_not_configured")?;
    let base = reqwest::Url::parse(base).map_err(|_| "invalid_broker")?;
    if origin.scheme() != "https"
        || base.scheme() != "https"
        || origin.origin() != base.origin()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.path().ends_with('/')
    {
        return Err("invalid_broker");
    }
    let target = base.join(operation).map_err(|_| "invalid_broker")?;
    let body = serde_json::to_vec(request).map_err(|_| "invalid_request")?;
    if body.len() > 8192 {
        return Err("invalid_request");
    }
    let owned_token;
    let token = match supplied_token {
        Some(token) => token,
        None => {
            owned_token = crate::provider_auth::self_hosted_access_token(cfg, account).map_err(|_| "auth_unavailable")?;
            &owned_token
        }
    };
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "transport_unavailable")?;
    let response = client
        .post(target)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(|_| "outcome_unknown")?;
    if response.status().is_server_error() {
        return Err("outcome_unknown");
    }
    if !response.status().is_success() {
        return Err("rejected");
    }
    let mut bytes = Vec::new();
    response
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "outcome_unknown")?;
    if bytes.len() > 65536 {
        return Err("outcome_unknown");
    }
    let result: Value = serde_json::from_slice(&bytes).map_err(|_| "outcome_unknown")?;
    if !result.is_object() {
        return Err("outcome_unknown");
    }
    Ok(result)
}
