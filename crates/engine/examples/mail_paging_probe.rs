//! Bounded, read-only live pagination probe using a disposable in-memory store.
use isyncyou_core::Config;
use isyncyou_engine::self_hosted_mail;
use isyncyou_store::Store;
use std::path::PathBuf;

fn main() {
    if run().is_err() {
        eprintln!("mail_paging_probe_failed");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 { return Err("expected config path, account ID and synthetic state".into()); }
    let cfg = Config::load(&PathBuf::from(&args[1]))?;
    cfg.validate().map_err(|_| "invalid configuration")?;
    let store = Store::open_in_memory()?;
    let client = self_hosted_mail::client(&cfg, &args[2])?;
    let before = self_hosted_mail::sync_metrics(&client);
    let changes = self_hosted_mail::sync_with_page_limit(&cfg, &args[2], &store, &client, 1)?;
    let initial = self_hosted_mail::sync_metrics(&client);
    let next = self_hosted_mail::sync_with_page_limit(&cfg, &args[2], &store, &client, 1)?;
    let final_counts = self_hosted_mail::sync_metrics(&client);
    let scope = self_hosted_mail::scope(&cfg, &args[2], &client)?;
    let before_ids: Vec<String> = store.provider_objects_page(&scope, "", 200)?.into_iter().map(|v| v.remote_id).collect();
    if before_ids.is_empty() || before_ids.len() == 200 { return Err("fixture size outside probe bounds".into()); }
    store.apply_provider_page(&scope, &[], &args[3])?;
    let recovered = self_hosted_mail::sync_with_page_limit(&cfg, &args[2], &store, &client, 1)?;
    let recovered_counts = self_hosted_mail::sync_metrics(&client);
    let after_ids: Vec<String> = store.provider_objects_page(&scope, "", 200)?.into_iter().map(|v| v.remote_id).collect();
    if before_ids != after_ids || recovered_counts.email_reconciliations.saturating_sub(final_counts.email_reconciliations) != 1 {
        return Err("server did not reject state and reconcile exact identities".into());
    }
    println!("initial_changes={changes} initial_query_pages={} initial_body_fetch={} initial_blob_download={} unchanged_changes={next} unchanged_email_get={} unchanged_body_fetch={} unchanged_blob_download={}",
        initial.email_query.saturating_sub(before.email_query), initial.body_fetch.saturating_sub(before.body_fetch), initial.blob_download.saturating_sub(before.blob_download),
        final_counts.email_get.saturating_sub(initial.email_get), final_counts.body_fetch.saturating_sub(initial.body_fetch), final_counts.blob_download.saturating_sub(initial.blob_download));
    println!("reconciliation=1 identities_preserved={} recovered_changes={recovered} query_pages={} email_get={} body_fetch={} blob_download={}", after_ids.len(), recovered_counts.email_query.saturating_sub(final_counts.email_query), recovered_counts.email_get.saturating_sub(final_counts.email_get), recovered_counts.body_fetch.saturating_sub(final_counts.body_fetch), recovered_counts.blob_download.saturating_sub(final_counts.blob_download));
    Ok(())
}
