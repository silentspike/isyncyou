//! Linux-only credential fallback for explicit personal/development builds.
//!
//! This module owns every read of locally installed Claude/Codex client state. It
//! extracts only the minimum credential bundle needed by the existing iSyncYou
//! providers; no client prompt, tool, rule, history, or other harness state crosses
//! this boundary.

use serde_json::Value;
use std::fmt;
use std::path::{Path, PathBuf};

const MAX_CREDENTIAL_JSON_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalCliFallbackError {
    #[cfg(not(target_os = "linux"))]
    UnsupportedPlatform,
    MissingEnvironment,
    InvalidRoot,
    MissingFile,
    UnsafeFileType,
    UnsafeOwner,
    UnsafePermissions,
    OversizedFile,
    UnreadableFile,
    InvalidJson,
    MissingCredential,
}

impl fmt::Display for LocalCliFallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            #[cfg(not(target_os = "linux"))]
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::MissingEnvironment => "missing_environment",
            Self::InvalidRoot => "invalid_root",
            Self::MissingFile => "missing_file",
            Self::UnsafeFileType => "unsafe_file_type",
            Self::UnsafeOwner => "unsafe_owner",
            Self::UnsafePermissions => "unsafe_permissions",
            Self::OversizedFile => "oversized_file",
            Self::UnreadableFile => "unreadable_file",
            Self::InvalidJson => "invalid_json",
            Self::MissingCredential => "missing_credential",
        };
        write!(f, "local CLI credential unavailable: {reason}")
    }
}

impl std::error::Error for LocalCliFallbackError {}

impl LocalCliFallbackError {
    pub(crate) fn is_absent(self) -> bool {
        matches!(self, Self::MissingEnvironment | Self::MissingFile)
    }

    pub(crate) fn is_unsupported_platform(self) -> bool {
        #[cfg(not(target_os = "linux"))]
        {
            self == Self::UnsupportedPlatform
        }
        #[cfg(target_os = "linux")]
        {
            let _ = self;
            false
        }
    }
}

pub(crate) struct LocalClaudeCredential {
    pub(crate) access_token: String,
}

pub(crate) struct LocalCodexCredential {
    pub(crate) access_token: String,
    pub(crate) account_id: String,
}

#[derive(Debug, Default)]
struct LocalCliEnvironment {
    home: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
    codex_home: Option<PathBuf>,
}

impl LocalCliEnvironment {
    fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            claude_config_dir: std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            codex_home: std::env::var_os("CODEX_HOME").map(PathBuf::from),
        }
    }

    fn claude_root(&self) -> Result<PathBuf, LocalCliFallbackError> {
        resolve_root(
            self.claude_config_dir.as_deref(),
            self.home.as_deref(),
            ".claude",
        )
    }

    fn codex_root(&self) -> Result<PathBuf, LocalCliFallbackError> {
        resolve_root(self.codex_home.as_deref(), self.home.as_deref(), ".codex")
    }
}

fn resolve_root(
    override_root: Option<&Path>,
    home: Option<&Path>,
    home_child: &str,
) -> Result<PathBuf, LocalCliFallbackError> {
    let root = match override_root {
        Some(root) => root.to_path_buf(),
        None => home
            .ok_or(LocalCliFallbackError::MissingEnvironment)?
            .join(home_child),
    };
    if root.as_os_str().is_empty() || !root.is_absolute() {
        return Err(LocalCliFallbackError::InvalidRoot);
    }
    Ok(root)
}

pub(crate) fn load_claude_from_process() -> Result<LocalClaudeCredential, LocalCliFallbackError> {
    load_claude(&LocalCliEnvironment::from_process())
}

pub(crate) fn load_codex_from_process() -> Result<LocalCodexCredential, LocalCliFallbackError> {
    load_codex(&LocalCliEnvironment::from_process())
}

fn load_claude(
    environment: &LocalCliEnvironment,
) -> Result<LocalClaudeCredential, LocalCliFallbackError> {
    ensure_linux()?;
    let value = read_bounded_json(&environment.claude_root()?.join(".credentials.json"))?;
    let access_token = nonempty_string(
        value
            .get("claudeAiOauth")
            .and_then(|oauth| oauth.get("accessToken")),
    )?;
    Ok(LocalClaudeCredential { access_token })
}

fn load_codex(
    environment: &LocalCliEnvironment,
) -> Result<LocalCodexCredential, LocalCliFallbackError> {
    ensure_linux()?;
    let value = read_bounded_json(&environment.codex_root()?.join("auth.json"))?;
    let tokens = value
        .get("tokens")
        .ok_or(LocalCliFallbackError::MissingCredential)?;
    let access_token = nonempty_string(tokens.get("access_token"))?;
    let account_id = nonempty_string(tokens.get("account_id"))?;
    Ok(LocalCodexCredential {
        access_token,
        account_id,
    })
}

fn nonempty_string(value: Option<&Value>) -> Result<String, LocalCliFallbackError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or(LocalCliFallbackError::MissingCredential)?;
    if value.trim().is_empty() {
        return Err(LocalCliFallbackError::MissingCredential);
    }
    Ok(value.to_string())
}

#[cfg(target_os = "linux")]
fn ensure_linux() -> Result<(), LocalCliFallbackError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_linux() -> Result<(), LocalCliFallbackError> {
    Err(LocalCliFallbackError::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn read_bounded_json(path: &Path) -> Result<Value, LocalCliFallbackError> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    let link_metadata = std::fs::symlink_metadata(path).map_err(map_file_error)?;
    if link_metadata.file_type().is_symlink() || !link_metadata.file_type().is_file() {
        return Err(LocalCliFallbackError::UnsafeFileType);
    }

    let mut file = std::fs::File::open(path).map_err(map_file_error)?;
    let metadata = file
        .metadata()
        .map_err(|_| LocalCliFallbackError::UnreadableFile)?;
    if !metadata.is_file() {
        return Err(LocalCliFallbackError::UnsafeFileType);
    }
    if metadata.len() > MAX_CREDENTIAL_JSON_BYTES {
        return Err(LocalCliFallbackError::OversizedFile);
    }

    let process_uid = std::fs::metadata("/proc/self")
        .map_err(|_| LocalCliFallbackError::UnsafeOwner)?
        .uid();
    if metadata.uid() != process_uid {
        return Err(LocalCliFallbackError::UnsafeOwner);
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(LocalCliFallbackError::UnsafePermissions);
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_CREDENTIAL_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalCliFallbackError::UnreadableFile)?;
    if bytes.len() as u64 > MAX_CREDENTIAL_JSON_BYTES {
        return Err(LocalCliFallbackError::OversizedFile);
    }
    serde_json::from_slice(&bytes).map_err(|_| LocalCliFallbackError::InvalidJson)
}

#[cfg(not(target_os = "linux"))]
fn read_bounded_json(_path: &Path) -> Result<Value, LocalCliFallbackError> {
    Err(LocalCliFallbackError::UnsupportedPlatform)
}

fn map_file_error(error: std::io::Error) -> LocalCliFallbackError {
    if error.kind() == std::io::ErrorKind::NotFound {
        LocalCliFallbackError::MissingFile
    } else {
        LocalCliFallbackError::UnreadableFile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "isy-local-cli-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn secure_write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn expect_error<T>(result: Result<T, LocalCliFallbackError>) -> LocalCliFallbackError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected local CLI credential rejection"),
        }
    }

    #[test]
    fn experimental_claude_fallback_respects_claude_config_dir() {
        let root = TestRoot::new("claude-config-dir");
        let home = root.path().join("home");
        let override_root = root.path().join("claude-override");
        secure_write(
            &home.join(".claude/.credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"wrong-home-token"}}"#,
        );
        secure_write(
            &override_root.join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"expected-override-token"}}"#,
        );
        let environment = LocalCliEnvironment {
            home: Some(home),
            claude_config_dir: Some(override_root),
            codex_home: None,
        };

        let credential = load_claude(&environment).unwrap();

        assert!(
            credential.access_token == "expected-override-token",
            "Claude override credential mismatch"
        );
    }

    #[test]
    fn experimental_codex_fallback_respects_codex_home() {
        let root = TestRoot::new("codex-home");
        let home = root.path().join("home");
        let override_root = root.path().join("codex-override");
        secure_write(
            &home.join(".codex/auth.json"),
            br#"{"tokens":{"access_token":"wrong-home-token","account_id":"wrong-home-account"}}"#,
        );
        secure_write(
            &override_root.join("auth.json"),
            br#"{"tokens":{"access_token":"expected-override-token","account_id":"expected-override-account"}}"#,
        );
        let environment = LocalCliEnvironment {
            home: Some(home),
            claude_config_dir: None,
            codex_home: Some(override_root),
        };

        let credential = load_codex(&environment).unwrap();

        assert!(
            credential.access_token == "expected-override-token",
            "Codex override token mismatch"
        );
        assert!(
            credential.account_id == "expected-override-account",
            "Codex override account mismatch"
        );
    }

    #[test]
    fn experimental_local_cli_fallback_uses_home_fallbacks() {
        let root = TestRoot::new("home-fallback");
        let home = root.path().join("home");
        secure_write(
            &home.join(".claude/.credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"home-claude-token"}}"#,
        );
        secure_write(
            &home.join(".codex/auth.json"),
            br#"{"tokens":{"access_token":"home-codex-token","account_id":"home-codex-account"}}"#,
        );
        let environment = LocalCliEnvironment {
            home: Some(home),
            claude_config_dir: None,
            codex_home: None,
        };

        assert!(load_claude(&environment).is_ok());
        assert!(load_codex(&environment).is_ok());
    }

    #[test]
    fn experimental_local_cli_fallback_rejects_oversized_or_malformed_json() {
        let root = TestRoot::new("invalid-json");
        let claude_root = root.path().join("claude");
        let codex_root = root.path().join("codex");
        secure_write(&claude_root.join(".credentials.json"), b"{not-json");
        secure_write(
            &codex_root.join("auth.json"),
            &vec![b' '; MAX_CREDENTIAL_JSON_BYTES as usize + 1],
        );
        let environment = LocalCliEnvironment {
            home: None,
            claude_config_dir: Some(claude_root),
            codex_home: Some(codex_root),
        };

        assert_eq!(
            expect_error(load_claude(&environment)),
            LocalCliFallbackError::InvalidJson
        );
        assert_eq!(
            expect_error(load_codex(&environment)),
            LocalCliFallbackError::OversizedFile
        );
    }

    #[test]
    fn experimental_local_cli_fallback_rejects_relative_unsafe_or_symlink_files() {
        let relative = LocalCliEnvironment {
            home: None,
            claude_config_dir: Some(PathBuf::from("relative")),
            codex_home: None,
        };
        assert_eq!(
            expect_error(load_claude(&relative)),
            LocalCliFallbackError::InvalidRoot
        );

        let root = TestRoot::new("unsafe-file");
        let unsafe_root = root.path().join("unsafe");
        let unsafe_file = unsafe_root.join(".credentials.json");
        secure_write(
            &unsafe_file,
            br#"{"claudeAiOauth":{"accessToken":"permission-sentinel"}}"#,
        );
        std::fs::set_permissions(&unsafe_file, std::fs::Permissions::from_mode(0o666)).unwrap();
        let unsafe_environment = LocalCliEnvironment {
            home: None,
            claude_config_dir: Some(unsafe_root),
            codex_home: None,
        };
        assert_eq!(
            expect_error(load_claude(&unsafe_environment)),
            LocalCliFallbackError::UnsafePermissions
        );

        let target_root = root.path().join("target");
        let link_root = root.path().join("link");
        secure_write(
            &target_root.join("credential.json"),
            br#"{"claudeAiOauth":{"accessToken":"symlink-sentinel"}}"#,
        );
        std::fs::create_dir_all(&link_root).unwrap();
        symlink(
            target_root.join("credential.json"),
            link_root.join(".credentials.json"),
        )
        .unwrap();
        let link_environment = LocalCliEnvironment {
            home: None,
            claude_config_dir: Some(link_root),
            codex_home: None,
        };
        assert_eq!(
            expect_error(load_claude(&link_environment)),
            LocalCliFallbackError::UnsafeFileType
        );
    }

    #[test]
    fn experimental_local_cli_fallback_imports_credentials_not_client_harness_state() {
        let root = TestRoot::new("minimal-bundle");
        let claude_root = root.path().join("claude");
        secure_write(
            &claude_root.join(".credentials.json"),
            br#"{
                "claudeAiOauth":{"accessToken":"credential-only-sentinel"},
                "systemPrompt":"must-not-cross-boundary",
                "tools":["shell"],
                "mcp":{"server":"must-not-cross-boundary"},
                "history":["must-not-cross-boundary"]
            }"#,
        );
        let environment = LocalCliEnvironment {
            home: None,
            claude_config_dir: Some(claude_root),
            codex_home: None,
        };

        let credential = load_claude(&environment).unwrap();

        assert!(
            credential.access_token == "credential-only-sentinel",
            "minimal Claude credential mismatch"
        );
        assert_eq!(
            std::mem::size_of_val(&credential),
            std::mem::size_of::<String>()
        );
    }

    #[test]
    fn local_cli_fallback_imports_credentials_not_client_harness_state() {
        experimental_local_cli_fallback_imports_credentials_not_client_harness_state();
    }

    #[test]
    fn experimental_local_cli_errors_are_value_and_path_free() {
        let root = TestRoot::new("redacted-error");
        let claude_root = root.path().join("private-user-path-sentinel");
        secure_write(
            &claude_root.join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":""},"email":"person@example.invalid"}"#,
        );
        let environment = LocalCliEnvironment {
            home: None,
            claude_config_dir: Some(claude_root),
            codex_home: None,
        };

        let error = expect_error(load_claude(&environment));
        let rendered = format!("{error:?} {error}");

        for forbidden in [
            "private-user-path-sentinel",
            "person@example.invalid",
            "accessToken",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "error exposed a forbidden value"
            );
        }
    }
}
