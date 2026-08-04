//! Persisted link state: which account this instance is connected to, and the
//! credential it uses.
//!
//! Written atomically (temp file + rename) so a crash mid-write cannot leave a
//! half-parsed file that makes a working instance look unenrolled.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("Failed to read link state at {path}: {reason}")]
    Read { path: String, reason: String },

    #[error("Failed to write link state at {path}: {reason}")]
    Write { path: String, reason: String },

    #[error("Link state at {path} is corrupt: {reason}")]
    Corrupt { path: String, reason: String },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollmentState {
    /// Minted once, on first run, and kept forever. Stable across
    /// re-enrollment so the backend recognises a returning instance rather
    /// than accumulating duplicates.
    pub instance_id: Uuid,

    /// Base URL of the managed backend.
    pub base_url: String,

    /// Bearer token. `None` means "known backend, not linked" — a different
    /// state from having no file at all, and worth distinguishing in the UI.
    pub token: Option<String>,

    pub tenant_id: Option<Uuid>,
}

impl std::fmt::Debug for EnrollmentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollmentState")
            .field("instance_id", &self.instance_id)
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

impl EnrollmentState {
    /// A brand-new, unlinked instance.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            instance_id: Uuid::new_v4(),
            base_url: base_url.into(),
            token: None,
            tenant_id: None,
        }
    }

    pub fn is_linked(&self) -> bool {
        self.token.is_some()
    }

    /// Load, or `Ok(None)` when this instance has never been linked.
    ///
    /// A missing file is a normal state, not an error — most instances never
    /// connect anything, and treating that as a failure would fill their logs.
    pub fn load(path: &Path) -> Result<Option<Self>, StateError> {
        #[cfg(unix)]
        if path.exists() {
            use std::os::unix::fs::PermissionsExt;

            let read_err = |reason: String| StateError::Read {
                path: path.display().to_string(),
                reason,
            };
            let dir = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| read_err(format!("protect credential directory: {e}")))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| read_err(format!("protect credential file: {e}")))?;
        }

        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(StateError::Read {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };

        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| StateError::Corrupt {
                path: path.display().to_string(),
                reason: e.to_string(),
            })
    }

    /// Persist atomically.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let write_err = |reason: String| StateError::Write {
            path: path.display().to_string(),
            reason,
        };

        let dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| write_err(e.to_string()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| write_err(e.to_string()))?;
        }

        let json = serde_json::to_string_pretty(self).map_err(|e| write_err(e.to_string()))?;

        // A unique O_EXCL temporary file prevents a predictable `.tmp` symlink
        // from redirecting the credential write. Same directory keeps the final
        // persist atomic on Unix.
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .map_err(|e| write_err(format!("create secure temporary file: {e}")))?;
        tmp.write_all(json.as_bytes())
            .map_err(|e| write_err(format!("write secure temporary file: {e}")))?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| write_err(format!("sync secure temporary file: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| write_err(format!("protect secure temporary file: {e}")))?;
        }

        tmp.persist(path)
            .map_err(|e| write_err(format!("atomically replace link state: {}", e.error)))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| write_err(format!("protect link state: {e}")))?;
        }

        Ok(())
    }

    /// Forget the credential but keep the identity.
    ///
    /// Disconnecting must not mint a new `instance_id`: re-linking later should
    /// reattach to the same instance record rather than orphaning its history.
    pub fn unlink(&mut self) {
        self.token = None;
        self.tenant_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nested").join("link.json");
        (d, p)
    }

    #[test]
    fn a_never_linked_instance_loads_as_none_not_an_error() {
        let (_d, p) = temp();
        assert!(matches!(EnrollmentState::load(&p), Ok(None)));
    }

    #[test]
    fn state_round_trips_through_disk() {
        let (_d, p) = temp();
        let mut s = EnrollmentState::new("https://cloud.test");
        s.token = Some("inst_abc".into());
        s.tenant_id = Some(Uuid::new_v4());

        s.save(&p).unwrap();
        assert_eq!(EnrollmentState::load(&p).unwrap(), Some(s));
    }

    #[test]
    fn saving_creates_missing_parent_directories() {
        let (_d, p) = temp();
        EnrollmentState::new("https://cloud.test").save(&p).unwrap();
        assert!(p.exists());
    }

    #[test]
    fn a_corrupt_file_is_reported_with_its_path_not_silently_ignored() {
        let (_d, p) = temp();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{not json").unwrap();

        match EnrollmentState::load(&p) {
            Err(StateError::Corrupt { path, .. }) => {
                assert!(path.contains("link.json"), "error must name the file");
            }
            other => panic!("corruption must not be swallowed, got {other:?}"),
        }
    }

    #[test]
    fn unlinking_keeps_the_instance_identity() {
        let mut s = EnrollmentState::new("https://cloud.test");
        let id = s.instance_id;
        s.token = Some("inst_abc".into());
        s.tenant_id = Some(Uuid::new_v4());

        s.unlink();

        assert!(!s.is_linked());
        assert!(s.tenant_id.is_none());
        assert_eq!(s.instance_id, id, "re-linking must reattach, not orphan");
    }

    #[test]
    fn saving_twice_leaves_no_temp_file_behind() {
        let (_d, p) = temp();
        let s = EnrollmentState::new("https://cloud.test");
        s.save(&p).unwrap();
        s.save(&p).unwrap();
        assert!(
            !p.with_extension("tmp").exists(),
            "temp file was left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persisted_credentials_are_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;

        let (_d, p) = temp();
        let mut s = EnrollmentState::new("https://cloud.test");
        s.token = Some("inst_secret".into());
        s.save(&p).unwrap();

        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn loading_repairs_permissions_from_a_legacy_installation() {
        use std::os::unix::fs::PermissionsExt;

        let (_d, p) = temp();
        let mut s = EnrollmentState::new("https://cloud.test");
        s.token = Some("inst_secret".into());
        s.save(&p).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(p.parent().unwrap(), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        assert!(EnrollmentState::load(&p).unwrap().is_some());
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn debug_output_redacts_the_bearer_token() {
        let mut s = EnrollmentState::new("https://cloud.test");
        s.token = Some("inst_secret".into());
        assert!(!format!("{s:?}").contains("inst_secret"));
    }

    #[test]
    fn a_known_backend_without_a_token_is_not_linked() {
        // Distinct from "no file": the operator configured a backend and has
        // not finished connecting, which the UI should say plainly.
        let s = EnrollmentState::new("https://cloud.test");
        assert!(!s.is_linked());
        assert!(s.token.is_none());
    }
}
