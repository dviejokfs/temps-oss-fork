//! Applies a published release to the running install, on request from the API.
//!
//! This is the implementation behind `temps_core::SelfUpdater`; the HTTP surface
//! lives in temps-config (`GET/POST /settings/update`). It reuses the download,
//! checksum and atomic-swap machinery of `temps upgrade` — the only genuinely
//! new problems here are (a) deciding honestly whether the process will come
//! back after it exits, and (b) reporting the outcome of an operation that by
//! definition kills the process that started it.
//!
//! **How the restart works.** Nothing in temps restarts temps. Where a
//! supervisor is detected (systemd `Restart=always`, launchd `KeepAlive`) the
//! binary is swapped and the process exits, and the supervisor brings it back
//! on the new one. Where no supervisor is detected the binary is still
//! installed — the operator asked for the update and gets it — but the process
//! deliberately keeps running the OLD version instead of exiting into an
//! outage nothing would recover from. The capability says which of the two
//! will happen BEFORE the click, and the result says the update is not live
//! until temps is restarted. See `detect_supervisor` and `SelfUpdateRestartMode`.
//!
//! **How the outcome is reported.** The attempt is written to
//! `<data_dir>/self-update.json` as `pending` immediately before exiting, and
//! resolved on the next boot by comparing the running version against the
//! target (`reconcile_journal`). So the console can say "updated to v0.2.0" or
//! "came back on the old version" instead of the operator staring at a
//! reconnecting spinner with no idea what happened.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use temps_core::{
    AvailableUpdate, ReleaseCheckResult, SelfUpdateAttempt, SelfUpdateBlocker,
    SelfUpdateCapability, SelfUpdateError, SelfUpdatePhase, SelfUpdatePolicy,
    SelfUpdateRestartMode, SelfUpdateStatus, SelfUpdater, StartedSelfUpdate, SupervisorKind,
    UpdateStatusSlot, SELF_UPDATE_JOURNAL_FILE,
};
use tracing::{error, info, warn};

use crate::commands::upgrade::{
    check_write_permission, current_version_tag, download_asset, download_asset_text,
    extract_binary_from_tarball, fetch_latest_release_in_channel, fetch_specific_release,
    is_newer_version, platform_target, replace_binary, verify_checksum, GitHubRelease,
    UpgradeChannel,
};

/// Grace period between accepting the update and exiting the process. Long
/// enough for the 202 response and one status poll to reach the console, so the
/// UI can show "restarting" instead of an unexplained connection drop.
const RESTART_GRACE: std::time::Duration = std::time::Duration::from_millis(1_500);

/// What the console should budget for the server to come back: the systemd unit
/// installed by `deploy.sh` uses `RestartSec=5`, plus boot (migrations, plugin
/// init). Advisory only — it drives the polling timeout, nothing else.
const ESTIMATED_RESTART_SECS: u64 = 45;

/// Bound on an operator-triggered check so a hung release API cannot leave the
/// request (and the spinner behind it) waiting indefinitely.
const CHECK_NOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Default)]
struct UpdaterState {
    phase: SelfUpdatePhase,
    phase_error: Option<String>,
    last_attempt: Option<SelfUpdateAttempt>,
}

/// Replaces the on-disk binary and exits so the supervisor restarts it.
pub struct BinarySelfUpdater {
    /// Binary that gets replaced — the running executable, symlinks resolved.
    binary_path: PathBuf,
    /// Directory holding the update journal.
    data_dir: PathBuf,
    /// `temps serve --disable-self-update`. A hard, API-proof off switch: unlike
    /// the settings toggle, nothing reachable over HTTP can clear it.
    disabled_by_flag: bool,
    supervisor: SupervisorKind,
    /// Split-topology note supplied by the caller, merged into `caveats()`.
    topology_caveat: Option<String>,
    /// Shared notice slot the background notifier also writes. An on-demand
    /// check republishes it so the banner and the version page never disagree.
    update_status: Arc<UpdateStatusSlot>,
    state: Arc<RwLock<UpdaterState>>,
}

impl BinarySelfUpdater {
    /// Build the updater and resolve any attempt left `pending` by the previous
    /// process. Never fails: a bad journal or an unresolvable binary path
    /// degrades to "self-update unavailable", which the capability endpoint
    /// reports with a reason.
    pub fn new(
        data_dir: PathBuf,
        disabled_by_flag: bool,
        topology_caveat: Option<String>,
        update_status: Arc<UpdateStatusSlot>,
    ) -> Self {
        let binary_path = std::env::current_exe()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .unwrap_or_else(|e| {
                warn!("Could not determine the running binary path: {}", e);
                PathBuf::new()
            });

        let supervisor = detect_supervisor();
        info!(
            supervisor = supervisor.as_str(),
            binary = %binary_path.display(),
            disabled_by_flag,
            "Self-update capability initialized"
        );

        let updater = Self {
            binary_path,
            data_dir,
            disabled_by_flag,
            supervisor,
            topology_caveat,
            update_status,
            state: Arc::new(RwLock::new(UpdaterState::default())),
        };

        let last_attempt = updater.reconcile_journal();
        if let Ok(mut state) = updater.state.write() {
            state.last_attempt = last_attempt;
        }
        updater
    }

    fn journal_path(&self) -> PathBuf {
        self.data_dir.join(SELF_UPDATE_JOURNAL_FILE)
    }

    /// Resolve an attempt the previous process left `pending`.
    ///
    /// The process that started the update cannot record its own outcome — it
    /// exits mid-flight by design. So its successor decides: if we are running
    /// the version the attempt targeted it succeeded, otherwise the swap did not
    /// take effect and the operator needs to know that (and where the previous
    /// binary was kept).
    fn reconcile_journal(&self) -> Option<SelfUpdateAttempt> {
        let path = self.journal_path();
        let mut attempt = match read_journal(&path) {
            Ok(attempt) => attempt?,
            Err(e) => {
                warn!(
                    "Ignoring unreadable self-update journal at {}: {}",
                    path.display(),
                    e
                );
                return None;
            }
        };

        // `InstalledPendingRestart` is resolvable too: the operator may be
        // restarting right now, which is exactly this boot.
        if !matches!(
            attempt.status,
            SelfUpdateStatus::Pending | SelfUpdateStatus::InstalledPendingRestart
        ) {
            return Some(attempt);
        }

        let running = current_version_tag();
        let awaiting_manual_restart = attempt.status == SelfUpdateStatus::InstalledPendingRestart;
        attempt.finished_at = Some(Utc::now());
        if attempt.to_version.as_deref() == Some(running.as_str()) {
            attempt.status = SelfUpdateStatus::Succeeded;
            info!(
                from = %attempt.from_version,
                to = %running,
                "Self-update completed: restarted on the new version"
            );
        } else if awaiting_manual_restart {
            // Still on the old binary, but nothing ever promised otherwise:
            // this install is waiting on a restart the operator hasn't done.
            // Calling that a failure would hide a pending upgrade, so the
            // attempt stays exactly where it is.
            attempt.finished_at = None;
            info!(
                installed = ?attempt.to_version,
                running = %running,
                "A self-update is installed and waiting for a restart"
            );
        } else {
            attempt.status = SelfUpdateStatus::Failed;
            let target = attempt
                .to_version
                .clone()
                .unwrap_or_else(|| "?".to_string());
            attempt.error = Some(format!(
                "Restarted on {running} instead of {target}. The binary swap did not take effect \
                 — the supervisor may have relaunched an older binary from a different path, or \
                 the new binary failed to start and was rolled back by the supervisor.{}",
                match &attempt.previous_binary_path {
                    Some(p) => format!(" The replaced binary was kept at {p}."),
                    None => String::new(),
                }
            ));
            error!(
                from = %attempt.from_version,
                target = %target,
                running = %running,
                "Self-update did not take effect"
            );
        }

        if let Err(e) = write_journal(&path, &attempt) {
            warn!("Could not persist resolved self-update journal: {}", e);
        }
        Some(attempt)
    }

    /// The command an operator runs by hand when the API path is unavailable.
    /// Always answerable — there is no state in which the manual path is gone.
    fn manual_command(&self, blocker: Option<SelfUpdateBlocker>) -> String {
        match blocker {
            Some(SelfUpdateBlocker::BinaryNotWritable) => "sudo temps upgrade".to_string(),
            _ if self.supervisor == SupervisorKind::Container => {
                "docker compose pull && docker compose up -d".to_string()
            }
            _ => "temps upgrade".to_string(),
        }
    }

    /// Everything true about this install that the operator should know before
    /// clicking, but that does not stop the update. Composed rather than
    /// single-valued because a split-topology console in a container has two
    /// separate things worth saying.
    fn caveats(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        match self.supervisor {
            SupervisorKind::Container => parts.push(
                "This server runs in a container. The new binary is written into the container's \
                 filesystem and survives a restart, but recreating the container (for example \
                 `docker compose up -d` after an image change) reverts it to the image's version \
                 — update your image tag for a durable upgrade."
                    .to_string(),
            ),
            SupervisorKind::None => parts.push(
                "No process supervisor was detected, so temps cannot restart itself. The new \
                 binary will be installed and temps will keep serving the current version until \
                 you restart it."
                    .to_string(),
            ),
            SupervisorKind::Systemd | SupervisorKind::Launchd => {}
        }
        if let Some(topology) = &self.topology_caveat {
            parts.push(topology.clone());
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// First blocker that applies, or `None` when an update can run.
    ///
    /// Order is deliberate: an explicit operator decision (`--disable-self-update`,
    /// then the settings toggle) is reported before any environmental problem,
    /// because "you turned this off" is the honest answer even on a host that
    /// also happens to be unsupervised.
    fn find_blocker(&self, policy: &SelfUpdatePolicy) -> Option<(SelfUpdateBlocker, String)> {
        if self.disabled_by_flag {
            return Some((
                SelfUpdateBlocker::DisabledByFlag,
                "The server was started with --disable-self-update. Restart it without that flag \
                 to allow updates from the console; this cannot be re-enabled from the UI."
                    .to_string(),
            ));
        }
        if !policy.enabled {
            return Some((
                SelfUpdateBlocker::DisabledBySetting,
                "One-click updates are turned off in Settings → Platform. An admin can re-enable \
                 them there."
                    .to_string(),
            ));
        }
        // NOTE: a missing supervisor is deliberately NOT a blocker. Installing
        // the binary is still exactly what the operator asked for; only the
        // restart is out of reach, and `restart_mode` says so up front.
        if let Err(e) = platform_target() {
            return Some((SelfUpdateBlocker::UnsupportedPlatform, e.to_string()));
        }
        if self.binary_path.as_os_str().is_empty() {
            return Some((
                SelfUpdateBlocker::BinaryNotWritable,
                "The path of the running binary could not be determined, so it cannot be replaced."
                    .to_string(),
            ));
        }
        if let Err(e) = check_write_permission(&self.binary_path) {
            return Some((
                SelfUpdateBlocker::BinaryNotWritable,
                format!(
                    "The server user cannot replace {}: {}",
                    self.binary_path.display(),
                    e
                ),
            ));
        }
        // Checked last: an in-flight attempt is transient, and reporting it
        // ahead of a permanent blocker would hide the real problem.
        let phase = self
            .state
            .read()
            .map(|s| s.phase)
            .unwrap_or(SelfUpdatePhase::Idle);
        if phase.is_active() {
            return Some((
                SelfUpdateBlocker::InProgress,
                format!("An update is already running ({}).", phase.as_str()),
            ));
        }
        None
    }

    fn set_phase(&self, phase: SelfUpdatePhase, phase_error: Option<String>) {
        if let Ok(mut state) = self.state.write() {
            state.phase = phase;
            state.phase_error = phase_error;
        }
    }
}

/// Resolve the channel to track: the operator's explicit setting, else the one
/// implied by the running version tag. An unparsable setting falls back to
/// inference rather than erroring, so a bad value can never wedge the checker.
pub(crate) fn resolve_channel(
    configured: Option<&str>,
    current_version: &str,
) -> (UpgradeChannel, bool) {
    match configured.and_then(UpgradeChannel::from_setting) {
        Some(channel) => (channel, true),
        None => (
            UpgradeChannel::for_installed_version(current_version),
            false,
        ),
    }
}

#[async_trait::async_trait]
impl SelfUpdater for BinarySelfUpdater {
    fn capability(&self, policy: &SelfUpdatePolicy) -> SelfUpdateCapability {
        let blocker = self.find_blocker(policy);
        let current_version = current_version_tag();
        // The capability also backs the version page, so it reports the
        // effective channel even when no update is pending.
        let (channel, pinned) = resolve_channel(policy.channel.as_deref(), &current_version);
        let (phase, phase_error, last_attempt) = match self.state.read() {
            Ok(state) => (
                state.phase,
                state.phase_error.clone(),
                state.last_attempt.clone(),
            ),
            Err(poisoned) => {
                let state = poisoned.into_inner();
                (
                    state.phase,
                    state.phase_error.clone(),
                    state.last_attempt.clone(),
                )
            }
        };

        SelfUpdateCapability {
            can_apply: blocker.is_none(),
            blocker: blocker.as_ref().map(|(b, _)| *b),
            reason: blocker.as_ref().map(|(_, r)| r.clone()),
            manual_command: self.manual_command(blocker.as_ref().map(|(b, _)| *b)),
            current_version: current_version.clone(),
            channel: channel.as_str().to_string(),
            channel_is_pinned: pinned,
            supervisor: self.supervisor,
            restart_mode: self.supervisor.restart_mode(),
            binary_path: self.binary_path.display().to_string(),
            caveat: self.caveats(),
            phase,
            phase_error,
            last_attempt,
        }
    }

    fn start(
        &self,
        target_version: Option<String>,
        triggered_by_user_id: Option<i32>,
        policy: &SelfUpdatePolicy,
    ) -> Result<StartedSelfUpdate, SelfUpdateError> {
        if let Some((blocker, reason)) = self.find_blocker(policy) {
            if blocker == SelfUpdateBlocker::InProgress {
                let phase = self
                    .state
                    .read()
                    .map(|s| s.phase)
                    .unwrap_or(SelfUpdatePhase::Idle);
                return Err(SelfUpdateError::AlreadyRunning {
                    phase: phase.as_str(),
                });
            }
            return Err(SelfUpdateError::Unavailable { blocker, reason });
        }

        let current_version = current_version_tag();
        self.set_phase(SelfUpdatePhase::Resolving, None);

        let restart_mode = self.supervisor.restart_mode();
        let (channel, _) = resolve_channel(policy.channel.as_deref(), &current_version);
        let job = UpdateJob {
            binary_path: self.binary_path.clone(),
            journal_path: self.journal_path(),
            from_version: current_version.clone(),
            target_version,
            triggered_by_user_id,
            restart_mode,
            channel,
            state: self.state.clone(),
        };
        tokio::spawn(job.run());

        info!(
            user_id = ?triggered_by_user_id,
            from = %current_version,
            restart_mode = restart_mode.as_str(),
            "Self-update accepted; downloading in the background"
        );

        Ok(StartedSelfUpdate {
            current_version,
            // Nothing goes down in manual mode, so the caller has nothing to
            // wait out.
            estimated_restart_secs: match restart_mode {
                SelfUpdateRestartMode::Automatic => ESTIMATED_RESTART_SECS,
                SelfUpdateRestartMode::Manual => 0,
            },
            restart_mode,
        })
    }

    async fn check_now(&self, channel: Option<String>) -> Result<ReleaseCheckResult, String> {
        let current_version = current_version_tag();
        let (channel, _) = resolve_channel(channel.as_deref(), &current_version);

        let release =
            tokio::time::timeout(CHECK_NOW_TIMEOUT, fetch_latest_release_in_channel(channel))
                .await
                .map_err(|_| {
                    format!(
                        "Checking the {} channel timed out after {}s.",
                        channel.as_str(),
                        CHECK_NOW_TIMEOUT.as_secs()
                    )
                })?
                .map_err(|e| format!("Could not check the {} channel: {e}", channel.as_str()))?;

        let update_available = is_newer_version(&release.tag_name, &current_version);
        if update_available {
            self.update_status.set(AvailableUpdate {
                current_version: current_version.clone(),
                latest_version: release.tag_name.clone(),
                channel: channel.as_str().to_string(),
                release_url: release.html_url.clone(),
                checked_at: Utc::now(),
            });
        } else {
            // Clear rather than leave the previous notice: after switching from
            // nightly to stable the newest stable is OLDER, so it can never
            // overwrite the stale nightly notice and the banner would lie.
            self.update_status.clear();
        }

        info!(
            channel = channel.as_str(),
            current = %current_version,
            latest = %release.tag_name,
            update_available,
            "Release check requested from the console"
        );

        Ok(ReleaseCheckResult {
            channel: channel.as_str().to_string(),
            current_version,
            latest_version: Some(release.tag_name),
            release_url: Some(release.html_url),
            update_available,
        })
    }

    fn available_update(&self) -> Option<AvailableUpdate> {
        self.update_status.get()
    }
}

/// The background half of an update: everything between "accepted" and "the
/// process is gone". Owns clones rather than borrowing the updater so it can
/// outlive the request that started it.
struct UpdateJob {
    binary_path: PathBuf,
    journal_path: PathBuf,
    from_version: String,
    target_version: Option<String>,
    triggered_by_user_id: Option<i32>,
    restart_mode: SelfUpdateRestartMode,
    /// Channel the "newest release" lookup uses when no version is pinned.
    channel: UpgradeChannel,
    state: Arc<RwLock<UpdaterState>>,
}

impl UpdateJob {
    fn set_phase(&self, phase: SelfUpdatePhase, phase_error: Option<String>) {
        if let Ok(mut state) = self.state.write() {
            state.phase = phase;
            state.phase_error = phase_error;
        }
    }

    async fn run(self) {
        let started_at = Utc::now();
        match self.execute(started_at).await {
            Ok(target) => match self.restart_mode {
                SelfUpdateRestartMode::Automatic => {
                    // The swap is done and the journal says `pending`. Exiting
                    // is now the whole job: the supervisor brings us back on
                    // the new binary and the next boot resolves the journal.
                    self.set_phase(SelfUpdatePhase::Restarting, None);
                    warn!(
                        from = %self.from_version,
                        to = %target,
                        "Binary replaced — exiting so the supervisor restarts temps on the new version"
                    );
                    tokio::time::sleep(RESTART_GRACE).await;
                    std::process::exit(0);
                }
                SelfUpdateRestartMode::Manual => {
                    // Installed, but exiting here would take the server down
                    // with nothing to bring it back. Keep serving the old
                    // binary and make the outstanding restart obvious.
                    self.set_phase(SelfUpdatePhase::PendingRestart, None);
                    warn!(
                        from = %self.from_version,
                        to = %target,
                        "Binary replaced, but no supervisor was detected — temps is STILL RUNNING \
                         {} and will pick up {} when you restart it",
                        self.from_version,
                        target
                    );
                }
            },
            Err(e) => {
                let message = e.to_string();
                error!(
                    from = %self.from_version,
                    error = %message,
                    "Self-update failed; the running binary was left untouched"
                );
                // Record the failure so it survives to the UI even if the
                // operator's tab was closed when it happened.
                let attempt = SelfUpdateAttempt {
                    from_version: self.from_version.clone(),
                    to_version: None,
                    status: SelfUpdateStatus::Failed,
                    started_at,
                    finished_at: Some(Utc::now()),
                    triggered_by_user_id: self.triggered_by_user_id,
                    error: Some(message.clone()),
                    previous_binary_path: None,
                };
                if let Err(e) = write_journal(&self.journal_path, &attempt) {
                    warn!("Could not persist failed self-update attempt: {}", e);
                }
                if let Ok(mut state) = self.state.write() {
                    state.phase = SelfUpdatePhase::Failed;
                    state.phase_error = Some(message);
                    state.last_attempt = Some(attempt);
                }
            }
        }
    }

    /// Download, verify and install. Returns the version now on disk.
    ///
    /// Every failure here leaves the running binary untouched: the new bytes go
    /// to a temp file and are only moved into place after the checksum matches
    /// AND the new binary answers `--version`.
    async fn execute(&self, started_at: chrono::DateTime<Utc>) -> anyhow::Result<String> {
        let target = platform_target()?;

        let release = self.resolve_release().await?;
        let to_version = release.tag_name.clone();
        if to_version == self.from_version {
            return Err(anyhow::anyhow!(
                "Already running {} — nothing to update",
                self.from_version
            ));
        }

        let tarball_name = format!("temps-{}.tar.gz", target);
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == tarball_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Release {} publishes no asset for this platform ({}). Available: {}",
                    to_version,
                    target,
                    release
                        .assets
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

        info!(
            to = %to_version,
            size_mb = format!("{:.1}", asset.size as f64 / 1_048_576.0),
            "Downloading release asset"
        );
        self.set_phase(SelfUpdatePhase::Downloading, None);
        let tarball = download_asset(&asset.browser_download_url).await?;

        self.set_phase(SelfUpdatePhase::Verifying, None);
        // Fail closed on a missing checksum. `temps upgrade` merely warns
        // because a human is watching the terminal and can judge; a background
        // job triggered from a browser has nobody to make that call, so it must
        // never install bytes it could not verify.
        let checksum_asset = release
            .assets
            .iter()
            .find(|a| a.name == format!("{}.sha256", tarball_name))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Release {} publishes no SHA-256 for {}, so the download cannot be verified. \
                     Upgrade from the command line if you want to install it anyway.",
                    to_version,
                    tarball_name
                )
            })?;
        let expected = download_asset_text(&checksum_asset.browser_download_url).await?;
        verify_checksum(&tarball, &expected)?;

        let new_binary = extract_binary_from_tarball(&tarball)?;
        preflight_binary(&self.binary_path, &new_binary)?;

        self.set_phase(SelfUpdatePhase::Installing, None);
        // Keep the outgoing binary next to the new one so a release that boots
        // but misbehaves can be reverted with a single `mv`, without network.
        let backup_path = backup_current_binary(&self.binary_path)?;
        replace_binary(&self.binary_path, &new_binary)?;

        // Written BEFORE the exit: once the process is gone there is no second
        // chance to record what it was doing.
        let attempt = SelfUpdateAttempt {
            from_version: self.from_version.clone(),
            to_version: Some(to_version.clone()),
            // `Pending` means "we are about to exit, resolve this on the next
            // boot". In manual mode nothing exits, so the attempt is already
            // as finished as it can get until the operator restarts.
            status: match self.restart_mode {
                SelfUpdateRestartMode::Automatic => SelfUpdateStatus::Pending,
                SelfUpdateRestartMode::Manual => SelfUpdateStatus::InstalledPendingRestart,
            },
            started_at,
            finished_at: match self.restart_mode {
                SelfUpdateRestartMode::Automatic => None,
                SelfUpdateRestartMode::Manual => Some(Utc::now()),
            },
            triggered_by_user_id: self.triggered_by_user_id,
            error: None,
            previous_binary_path: backup_path.map(|p| p.display().to_string()),
        };
        write_journal(&self.journal_path, &attempt)?;
        if let Ok(mut state) = self.state.write() {
            state.last_attempt = Some(attempt);
        }

        Ok(to_version)
    }

    /// The release to install: an explicit pin, or the newest one on the channel
    /// this install already tracks (inferred from the running version tag, the
    /// same rule the startup notifier uses).
    async fn resolve_release(&self) -> anyhow::Result<GitHubRelease> {
        match &self.target_version {
            Some(version) => fetch_specific_release(version).await,
            None => fetch_latest_release_in_channel(self.channel).await,
        }
    }
}

/// Is this process running inside a container? Checked before anything else,
/// because a swapped binary there is silently discarded on the next recreate.
fn in_container() -> bool {
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return true;
    }
    // Fallback for runtimes that create neither marker file.
    std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
        cgroup.contains("/docker/")
            || cgroup.contains("/docker-")
            || cgroup.contains("containerd")
            || cgroup.contains("kubepods")
            || cgroup.contains("/lxc/")
    })
}

/// Identify what will restart this process after it exits.
///
/// Both signals are set by the supervisor itself in the child's environment,
/// so they cannot be faked by a wrong assumption about how temps was launched:
/// systemd exports `INVOCATION_ID`, launchd exports `XPC_SERVICE_NAME`.
fn detect_supervisor() -> SupervisorKind {
    if in_container() {
        return SupervisorKind::Container;
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return SupervisorKind::Systemd;
    }
    // launchd sets this for managed jobs; processes started from a terminal
    // inherit the literal "0", which means "not a launchd service".
    if std::env::var("XPC_SERVICE_NAME").is_ok_and(|v| v != "0") {
        return SupervisorKind::Launchd;
    }
    SupervisorKind::None
}

/// Run the downloaded binary's `--version` before trusting it.
///
/// Catches the failures a checksum cannot: a build for the wrong libc, a
/// missing shared library, a corrupted extraction. Cheap insurance — without
/// it, a binary that cannot exec turns a one-click update into an outage that
/// only a console on the host can fix.
fn preflight_binary(binary_path: &Path, new_binary: &[u8]) -> anyhow::Result<()> {
    let parent = binary_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine the binary's directory"))?;
    let probe_path = parent.join(".temps-update-probe");

    std::fs::write(&probe_path, new_binary).map_err(|e| {
        anyhow::anyhow!(
            "Failed to stage the new binary at {}: {}",
            probe_path.display(),
            e
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&probe_path, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_file(&probe_path);
            return Err(anyhow::anyhow!(
                "Failed to make the staged binary executable: {}",
                e
            ));
        }
    }

    let output = std::process::Command::new(&probe_path)
        .arg("--version")
        .output();
    let _ = std::fs::remove_file(&probe_path);

    let output = output.map_err(|e| {
        anyhow::anyhow!(
            "The downloaded binary could not be executed on this host: {}. \
             The running version was left untouched.",
            e
        )
    })?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "The downloaded binary exited with {} when asked for its version: {}. \
             The running version was left untouched.",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Copy the current binary aside as `<binary>.bak`, returning where it went.
///
/// A copy (not a rename) so the target is never momentarily absent — if the
/// process dies between the two steps, the install is still intact. Best
/// effort: failing to keep a backup must not block an otherwise valid update,
/// since the release remains downloadable.
fn backup_current_binary(binary_path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let backup_path = binary_path.with_extension("bak");
    match std::fs::copy(binary_path, &backup_path) {
        Ok(_) => {
            info!(
                "Previous binary kept at {} — restore it with `mv {} {}` if the new version misbehaves",
                backup_path.display(),
                backup_path.display(),
                binary_path.display()
            );
            Ok(Some(backup_path))
        }
        Err(e) => {
            warn!(
                "Could not keep a backup of the current binary at {}: {}",
                backup_path.display(),
                e
            );
            Ok(None)
        }
    }
}

fn read_journal(path: &Path) -> anyhow::Result<Option<SelfUpdateAttempt>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    let attempt = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", path.display(), e))?;
    Ok(Some(attempt))
}

fn write_journal(path: &Path, attempt: &SelfUpdateAttempt) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("Failed to create {}: {}", parent.display(), e))?;
    }
    let json = serde_json::to_string_pretty(attempt)
        .map_err(|e| anyhow::anyhow!("Failed to serialize the update journal: {}", e))?;
    std::fs::write(path, json)
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updater(
        dir: &Path,
        disabled_by_flag: bool,
        supervisor: SupervisorKind,
    ) -> BinarySelfUpdater {
        BinarySelfUpdater {
            // A path inside the temp dir: present and writable, so the write
            // check passes and the test exercises the blocker being asserted.
            binary_path: dir.join("temps"),
            data_dir: dir.to_path_buf(),
            disabled_by_flag,
            supervisor,
            topology_caveat: None,
            update_status: Arc::new(UpdateStatusSlot::new()),
            state: Arc::new(RwLock::new(UpdaterState::default())),
        }
    }

    /// Settings-backed policy for tests: enabled/disabled, channel inferred.
    fn policy(enabled: bool) -> SelfUpdatePolicy {
        SelfUpdatePolicy {
            enabled,
            channel: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("temps-self-update-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("temps"), b"#!/bin/sh\n").expect("write fake binary");
        dir
    }

    #[test]
    fn test_flag_blocks_even_when_everything_else_is_fine() {
        let dir = temp_dir("flag");
        let cap = updater(&dir, true, SupervisorKind::Systemd).capability(&policy(true));
        assert!(!cap.can_apply);
        assert_eq!(cap.blocker, Some(SelfUpdateBlocker::DisabledByFlag));
        // The reason must say the flag cannot be cleared from the UI, or an
        // admin will hunt for a toggle that does not exist.
        assert!(cap.reason.unwrap().contains("--disable-self-update"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_flag_wins_over_settings_toggle() {
        // Both off: the flag is the one the operator cannot undo in the UI, so
        // it must be the reported blocker.
        let dir = temp_dir("flag-wins");
        let cap = updater(&dir, true, SupervisorKind::Systemd).capability(&policy(false));
        assert_eq!(cap.blocker, Some(SelfUpdateBlocker::DisabledByFlag));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_settings_toggle_blocks() {
        let dir = temp_dir("setting");
        let cap = updater(&dir, false, SupervisorKind::Systemd).capability(&policy(false));
        assert!(!cap.can_apply);
        assert_eq!(cap.blocker, Some(SelfUpdateBlocker::DisabledBySetting));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_container_installs_with_a_caveat_instead_of_being_refused() {
        let dir = temp_dir("container");
        let cap = updater(&dir, false, SupervisorKind::Container).capability(&policy(true));
        // The operator asked for the binary to be updated; a container is a
        // reason to warn, not to refuse.
        assert!(cap.can_apply, "blocked by {:?}", cap.blocker);
        assert_eq!(cap.restart_mode, SelfUpdateRestartMode::Manual);
        // ...but they must be told the swap does not survive a recreate.
        let caveat = cap.caveat.expect("container needs a caveat");
        assert!(
            caveat.contains("recreating") || caveat.contains("recreate"),
            "{caveat}"
        );
        // The durable fix points at the image, not `temps upgrade`.
        assert!(cap.manual_command.contains("docker"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unsupervised_installs_without_restarting() {
        let dir = temp_dir("unsupervised");
        let cap = updater(&dir, false, SupervisorKind::None).capability(&policy(true));
        // Installing still works with no supervisor — only the restart is out
        // of reach, and that is stated rather than used as a refusal.
        assert!(cap.can_apply, "blocked by {:?}", cap.blocker);
        assert_eq!(cap.blocker, None);
        assert_eq!(cap.restart_mode, SelfUpdateRestartMode::Manual);
        let caveat = cap.caveat.expect("manual restart needs a caveat");
        assert!(caveat.contains("restart it"), "{caveat}");
        assert_eq!(cap.manual_command, "temps upgrade");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_supervised_install_restarts_automatically_and_has_no_caveat() {
        let dir = temp_dir("supervised-mode");
        let cap = updater(&dir, false, SupervisorKind::Systemd).capability(&policy(true));
        assert_eq!(cap.restart_mode, SelfUpdateRestartMode::Automatic);
        assert_eq!(cap.caveat, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_split_topology_caveat_is_kept_alongside_the_restart_caveat() {
        let dir = temp_dir("caveats");
        let mut u = updater(&dir, false, SupervisorKind::None);
        u.topology_caveat = Some("Console only; restart the proxy too.".to_string());
        let caveat = u.capability(&policy(true)).caveat.expect("both caveats");
        assert!(caveat.contains("restart it"), "{caveat}");
        assert!(caveat.contains("restart the proxy too"), "{caveat}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_supervised_install_can_apply() {
        let dir = temp_dir("ok");
        let cap = updater(&dir, false, SupervisorKind::Systemd).capability(&policy(true));
        assert!(
            cap.can_apply,
            "blocked by {:?}: {:?}",
            cap.blocker, cap.reason
        );
        assert_eq!(cap.blocker, None);
        assert_eq!(cap.phase, SelfUpdatePhase::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_start_refused_when_disabled_reports_blocker() {
        let dir = temp_dir("start-refused");
        let err = updater(&dir, false, SupervisorKind::Systemd)
            .start(None, Some(1), &policy(false))
            .expect_err("must refuse when disabled in settings");
        assert_eq!(err.blocker(), SelfUpdateBlocker::DisabledBySetting);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_start_refused_while_another_attempt_runs() {
        let dir = temp_dir("start-busy");
        let updater = updater(&dir, false, SupervisorKind::Systemd);
        updater.set_phase(SelfUpdatePhase::Downloading, None);
        let err = updater
            .start(None, Some(1), &policy(true))
            .expect_err("must refuse a concurrent attempt");
        assert!(matches!(err, SelfUpdateError::AlreadyRunning { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_failed_phase_does_not_block_a_retry() {
        // A previous failure must not wedge the updater — the operator has to
        // be able to try again after fixing whatever broke.
        let dir = temp_dir("retry");
        let updater = updater(&dir, false, SupervisorKind::Systemd);
        updater.set_phase(SelfUpdatePhase::Failed, Some("network down".to_string()));
        let cap = updater.capability(&policy(true));
        assert!(cap.can_apply);
        assert_eq!(cap.phase, SelfUpdatePhase::Failed);
        assert_eq!(cap.phase_error.as_deref(), Some("network down"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pending_journal_resolves_to_succeeded_on_matching_version() {
        let dir = temp_dir("journal-ok");
        let running = current_version_tag();
        let attempt = SelfUpdateAttempt {
            from_version: "v0.0.1".to_string(),
            to_version: Some(running.clone()),
            status: SelfUpdateStatus::Pending,
            started_at: Utc::now(),
            finished_at: None,
            triggered_by_user_id: Some(3),
            error: None,
            previous_binary_path: None,
        };
        let path = dir.join(SELF_UPDATE_JOURNAL_FILE);
        write_journal(&path, &attempt).expect("write journal");

        let resolved = updater(&dir, false, SupervisorKind::Systemd)
            .reconcile_journal()
            .expect("journal entry");
        assert_eq!(resolved.status, SelfUpdateStatus::Succeeded);
        assert!(resolved.finished_at.is_some());
        // Resolution is persisted, so a later boot doesn't redo it.
        let reread = read_journal(&path).expect("read").expect("entry");
        assert_eq!(reread.status, SelfUpdateStatus::Succeeded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_pending_journal_resolves_to_failed_on_version_mismatch() {
        let dir = temp_dir("journal-fail");
        let attempt = SelfUpdateAttempt {
            from_version: current_version_tag(),
            // A version we are demonstrably not running.
            to_version: Some("v999.0.0".to_string()),
            status: SelfUpdateStatus::Pending,
            started_at: Utc::now(),
            finished_at: None,
            triggered_by_user_id: None,
            error: None,
            previous_binary_path: Some("/usr/local/bin/temps.bak".to_string()),
        };
        write_journal(&dir.join(SELF_UPDATE_JOURNAL_FILE), &attempt).expect("write journal");

        let resolved = updater(&dir, false, SupervisorKind::Systemd)
            .reconcile_journal()
            .expect("journal entry");
        assert_eq!(resolved.status, SelfUpdateStatus::Failed);
        let error = resolved.error.expect("failure must explain itself");
        assert!(error.contains("v999.0.0"), "{error}");
        // The operator needs to be told where the old binary went.
        assert!(error.contains("/usr/local/bin/temps.bak"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolved_journal_is_left_alone() {
        // Already-decided attempts must not be re-resolved on every boot,
        // which would rewrite a success into a failure once the NEXT version
        // is installed.
        let dir = temp_dir("journal-stable");
        let attempt = SelfUpdateAttempt {
            from_version: "v0.0.1".to_string(),
            to_version: Some("v0.0.2".to_string()),
            status: SelfUpdateStatus::Succeeded,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            triggered_by_user_id: None,
            error: None,
            previous_binary_path: None,
        };
        write_journal(&dir.join(SELF_UPDATE_JOURNAL_FILE), &attempt).expect("write journal");
        let resolved = updater(&dir, false, SupervisorKind::Systemd)
            .reconcile_journal()
            .expect("journal entry");
        assert_eq!(resolved.status, SelfUpdateStatus::Succeeded);
        assert_eq!(resolved.to_version.as_deref(), Some("v0.0.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_installed_pending_restart_survives_a_restart_on_the_old_version() {
        // The operator installed an update but has not restarted yet. Booting
        // the OLD binary must not turn that into a failure — the new binary is
        // still sitting there waiting.
        let dir = temp_dir("journal-pending-restart");
        let attempt = SelfUpdateAttempt {
            from_version: current_version_tag(),
            to_version: Some("v999.0.0".to_string()),
            status: SelfUpdateStatus::InstalledPendingRestart,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            triggered_by_user_id: Some(1),
            error: None,
            previous_binary_path: None,
        };
        write_journal(&dir.join(SELF_UPDATE_JOURNAL_FILE), &attempt).expect("write journal");

        let resolved = updater(&dir, false, SupervisorKind::None)
            .reconcile_journal()
            .expect("journal entry");
        assert_eq!(resolved.status, SelfUpdateStatus::InstalledPendingRestart);
        assert_eq!(resolved.error, None, "waiting is not a failure");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_installed_pending_restart_resolves_once_the_new_version_boots() {
        let dir = temp_dir("journal-pending-done");
        let running = current_version_tag();
        let attempt = SelfUpdateAttempt {
            from_version: "v0.0.1".to_string(),
            to_version: Some(running.clone()),
            status: SelfUpdateStatus::InstalledPendingRestart,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            triggered_by_user_id: Some(1),
            error: None,
            previous_binary_path: None,
        };
        write_journal(&dir.join(SELF_UPDATE_JOURNAL_FILE), &attempt).expect("write journal");

        let resolved = updater(&dir, false, SupervisorKind::None)
            .reconcile_journal()
            .expect("journal entry");
        assert_eq!(resolved.status, SelfUpdateStatus::Succeeded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_missing_journal_is_not_an_error() {
        let dir = temp_dir("journal-absent");
        assert!(updater(&dir, false, SupervisorKind::Systemd)
            .reconcile_journal()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_corrupt_journal_degrades_instead_of_failing_startup() {
        let dir = temp_dir("journal-corrupt");
        std::fs::write(dir.join(SELF_UPDATE_JOURNAL_FILE), "{not json").expect("write");
        assert!(updater(&dir, false, SupervisorKind::Systemd)
            .reconcile_journal()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_preflight_rejects_a_binary_that_cannot_run() {
        let dir = temp_dir("preflight");
        // Not a valid executable — exec must fail, and the error must say the
        // running version is untouched.
        let err = preflight_binary(&dir.join("temps"), b"definitely not an executable")
            .expect_err("must reject");
        assert!(err.to_string().contains("left untouched"), "{err}");
        // The probe file must not be left behind.
        assert!(!dir.join(".temps-update-probe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_backup_keeps_the_original_in_place() {
        let dir = temp_dir("backup");
        let binary = dir.join("temps");
        let backup = backup_current_binary(&binary)
            .expect("backup")
            .expect("path");
        assert!(binary.exists(), "the original must never be moved away");
        assert!(backup.exists());
        assert_eq!(
            std::fs::read(&binary).unwrap(),
            std::fs::read(&backup).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
