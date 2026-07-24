use std::path::Path;

fn assert_tree_omits(path: &Path, forbidden: &[String]) {
    for entry in std::fs::read_dir(path).expect("test/help directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            assert_tree_omits(&path, forbidden);
            continue;
        }
        let is_text = matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("rs" | "md" | "txt")
        );
        if !is_text {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("UTF-8 test/help source");
        for value in forbidden {
            assert!(
                !source.contains(value),
                "experimental test/help source documents a raw credential export"
            );
        }
    }
}

#[test]
fn experimental_tests_do_not_document_raw_token_export() {
    let forbidden = [
        ["ISY_LIVE_", "TOKEN"].concat(),
        ["ISY_CODEX_", "TOKEN"].concat(),
        ["ISY_CODEX_", "ACCOUNT"].concat(),
    ];
    assert_tree_omits(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &forbidden,
    );
}
