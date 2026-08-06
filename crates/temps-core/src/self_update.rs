//! Contract for applying a release update to the running binary from the API.
//!
//! Lives in temps-core because the two crates involved must not depend on each
//! other: the implementation belongs to temps-cli (it owns the binary path, the
//! release downloader and the process lifecycle), while the HTTP surface
//! belongs to temps-config (`POST /settings/update`). The console registers the
//! implementation as a service; ConfigPlugin picks it up if present.
//!
//! **Why a capability instead of "just do it":** replacing the binary is only
//! half the job — the process then has to come back on the new one. That is
//! only true when something supervises it (systemd `Restart=always`, launchd
//! `KeepAlive`). Inside a container the binary lives in the image, so a swap is
//! discarded on the next container recreate and a restart returns to the OLD
//! version; run from a shell, exiting is simply permanent downtime. So the
//! capability is reported honestly up front, with the reason and the manual
//! command to run instead, rather than discovering it after the process is gone.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// File under the data dir recording the in-flight/last update attempt. Read
/// back on the next boot to report the outcome of an update that, by
/// definition, killed the process that started it.
pub const SELF_UPDATE_JOURNAL_FILE: &str = "self-update.json";

/// What (if anything) will restart the process after it exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorKind {
    /// Started by systemd (detected via `INVOCATION_ID`). The unit installed by
    /// `deploy.sh` carries `Restart=always`, so exiting restarts on the new binary.
    Systemd,
    /// Started by launchd (detected via `XPC_SERVICE_NAME`), the macOS install path.
    Launchd,
    /// Running inside a container. The binary comes from the image, so a
    /// self-update cannot survive a container recreate — never updatable.
    Container,
    /// No supervisor found: a foreground/manual `temps serve`. Exiting is downtime.
    None,
}

impl SupervisorKind {
    /// Can a process under this supervisor be expected to come back after exit?
    pub fn restarts_on_exit(self) -> bool {
        matches!(self, Self::Systemd | Self::Launchd)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
            Self::Container => "container",
            Self::None => "none",
        }
    }
}

/// Why a one-click update is unavailable. Exactly one is reported — the most
/// fundamental blocker wins, so the operator fixes the real problem first
/// rather than clearing one only to hit the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateBlocker {
    /// The operator started the server with `--disable-self-update`. Deliberate
    /// and NOT overridable from the API — see the module docs on the two levels.
    DisabledByFlag,
    /// Turned off in Settings (`self_update.enabled = false`). An admin can turn
    /// it back on in the UI.
    DisabledBySetting,
    /// Running in a container: update the image tag instead.
    Container,
    /// Nothing would restart the process after it exits.
    NoSupervisor,
    /// The binary (or its directory) is not writable by the server user.
    BinaryNotWritable,
    /// No release assets are published for this OS/arch.
    UnsupportedPlatform,
    /// Another update attempt is already running.
    InProgress,
}

/// Where an in-flight update has got to. Polled by the console so a long
/// download shows progress instead of an indefinite spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdatePhase {
    /// Nothing running.
    #[default]
    Idle,
    /// Resolving the target release from the release API.
    Resolving,
    /// Downloading the release tarball.
    Downloading,
    /// Checking the published SHA-256 and running `--version` on the new binary.
    Verifying,
    /// Swapping the binary on disk (previous one kept as a `.bak` sibling).
    Installing,
    /// Binary swapped; the process is shutting down so the supervisor restarts it.
    Restarting,
    /// The attempt failed. The running binary was left untouched.
    Failed,
}

impl SelfUpdatePhase {
    /// Is an attempt currently occupying the updater?
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Resolving | Self::Downloading | Self::Verifying | Self::Installing
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Installing => "installing",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
        }
    }
}

/// Outcome of an update attempt, as persisted in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelfUpdateStatus {
    /// Binary swapped, process exiting — written just before shutdown and
    /// resolved on the next boot by comparing the running version to the target.
    Pending,
    /// The process came back on the target version.
    Succeeded,
    /// The attempt failed before the swap, or the process came back on the old
    /// version despite a completed swap.
    Failed,
}

/// A single update attempt. Persisted to `<data_dir>/self-update.json` so the
/// result survives the restart it causes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SelfUpdateAttempt {
    /// Version the attempt started from.
    pub from_version: String,
    /// Version the attempt targeted. `None` if it failed before resolving one.
    pub to_version: Option<String>,
    pub status: SelfUpdateStatus,
    #[schema(value_type = String, format = DateTime, example = "2026-08-06T09:12:31Z")]
    pub started_at: DateTime<Utc>,
    /// When the outcome was decided. `None` while still `Pending`.
    #[schema(value_type = Option<String>, format = DateTime)]
    pub finished_at: Option<DateTime<Utc>>,
    /// User who clicked the button. `None` for attempts started by the CLI.
    pub triggered_by_user_id: Option<i32>,
    /// Operator-facing failure reason. Always set when `status` is `Failed`.
    pub error: Option<String>,
    /// Where the replaced binary was kept, so a bad release can be reverted by
    /// hand (`mv <path> <binary>`). Set once the swap completes.
    pub previous_binary_path: Option<String>,
}

/// Everything the console needs to render the update control honestly: whether
/// it can run, why not, what to do instead, and how the last attempt went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SelfUpdateCapability {
    /// True only when a click would actually download, swap and come back up.
    pub can_apply: bool,
    /// Set iff `can_apply` is false.
    pub blocker: Option<SelfUpdateBlocker>,
    /// Operator-facing explanation of `blocker`, naming the specific thing that
    /// is wrong (path, supervisor, flag) rather than a generic refusal.
    pub reason: Option<String>,
    /// Command to run by hand instead. Always present — the manual path works
    /// even when the API path is blocked, so the operator is never stuck.
    pub manual_command: String,
    pub supervisor: SupervisorKind,
    /// Absolute path of the binary that would be replaced.
    pub binary_path: String,
    /// Something true about this topology that the operator must know *before*
    /// clicking, even though it does not block the update — currently the
    /// split-topology case, where restarting the console leaves the separate
    /// proxy process on the old binary. Shown alongside the confirmation.
    pub caveat: Option<String>,
    /// Phase of the in-flight attempt (`idle` when none).
    pub phase: SelfUpdatePhase,
    /// Failure detail for `phase == failed`, before it is cleared by a retry.
    pub phase_error: Option<String>,
    /// The most recent attempt, including one resolved on this boot.
    pub last_attempt: Option<SelfUpdateAttempt>,
}

/// Accepted-update receipt returned by `start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StartedSelfUpdate {
    /// Version the server is running right now.
    pub current_version: String,
    /// How long the console should expect to wait before the server answers
    /// again, so it can poll with a sensible timeout instead of guessing.
    pub estimated_restart_secs: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SelfUpdateError {
    #[error("Self-update is unavailable on this install: {reason}")]
    Unavailable {
        blocker: SelfUpdateBlocker,
        reason: String,
    },
    #[error("An update is already running (phase: {phase})")]
    AlreadyRunning { phase: &'static str },
}

impl SelfUpdateError {
    /// The blocker behind this error, for mapping to a response body.
    pub fn blocker(&self) -> SelfUpdateBlocker {
        match self {
            Self::Unavailable { blocker, .. } => *blocker,
            Self::AlreadyRunning { .. } => SelfUpdateBlocker::InProgress,
        }
    }
}

/// Applies a published release to the running install.
///
/// Registered as a service by `temps serve` and consumed by the settings API.
/// Absent in hosts that cannot restart themselves meaningfully (e.g. the
/// standalone proxy), in which case the endpoint reports "not supported here".
pub trait SelfUpdater: Send + Sync {
    /// Report whether an update can be applied right now.
    ///
    /// `enabled_in_settings` is passed in rather than read here: the setting
    /// lives in the database, which this trait deliberately knows nothing
    /// about. The caller (which already loaded `AppSettings`) supplies it.
    fn capability(&self, enabled_in_settings: bool) -> SelfUpdateCapability;

    /// Begin an update. Returns as soon as the attempt is accepted — the
    /// download, swap and restart continue in the background and are observable
    /// through `capability().phase`, then through the journal after the restart.
    ///
    /// `target_version` pins a specific tag; `None` takes the newest release on
    /// this install's channel.
    fn start(
        &self,
        target_version: Option<String>,
        triggered_by_user_id: Option<i32>,
        enabled_in_settings: bool,
    ) -> Result<StartedSelfUpdate, SelfUpdateError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_real_supervisors_restart_on_exit() {
        assert!(SupervisorKind::Systemd.restarts_on_exit());
        assert!(SupervisorKind::Launchd.restarts_on_exit());
        // A container restart returns to the image's binary, not the swapped
        // one, so it must never count as "will come back updated".
        assert!(!SupervisorKind::Container.restarts_on_exit());
        assert!(!SupervisorKind::None.restarts_on_exit());
    }

    #[test]
    fn test_only_pre_restart_phases_are_active() {
        for phase in [
            SelfUpdatePhase::Resolving,
            SelfUpdatePhase::Downloading,
            SelfUpdatePhase::Verifying,
            SelfUpdatePhase::Installing,
        ] {
            assert!(phase.is_active(), "{phase:?} should occupy the updater");
        }
        // Restarting is deliberately NOT active: the swap is done and the
        // process is on its way out, so a second attempt has nothing to race.
        assert!(!SelfUpdatePhase::Restarting.is_active());
        assert!(!SelfUpdatePhase::Idle.is_active());
        assert!(!SelfUpdatePhase::Failed.is_active());
    }

    #[test]
    fn test_already_running_maps_to_in_progress_blocker() {
        let err = SelfUpdateError::AlreadyRunning {
            phase: "downloading",
        };
        assert_eq!(err.blocker(), SelfUpdateBlocker::InProgress);
    }

    #[test]
    fn test_attempt_roundtrips_through_json() {
        // The journal is written by one process and read by its successor, so
        // the on-disk shape must survive a serialize/deserialize round trip.
        let attempt = SelfUpdateAttempt {
            from_version: "v0.1.0".to_string(),
            to_version: Some("v0.2.0".to_string()),
            status: SelfUpdateStatus::Pending,
            started_at: Utc::now(),
            finished_at: None,
            triggered_by_user_id: Some(7),
            error: None,
            previous_binary_path: Some("/usr/local/bin/temps.bak".to_string()),
        };
        let json = serde_json::to_string(&attempt).expect("serialize");
        let parsed: SelfUpdateAttempt = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, attempt);
    }
}
