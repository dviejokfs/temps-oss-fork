//! Wire protocol between a self-hosted Temps instance and an optional managed
//! backend.
//!
//! This crate is deliberately public and dependency-light. An operator running
//! a self-hosted instance must be able to read exactly what their instance
//! would send before deciding to connect anything.
//!
//! # Design constraints
//!
//! A released binary cannot be recalled, and instances are never force-upgraded.
//! Old versions will be talking to the backend for years. Two rules follow:
//!
//! 1. **Negotiate, never assume.** Every connection opens with [`Hello`],
//!    which carries the protocol version and a capability set. Neither side
//!    may use a capability the other did not advertise.
//! 2. **Additive changes only.** New fields are optional with defaults; new
//!    message kinds are ignored by peers that do not know them. Removing or
//!    repurposing a field requires a new [`PROTOCOL_VERSION`].
//!
//! # Boundaries
//!
//! This channel is a *control* plane: config, heartbeat, health, enrollment.
//! It never carries end-user application traffic, and the managed backend is
//! never in the request path of a deployed app. If the backend is unreachable,
//! the instance continues on its cached configuration.

#![forbid(unsafe_code)]

pub mod messages;

pub use messages::{
    BackupCompleted, BackupTarget, BackupTargetRequest, EnrollRequest, EnrollResponse, Envelope,
    Heartbeat, IngestAck, SpanRecord, TelemetryBatch,
};

use serde::{Deserialize, Serialize};

/// Bumped only for a breaking change. Additive changes must not bump it.
pub const PROTOCOL_VERSION: u16 = 1;

/// A capability one side is willing to use. Absent = unsupported; the peer
/// must fall back rather than error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Instance may ship telemetry to the managed backend for longer retention
    /// than local storage provides. Local storage is unaffected either way.
    TelemetryShipping,
    /// Instance may have backups orchestrated centrally. Backup bytes always
    /// travel instance -> object storage directly, never through the backend.
    BackupOrchestration,
    /// Instance accepts managed DNS records and certificate material for a
    /// subdomain issued by the backend.
    ManagedSubdomain,
}

/// First frame on every connection, sent by both sides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    /// Human-readable build identifier, for support and skew diagnostics.
    pub agent_version: String,
    /// What this side is willing to do. The effective set is the intersection.
    pub capabilities: Vec<Capability>,
}

impl Hello {
    /// Capabilities usable on this connection: the intersection of both sides.
    ///
    /// Returns an error only for an incompatible major protocol version --
    /// a capability the peer lacks is a normal, non-fatal outcome.
    pub fn negotiate(&self, peer: &Hello) -> Result<Vec<Capability>, ProtocolError> {
        if peer.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: peer.protocol_version,
            });
        }
        Ok(self
            .capabilities
            .iter()
            .copied()
            .filter(|c| peer.capabilities.contains(c))
            .collect())
    }
}

/// Why a managed feature is unavailable, so the instance can say something
/// specific instead of failing silently or showing a generic error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Unavailable {
    /// No account is connected. The instance should offer to connect one.
    NotEnrolled,
    /// Enrolled, but the plan does not include this capability.
    NotEntitled { required_plan: String },
    /// Included, but the period allowance is exhausted.
    QuotaExhausted {
        used_bytes: u64,
        limit_bytes: u64,
        resets_at: chrono::DateTime<chrono::Utc>,
    },
    /// Backend reachable but degraded. The instance keeps buffering locally.
    Degraded {
        retry_after_secs: u32,
        detail: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol version mismatch: ours {ours}, peer {theirs}")]
    VersionMismatch { ours: u16, theirs: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(caps: &[Capability]) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "test".into(),
            capabilities: caps.to_vec(),
        }
    }

    #[test]
    fn negotiate_yields_the_intersection() {
        let ours = hello(&[
            Capability::TelemetryShipping,
            Capability::BackupOrchestration,
        ]);
        let theirs = hello(&[Capability::TelemetryShipping, Capability::ManagedSubdomain]);
        assert_eq!(
            ours.negotiate(&theirs).unwrap(),
            vec![Capability::TelemetryShipping]
        );
    }

    #[test]
    fn a_capability_the_peer_lacks_is_not_an_error() {
        let ours = hello(&[Capability::TelemetryShipping]);
        let theirs = hello(&[]);
        assert!(ours.negotiate(&theirs).unwrap().is_empty());
    }

    #[test]
    fn version_mismatch_is_fatal() {
        let ours = hello(&[]);
        let mut theirs = hello(&[]);
        theirs.protocol_version = PROTOCOL_VERSION + 1;
        assert!(matches!(
            ours.negotiate(&theirs),
            Err(ProtocolError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn unknown_message_kinds_do_not_break_deserialisation() {
        // Additive-change guarantee: a peer sending a variant we do not know
        // must not take down the connection.
        let json = r#"{"reason":"not_enrolled"}"#;
        assert!(serde_json::from_str::<Unavailable>(json).is_ok());
    }
}
