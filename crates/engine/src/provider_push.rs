//! Native-only JMAP push registration operations. Never log request or response values.

use isyncyou_connectors::jmap::{JmapClient, JmapError, PushMutation, PushOutcome};
use isyncyou_core::{config::ProviderConfig, Config};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.as_str(),
        None => return Err("account_mismatch"),
    };
    let result = crate::provider_auth::with_platform_credential_store(cfg, account, |store| {
        crate::provider_auth::with_locked_provider_identity(
            cfg,
            account,
            store,
            |token, subject| {
                perform_locked(origin, token, subject, operation, request).map_err(str::to_owned)
            },
        )
    });
    result.map_err(|error| match error.as_str() {
        "invalid_request" => "invalid_request",
        "account_mismatch" => "account_mismatch",
        "outcome_unknown" => "outcome_unknown",
        "session_changed" => "session_changed",
        _ => "provider_unavailable",
    })
}

fn bindings(origin: &str, subject: &str, primary: &str) -> (String, String) {
    let mut account = Sha256::new();
    account.update(b"isyncyou-push-account-v1\0");
    account.update(primary.as_bytes());
    let mut identity = Sha256::new();
    identity.update(b"isyncyou-push-identity-v1\0");
    for value in [origin, subject, primary] {
        identity.update((value.len() as u32).to_be_bytes());
        identity.update(value.as_bytes());
    }
    (
        format!("{:x}", account.finalize()),
        format!("{:x}", identity.finalize()),
    )
}

fn perform_locked(
    origin: &str,
    token: &str,
    subject: &str,
    operation: &str,
    request: &Value,
) -> Result<Value, &'static str> {
    let client =
        JmapClient::discover(origin, token.to_owned()).map_err(|_| "provider_unavailable")?;
    // with_locked_provider_identity resolves the same configured local account
    // and origin as self_hosted_mail::scope, verifies its subject-bound token,
    // and holds that account lock. Discovery must select the primary account.
    if client.account_id() != client.primary_account_id() {
        return Err("account_mismatch");
    }
    perform_client(origin, token, subject, operation, request, &client)
}

fn perform_client(
    origin: &str,
    token: &str,
    subject: &str,
    operation: &str,
    request: &Value,
    client: &JmapClient,
) -> Result<Value, &'static str> {
    let (account_binding, identity_binding) =
        bindings(origin, subject, client.primary_account_id());
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
                json!({"vapid":client.vapid_public_key(),"session_state":client.session_state(),"account_binding":account_binding,"identity_binding":identity_binding}),
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
        "create" => {
            let expected_vapid = match request.get("expected_vapid") {
                Some(Value::Null) => None,
                Some(Value::String(value)) if !value.is_empty() => Some(value.as_str()),
                _ => return Err("invalid_request"),
            };
            if get("identity_binding")? != identity_binding
                || get("expected_session_state")? != client.session_state()
                || expected_vapid != client.vapid_public_key()
            {
                return Err("session_changed");
            }
            client.push_create(
                get("creation_id")?,
                get("device_client_id")?,
                get("endpoint")?,
                get("p256dh")?,
                get("auth")?,
                None,
                None,
            )
        }
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
            JmapClient::discover(origin, token.to_owned()).map_err(|_| "outcome_unknown")?;
        if outcome["session_state"].as_str() != Some(client.session_state())
            || refreshed.session_state() != client.session_state()
            || refreshed.vapid_public_key() != client.vapid_public_key()
            || refreshed.primary_account_id() != client.primary_account_id()
        {
            return Err("session_changed");
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_binding_rejects_identity_session_and_vapid_changes_before_network() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let client = JmapClient::test_loopback(&origin).unwrap();
        let (account, identity) = bindings(&origin, "subject", "account");
        assert_eq!(
            account,
            "6ddd2bb858518bfb5dec1be2f4f8ca932e7d6aadbd17cbcb63bd48bd03c2922c"
        );
        assert_ne!(identity, bindings(&origin, "another-subject", "account").1);
        assert_ne!(identity, bindings(&origin, "subject", "other-account").1);
        assert_ne!(
            identity,
            bindings("https://other.example/", "subject", "account").1
        );
        let base = json!({"identity_binding":identity,"expected_session_state":"state","expected_vapid":null,
            "creation_id":"create","device_client_id":"installation","endpoint":"https://example.test/topic","p256dh":"key","auth":"auth"});
        for (key, value) in [
            ("identity_binding", json!("different")),
            ("expected_session_state", json!("different")),
            ("expected_vapid", json!("different")),
        ] {
            let mut request = base.clone();
            request[key] = value;
            assert_eq!(
                perform_client(&origin, "fixture", "subject", "create", &request, &client),
                Err("session_changed")
            );
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        let mut missing = base;
        missing.as_object_mut().unwrap().remove("expected_vapid");
        assert_eq!(
            perform_client(&origin, "fixture", "subject", "create", &missing, &client),
            Err("invalid_request")
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
