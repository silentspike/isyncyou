//! Live, manually-run verification of the experimental subscription provider.
//! Never runs in CI (`#[ignore]` + needs a real token via env). Run locally:
//!   ISY_LIVE_TOKEN=$(…accessToken…) cargo +1.95.0 test -p isyncyou-agent \
//!     --features agent-subscription-experimental -- --ignored --nocapture live_subscription_turn
#![cfg(feature = "agent-subscription-experimental")]

use isyncyou_agent::{
    CodexConfig, CodexProvider, LlmProvider, Message, StreamEvent, SubscriptionConfig,
    SubscriptionProvider,
};

#[test]
#[ignore = "live: hits the real Claude subscription; set ISY_LIVE_TOKEN to run"]
fn live_subscription_turn() {
    let token = std::env::var("ISY_LIVE_TOKEN").expect("set ISY_LIVE_TOKEN");
    let model = std::env::var("ISY_LIVE_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());
    let mut p = SubscriptionProvider::new(
        token,
        model,
        "You are a terse assistant.",
        SubscriptionConfig::default(),
    )
    .expect("provider");

    let mut text = String::new();
    let blocks = p
        .next(
            &[Message::user(
                "Reply with exactly this sentence and nothing else: hello from the subscription",
            )],
            &mut |e| {
                if let StreamEvent::Token(t) = e {
                    text.push_str(&t);
                }
            },
        )
        .expect("subscription call failed");

    eprintln!("=== RESPONSE: {text}");
    eprintln!(
        "=== USAGE: in={} out={}",
        p.last_usage.input_tokens, p.last_usage.output_tokens
    );
    assert!(!blocks.is_empty(), "no response blocks");
}

#[test]
#[ignore = "live: hits the real ChatGPT/Codex subscription; set ISY_CODEX_TOKEN + ISY_CODEX_ACCOUNT"]
fn live_codex_turn() {
    let token = std::env::var("ISY_CODEX_TOKEN").expect("set ISY_CODEX_TOKEN");
    let account = std::env::var("ISY_CODEX_ACCOUNT").expect("set ISY_CODEX_ACCOUNT");
    let cfg = CodexConfig {
        account_id: account,
        ..Default::default()
    };
    let mut p = CodexProvider::new(token, "You are a terse assistant.", cfg).expect("provider");

    let mut text = String::new();
    let blocks = p
        .next(
            &[Message::user(
                "Reply with exactly this sentence and nothing else: hello from the codex provider",
            )],
            &mut |e| {
                if let StreamEvent::Token(t) = e {
                    text.push_str(&t);
                }
            },
        )
        .expect("codex call failed");

    eprintln!("=== CODEX RESPONSE: {text}");
    eprintln!(
        "=== USAGE: in={} out={}",
        p.last_usage.input_tokens, p.last_usage.output_tokens
    );
    assert!(!blocks.is_empty(), "no response blocks");
}

