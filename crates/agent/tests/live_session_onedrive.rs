#![cfg(feature = "onedrive")]

use isyncyou_agent::{
    new_ulid, DeviceId, FileSessionCache, OneDriveTransport, Session, SessionCryptoConfig,
    SessionId, SessionTransport,
};
use isyncyou_graph::http::{GraphClient, UploadError};

const ROOT: &str = "Apps/iSyncYou/agent";
const SENTINEL: &str = "LIVE-CIPHERTEXT-SENTINEL-619";

struct Cleanup {
    client: GraphClient,
    path: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(Some(item)) = self.client.get_drive_item_by_path(&self.path, &["id"]) else {
            return;
        };
        if let Some(id) = item.get("id").and_then(|id| id.as_str()) {
            let _ = self.client.delete_item(id);
        }
    }
}

fn token_from_env() -> String {
    std::env::var("ISY_GRAPH_TOKEN")
        .or_else(|_| std::env::var("ISYNCYOU_TEST_WRITE_TOKEN"))
        .expect("set ISY_GRAPH_TOKEN or ISYNCYOU_TEST_WRITE_TOKEN with Files.ReadWrite")
}

fn item_id_at_path(client: &GraphClient, path: &str) -> Result<Option<String>, String> {
    let item = client
        .get_drive_item_by_path(path, &["id", "name"])
        .map_err(|e| e.to_string())?;
    Ok(item.and_then(|v| v.get("id").and_then(|id| id.as_str()).map(String::from)))
}

fn ensure_folder(
    client: &GraphClient,
    parent_id: &str,
    path: &str,
    name: &str,
) -> Result<String, String> {
    if let Some(id) = item_id_at_path(client, path)? {
        return Ok(id);
    }
    match client.create_folder(parent_id, name) {
        Ok(item) => item
            .get("id")
            .and_then(|id| id.as_str())
            .map(String::from)
            .ok_or_else(|| format!("created folder {path} had no id")),
        Err(UploadError::Http { status: 409, .. }) => item_id_at_path(client, path)?
            .ok_or_else(|| format!("folder {path} conflicted but could not be resolved")),
        Err(err) => Err(err.to_string()),
    }
}

fn ensure_agent_root(client: &GraphClient) -> Result<(), String> {
    let apps = ensure_folder(client, "", "Apps", "Apps")?;
    let isyncyou = ensure_folder(client, &apps, "Apps/iSyncYou", "iSyncYou")?;
    ensure_folder(client, &isyncyou, ROOT, "agent")?;
    Ok(())
}

#[test]
#[ignore = "requires live OneDrive Files.ReadWrite token in ISY_GRAPH_TOKEN"]
fn live_onedrive_session_roundtrip_ciphertext_and_cleanup() {
    let token = token_from_env();
    let graph = GraphClient::new(token.clone());
    ensure_agent_root(&graph).expect("agent root exists");

    let session_id = format!("ag619-{}", new_ulid().expect("ulid"));
    let session_path = format!("{ROOT}/{session_id}");
    let agent_root = item_id_at_path(&graph, ROOT)
        .expect("agent root lookup")
        .expect("agent root id");
    ensure_folder(&graph, &agent_root, &session_path, &session_id).expect("session folder");
    let _cleanup = Cleanup {
        client: GraphClient::new(token.clone()),
        path: session_path.clone(),
    };

    let config = SessionCryptoConfig::generate_default().expect("config");
    let pairing_secret = vec![0x51; 32];
    let session_a = Session::new_with_crypto_config(
        &session_id,
        pairing_secret.clone(),
        OneDriveTransport::new(token.clone()),
        config.clone(),
    )
    .expect("session A");
    let session_b = Session::new_with_crypto_config(
        &session_id,
        pairing_secret.clone(),
        OneDriveTransport::new(token.clone()),
        config.clone(),
    )
    .expect("session B");
    let device_a = DeviceId::new("pixel-a").expect("device A");
    let device_b = DeviceId::new("pixel-b").expect("device B");

    let mut active = session_a
        .begin_active_turn(&device_a)
        .expect("lease request")
        .expect("device A acquires lease");
    assert!(
        session_b
            .begin_active_turn(&device_b)
            .expect("second lease request")
            .is_none(),
        "device B must be blocked while A holds the lease"
    );

    let first = active
        .append("user", SENTINEL)
        .expect("first encrypted turn");
    active
        .append("assistant", "redacted assistant reply")
        .expect("second encrypted turn");
    active
        .append("user", "redacted follow-up")
        .expect("third encrypted turn");
    active.finish().expect("release lease");

    let loaded = session_a.load_full().expect("load session");
    assert_eq!(loaded.turns.len(), 3);
    assert!(loaded.fork.is_none(), "live linear session must not fork");
    assert_eq!(loaded.turns[0].content, SENTINEL);

    let left_cache = tempfile::tempdir().expect("left offline cache");
    let right_cache = tempfile::tempdir().expect("right offline cache");
    let session_left = Session::new_with_cache(
        &session_id,
        pairing_secret.clone(),
        OneDriveTransport::new(token.clone()),
        config.clone(),
        FileSessionCache::new(left_cache.path()),
    )
    .expect("left offline session");
    let session_right = Session::new_with_cache(
        &session_id,
        pairing_secret.clone(),
        OneDriveTransport::new(token.clone()),
        config,
        FileSessionCache::new(right_cache.path()),
    )
    .expect("right offline session");
    session_left
        .load_full()
        .expect("left observes current head");
    session_right
        .load_full()
        .expect("right observes same current head");
    session_left
        .append_offline_pending(&device_a, "user", "offline-left")
        .expect("left offline turn");
    session_right
        .append_offline_pending(&device_b, "user", "offline-right")
        .expect("right offline turn");
    assert_eq!(session_left.sync().expect("sync left pending"), 1);
    assert_eq!(session_right.sync().expect("sync right pending"), 1);

    let forked = session_a.load_full().expect("load forked session");
    let fork = forked.fork.expect("live concurrent offline heads fork");
    assert_eq!(fork.heads.len(), 2, "fork must expose the two live heads");

    let transport = OneDriveTransport::new(token.clone());
    let listed = transport
        .list(&SessionId::new(&session_id).expect("session id"))
        .expect("list turn ids");
    assert_eq!(listed.len(), 5, "only turn files should be listed");

    let raw_path = format!("{session_path}/{}.json", first.ulid);
    let raw = graph
        .get_bytes(&format!("/me/drive/root:/{raw_path}:/content"))
        .expect("download raw encrypted turn");
    let raw_text = String::from_utf8_lossy(&raw);
    assert!(
        raw_text.contains("\"ct\""),
        "raw turn should be an envelope"
    );
    assert!(
        !raw_text.contains(SENTINEL),
        "raw OneDrive turn must not contain plaintext sentinel"
    );

    println!(
        "{}",
        serde_json::json!({
            "session_id": session_id,
            "turn_count": listed.len(),
            "linear_heads": loaded.heads.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "fork_heads": fork.heads.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "ciphertext_only": true,
            "lease_blocked_second_holder": true,
            "fork_reported": true,
            "cleanup_path": session_path
        })
    );
}
