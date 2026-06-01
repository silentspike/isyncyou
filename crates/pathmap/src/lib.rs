//! `isyncyou-pathmap` — cloud <-> local filename mapping for bidirectional OneDrive sync.
//!
//! OneDrive (case-insensitive, forbids `" * : < > ? / \ |`, reserved names,
//! trailing dots/spaces) and Linux (case-sensitive, permissive) disagree about
//! valid file names. Syncing both ways without a sanitizing, **reversible** layer
//! risks data loss (name collisions, lost characters).
//!
//! This crate provides:
//! - a reversible character/name **codec** ([`to_cloud`] / [`to_local`]): forbidden
//!   characters map to fullwidth look-alikes, trailing dots/spaces to visible
//!   markers. Designed so `to_local(to_cloud(name)) == name`.
//! - reserved-name detection ([`is_reserved`]) for Windows-client compatibility.
//! - a persistent [`MappingTable`] that is the **authoritative** roundtrip
//!   guarantee: it resolves case-only collisions (Linux allows `Foo`+`foo`,
//!   OneDrive does not) by suffixing, and remembers every cloud<->local pair so
//!   the mapping survives codec edge cases.
//!
//! The codec is a bijection over names that do not already contain the fullwidth
//! replacement characters (which essentially never occur in real file names); the
//! [`MappingTable`] is the backstop for those rare cases and for case collisions.
//!
//! TODO: Unicode NFC normalization of comparison keys (HFS+/APFS interop).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// (ASCII character forbidden by OneDrive/Windows, fullwidth look-alike).
const CHAR_MAP: &[(char, char)] = &[
    ('"', '\u{FF02}'),  // ＂
    ('*', '\u{FF0A}'),  // ＊
    (':', '\u{FF1A}'),  // ：
    ('<', '\u{FF1C}'),  // ＜
    ('>', '\u{FF1E}'),  // ＞
    ('?', '\u{FF1F}'),  // ？
    ('\\', '\u{FF3C}'), // ＼
    ('|', '\u{FF5C}'),  // ｜
    ('/', '\u{FF0F}'),  // ／ (cannot occur inside a single path component, kept for completeness)
];

/// Trailing `.` -> fullwidth full stop (OneDrive strips trailing dots).
const TRAIL_DOT: char = '\u{FF0E}'; // ．
/// Trailing ` ` -> open-box marker (OneDrive strips trailing spaces).
const TRAIL_SPACE: char = '\u{2423}'; // ␣

fn enc_char(c: char) -> char {
    CHAR_MAP
        .iter()
        .find(|(a, _)| *a == c)
        .map(|(_, b)| *b)
        .unwrap_or(c)
}

fn dec_char(c: char) -> char {
    CHAR_MAP
        .iter()
        .find(|(_, b)| *b == c)
        .map(|(a, _)| *a)
        .unwrap_or(c)
}

/// Encode a single **local** path component into a OneDrive-safe **cloud** name.
///
/// Reversible via [`to_local`] for any input that does not already contain the
/// fullwidth replacement characters.
pub fn to_cloud(local: &str) -> String {
    // 1. map forbidden characters anywhere in the name.
    let mapped: Vec<char> = local.chars().map(enc_char).collect();
    // 2. encode the trailing run of '.'/' ' (only the trailing run matters).
    let mut out: Vec<char> = mapped.clone();
    let mut i = out.len();
    while i > 0 && (out[i - 1] == '.' || out[i - 1] == ' ') {
        i -= 1;
    }
    for c in out.iter_mut().skip(i) {
        *c = match *c {
            '.' => TRAIL_DOT,
            ' ' => TRAIL_SPACE,
            other => other,
        };
    }
    out.into_iter().collect()
}

/// Decode a **cloud** name back into its original **local** path component.
pub fn to_local(cloud: &str) -> String {
    cloud
        .chars()
        .map(|c| match c {
            TRAIL_DOT => '.',
            TRAIL_SPACE => ' ',
            other => dec_char(other),
        })
        .collect()
}

/// Windows reserved device names + OneDrive-special prefixes. Detection only;
/// encoding reserved names is deferred to the Windows client (Phase 3).
pub fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0');
    device
        || name.starts_with("~$")
        || name.starts_with("_vti_")
        || name.eq_ignore_ascii_case("desktop.ini")
}

/// Case-insensitive comparison key used to detect OneDrive case collisions.
pub fn case_key(name: &str) -> String {
    name.to_lowercase()
}

/// Split `name` into `(stem, extension_with_dot)` for suffix insertion.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        // keep dotfiles (".bashrc") and trailing-dot names intact
        Some(idx) if idx > 0 && idx < name.len() - 1 => (&name[..idx], &name[idx..]),
        _ => (name, ""),
    }
}

/// One folder's bidirectional name mapping. JSON-friendly (string keys only).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ParentMap {
    cloud_to_local: HashMap<String, String>, // cloud_name -> local_name
    local_to_cloud: HashMap<String, String>, // local_name -> cloud_name
    used_cloud: HashMap<String, String>,     // case_key(cloud_name) -> cloud_name
}

impl ParentMap {
    fn dedupe(&self, candidate: String) -> String {
        if !self.used_cloud.contains_key(&case_key(&candidate)) {
            return candidate;
        }
        let (stem, ext) = split_ext(&candidate);
        for n in 2.. {
            let alt = format!("{stem} ({n}){ext}");
            if !self.used_cloud.contains_key(&case_key(&alt)) {
                return alt;
            }
        }
        unreachable!("the integer range is unbounded")
    }

    fn record(&mut self, cloud: &str, local: &str) {
        self.cloud_to_local
            .insert(cloud.to_string(), local.to_string());
        self.local_to_cloud
            .insert(local.to_string(), cloud.to_string());
        self.used_cloud.insert(case_key(cloud), cloud.to_string());
    }
}

/// Persistent, reversible cloud<->local name mapping, scoped per parent folder.
///
/// This is the authoritative source of truth for the namespace: it guarantees a
/// stable roundtrip even when the codec alone cannot (case collisions, names that
/// already contain replacement characters).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MappingTable {
    parents: HashMap<String, ParentMap>,
}

impl MappingTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assign (or look up) the cloud name for a local file in `parent`.
    ///
    /// Encodes via [`to_cloud`], then resolves case-insensitive collisions with
    /// already-assigned siblings by inserting ` (n)` before the extension.
    pub fn assign_cloud_name(&mut self, parent: &str, local: &str) -> String {
        let p = self.parents.entry(parent.to_string()).or_default();
        if let Some(c) = p.local_to_cloud.get(local) {
            return c.clone();
        }
        let cloud = p.dedupe(to_cloud(local));
        p.record(&cloud, local);
        cloud
    }

    /// Assign (or look up) the local name for a cloud item in `parent`.
    ///
    /// Decodes via [`to_local`]; siblings differing only by case are fine on Linux,
    /// so no deduping is needed in this direction.
    pub fn assign_local_name(&mut self, parent: &str, cloud: &str) -> String {
        let p = self.parents.entry(parent.to_string()).or_default();
        if let Some(l) = p.cloud_to_local.get(cloud) {
            return l.clone();
        }
        let local = to_local(cloud);
        p.record(cloud, &local);
        local
    }

    /// Reverse lookups (return `None` if unknown).
    pub fn lookup_local(&self, parent: &str, cloud: &str) -> Option<&str> {
        self.parents
            .get(parent)?
            .cloud_to_local
            .get(cloud)
            .map(|s| s.as_str())
    }
    pub fn lookup_cloud(&self, parent: &str, local: &str) -> Option<&str> {
        self.parents
            .get(parent)?
            .local_to_cloud
            .get(local)
            .map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_chars_roundtrip() {
        let local = r#"report:2024 <draft>?*"x"|y\z/w.txt"#;
        let cloud = to_cloud(local);
        assert!(!cloud.contains(':'));
        assert!(!cloud.contains('*'));
        assert!(!cloud.contains('?'));
        assert_eq!(to_local(&cloud), local);
    }

    #[test]
    fn trailing_dot_and_space_encoded_and_restored() {
        for name in ["folder.", "name ", "weird. .", "a.b.c. ."] {
            let cloud = to_cloud(name);
            assert!(!cloud.ends_with('.'), "{cloud:?} still ends with dot");
            assert!(!cloud.ends_with(' '), "{cloud:?} still ends with space");
            assert_eq!(to_local(&cloud), name);
        }
    }

    #[test]
    fn interior_dots_and_spaces_untouched() {
        let name = "my file.tar.gz";
        assert_eq!(to_cloud(name), name);
        assert_eq!(to_local(name), name);
    }

    #[test]
    fn reserved_names_detected() {
        for r in [
            "CON",
            "con",
            "PRN.txt",
            "NUL",
            "COM1",
            "lpt9.log",
            "~$doc.docx",
            "desktop.ini",
        ] {
            assert!(is_reserved(r), "{r} should be reserved");
        }
        for ok in ["CONsole", "COM0", "COM10", "report.txt", "lptx", "com"] {
            assert!(!is_reserved(ok), "{ok} should NOT be reserved");
        }
    }

    #[test]
    fn case_collision_is_deduped_and_reversible() {
        let mut t = MappingTable::new();
        let a = t.assign_cloud_name("p", "Foo.txt");
        let b = t.assign_cloud_name("p", "foo.txt");
        let c = t.assign_cloud_name("p", "FOO.txt");
        assert_eq!(a, "Foo.txt");
        assert_ne!(case_key(&b), case_key(&a)); // forced unique on the cloud side
        assert_ne!(case_key(&c), case_key(&a));
        assert_ne!(case_key(&c), case_key(&b));
        // every local name maps back to a distinct, recoverable cloud name
        assert_eq!(t.lookup_local("p", &a), Some("Foo.txt"));
        assert_eq!(t.lookup_local("p", &b), Some("foo.txt"));
        assert_eq!(t.lookup_local("p", &c), Some("FOO.txt"));
    }

    #[test]
    fn dedupe_inserts_before_extension() {
        let mut t = MappingTable::new();
        assert_eq!(t.assign_cloud_name("p", "a.txt"), "a.txt");
        // the colliding sibling keeps its original casing, deduped with a suffix
        assert_eq!(t.assign_cloud_name("p", "A.txt"), "A (2).txt");
    }

    #[test]
    fn same_local_name_is_idempotent() {
        let mut t = MappingTable::new();
        let first = t.assign_cloud_name("p", "doc:1.txt");
        let again = t.assign_cloud_name("p", "doc:1.txt");
        assert_eq!(first, again);
    }

    #[test]
    fn different_parents_do_not_collide() {
        let mut t = MappingTable::new();
        assert_eq!(
            t.assign_cloud_name("p1", "x:y"),
            t.assign_cloud_name("p2", "x:y")
        );
    }

    #[test]
    fn table_serde_roundtrips() {
        let mut t = MappingTable::new();
        t.assign_cloud_name("p", "Foo:bar.txt");
        t.assign_cloud_name("p", "foo:bar.txt");
        let json = serde_json::to_string(&t).unwrap();
        let back: MappingTable = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lookup_local("p", "Foo：bar.txt"), Some("Foo:bar.txt"));
    }

    proptest::proptest! {
        // The codec is a bijection over names without the fullwidth replacement
        // chars. Generate realistic names (ASCII incl. forbidden, plus a few
        // common Unicode letters), excluding the replacement set.
        #[test]
        fn prop_codec_roundtrip(name in r#"[a-zA-Z0-9 ._:*<>?\\|"äéü😀-]{1,40}"#) {
            let cloud = to_cloud(&name);
            proptest::prop_assert_eq!(to_local(&cloud), name);
        }
    }
}
