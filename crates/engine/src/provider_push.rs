//! Native-only JMAP push registration operations. Never log request or response values.

use isyncyou_connectors::jmap::{JmapError, PushMutation, PushOutcome};
use isyncyou_core::Config;
use serde_json::{json, Value};

fn mutation(value: PushMutation) -> Value {
    match value.outcome {
        PushOutcome::Applied { id, expires } => {
            json!({"status":"applied","id":id,"expires":expires,"session_state":value.session_state})
        }
        PushOutcome::Rejected { kind } => json!({"status":"rejected","kind":kind}),
    }
}

/// Native requests are already tied to one configured account and protected state.
/// Any ambiguous mutation outcome remains unknown and is not retried here.
pub fn perform(
    cfg: &Config,
    account: &str,
    operation: &str,
    request: &Value,
) -> Result<Value, &'static str> {
    let provider =
        crate::self_hosted_mail::client(cfg, account).map_err(|_| "provider_unavailable")?;
    crate::self_hosted_mail::scope(cfg, account, &provider).map_err(|_| "account_mismatch")?;
    let client = &provider.client;
    let get = |name: &str| {
        request
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 2048)
            .ok_or("invalid_request")
    };
    let result = match operation {
        "metadata" => {
            return Ok(
                json!({"vapid":client.vapid_public_key(),"session_state":client.session_state()}),
            )
        }
        "get" => {
            let id = request
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 512);
            if request.get("id").is_some() && id.is_none() {
                return Err("invalid_request");
            }
            let ids = id.map(|value| vec![value.to_owned()]);
            let result = client
                .push_get(ids.as_deref())
                .map_err(|_| "provider_unavailable")?;
            if serde_json::to_vec(&result)
                .map_err(|_| "provider_unavailable")?
                .len()
                > 65536
            {
                return Err("provider_unavailable");
            }
            return Ok(result);
        }
        "create" => client.push_create(
            get("creation_id")?,
            get("device_client_id")?,
            get("endpoint")?,
            get("p256dh")?,
            get("auth")?,
            None,
            None,
        ),
        "verify" => client.push_verify(get("id")?, get("code")?),
        "renew" => client.push_renew(get("id")?, get("expires")?),
        "destroy" => client.push_destroy(get("id")?),
        _ => return Err("invalid_request"),
    };
    let result = result.map_err(|error| match error {
        JmapError::OutcomeUnknown => "outcome_unknown",
        _ => "provider_unavailable",
    })?;
    let outcome = mutation(result);
    if operation == "create" && outcome["status"] == "applied" {
        let refreshed =
            crate::self_hosted_mail::client(cfg, account).map_err(|_| "outcome_unknown")?;
        crate::self_hosted_mail::scope(cfg, account, &refreshed).map_err(|_| "outcome_unknown")?;
        if outcome["session_state"].as_str() != Some(client.session_state())
            || refreshed.client.session_state() != client.session_state()
            || refreshed.client.vapid_public_key() != client.vapid_public_key()
        {
            return Err("session_changed");
        }
    }
    Ok(outcome)
}
