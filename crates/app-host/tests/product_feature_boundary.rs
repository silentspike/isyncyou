use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_repo(path: &str) -> String {
    fs::read_to_string(repo_root().join(path))
        .unwrap_or_else(|error| panic!("could not read tracked product source {path}: {error}"))
}

fn production_source_before_final_test_module(source: &str) -> &str {
    let marker = "\n#[cfg(test)]\nmod tests {";
    let final_tests = source
        .rfind(marker)
        .expect("app-host must keep one final cfg(test) module");
    &source[..final_tests]
}

#[test]
fn readme_does_not_advertise_subscription_experimental() {
    for path in [
        "README.md",
        "docs/packaging-daemon-model.md",
        "docs/android-distribution.md",
        "android/README.md",
    ] {
        let source = read_repo(path);
        for forbidden in [
            ["agent-subscription", "-experimental"].concat(),
            [".claude/", ".credentials.json"].concat(),
            [".codex/", "auth.json"].concat(),
        ] {
            assert!(
                !source.contains(&forbidden),
                "product documentation advertises local client auth in {path}"
            );
        }
    }
}

#[test]
fn product_sources_do_not_reference_local_cli_auth_paths() {
    let app_host = read_repo("crates/app-host/src/lib.rs");
    let app_host_product = production_source_before_final_test_module(&app_host);
    let product_sources = [
        ("crates/app-host/src/lib.rs", app_host_product.to_string()),
        (
            "crates/mobile/src/lib.rs",
            read_repo("crates/mobile/src/lib.rs"),
        ),
        (
            "bin/daemon/src/main.rs",
            read_repo("bin/daemon/src/main.rs"),
        ),
        ("gui/webui/src/app.js", read_repo("gui/webui/src/app.js")),
        (
            "android MainActivity",
            read_repo("android/app/src/main/kotlin/com/silentspike/isyncyou/MainActivity.kt"),
        ),
    ];
    let forbidden = [
        [".claude/", ".credentials.json"].concat(),
        [".codex/", "auth.json"].concat(),
        ["CLAUDE_", "CONFIG_DIR"].concat(),
        ["CODEX_", "HOME"].concat(),
        ["subscription", "/import"].concat(),
    ];

    for (path, source) in product_sources {
        for needle in &forbidden {
            assert!(
                !source.contains(needle),
                "product source contains local credential/import marker {needle}: {path}"
            );
        }
    }
}

#[test]
fn product_source_scan_keeps_production_after_early_cfg_test_items() {
    let source = r#"
#[cfg(test)]
fn early_test_helper() {}

fn production_after_helper() {
    let _must_be_scanned = ".claude/.credentials.json";
}

#[cfg(test)]
mod tests {
    const TEST_ONLY: &str = ".codex/auth.json";
}
"#;

    let production = production_source_before_final_test_module(source);

    assert!(production.contains(".claude/.credentials.json"));
    assert!(!production.contains(".codex/auth.json"));
}
