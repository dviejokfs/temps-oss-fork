use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a proposal token remains valid before it expires.
pub const PROPOSAL_TTL: Duration = Duration::from_secs(5 * 60);

/// A pending write action awaiting human confirmation.
///
/// Created by a write tool (e.g. `trigger_deployment`); consumed exactly once
/// by `confirm_action`.  Expired or already-consumed tokens are rejected.
#[derive(Debug)]
pub struct Proposal {
    pub token: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub created_at: Instant,
    pub used: bool,
}

/// In-memory store for pending proposals.  Shared across all MCP connections
/// via `Arc<ProposalStore>` inside `McpHandlerState`.
///
/// Uses a `Mutex<HashMap>` because:
/// - Proposal operations are infrequent (human-facing flow, not hot path).
/// - `Mutex` avoids the need for an additional async dependency just for a
///   simple map.
pub struct ProposalStore {
    proposals: Mutex<HashMap<String, Proposal>>,
}

impl ProposalStore {
    pub fn new() -> Self {
        Self {
            proposals: Mutex::new(HashMap::new()),
        }
    }

    /// Acquire the inner mutex, recovering gracefully from lock poisoning.
    fn lock_store(&self) -> std::sync::MutexGuard<'_, HashMap<String, Proposal>> {
        match self.proposals.lock() {
            Ok(guard) => guard,
            // If a thread panicked while holding the lock we still get the
            // data — the map itself is consistent; only the thread is gone.
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Store a new proposal and return its token.
    pub fn create(&self, tool_name: String, arguments: serde_json::Value) -> String {
        let token = Uuid::new_v4().to_string();
        let proposal = Proposal {
            token: token.clone(),
            tool_name,
            arguments,
            created_at: Instant::now(),
            used: false,
        };
        self.lock_store().insert(token.clone(), proposal);
        token
    }

    /// Consume the proposal identified by `token`.
    ///
    /// Returns `Err(ProposalTakeError::NotFound)` when the token is unknown or
    /// already used, and `Err(ProposalTakeError::Expired)` when the TTL has
    /// elapsed.  On success the proposal is marked as used and returned.
    pub fn take(&self, token: &str) -> Result<TakenProposal, ProposalTakeError> {
        let mut store = self.lock_store();

        let proposal = store.get(token).ok_or(ProposalTakeError::NotFound)?;

        if proposal.used {
            return Err(ProposalTakeError::NotFound);
        }

        if proposal.created_at.elapsed() > PROPOSAL_TTL {
            store.remove(token);
            return Err(ProposalTakeError::Expired);
        }

        // Clone what we need before mutably borrowing.
        let taken = TakenProposal {
            tool_name: proposal.tool_name.clone(),
            arguments: proposal.arguments.clone(),
        };

        if let Some(p) = store.get_mut(token) {
            p.used = true;
        }

        Ok(taken)
    }
}

impl Default for ProposalStore {
    fn default() -> Self {
        Self::new()
    }
}

/// The data extracted from a proposal on successful consumption.
#[derive(Debug)]
pub struct TakenProposal {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// Error returned by [`ProposalStore::take`].
#[derive(Debug)]
pub enum ProposalTakeError {
    NotFound,
    Expired,
}

impl From<ProposalTakeError> for crate::error::McpError {
    fn from(e: ProposalTakeError) -> Self {
        match e {
            ProposalTakeError::NotFound => crate::error::McpError::ProposalNotFound,
            ProposalTakeError::Expired => crate::error::McpError::ProposalExpired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_take_proposal() {
        let store = ProposalStore::new();
        let args = serde_json::json!({ "project_id": 1 });
        let token = store.create("trigger_deployment".to_string(), args.clone());

        let taken = store.take(&token).expect("should consume proposal");
        assert_eq!(taken.tool_name, "trigger_deployment");
        assert_eq!(taken.arguments, args);
    }

    #[test]
    fn take_used_proposal_returns_not_found() {
        let store = ProposalStore::new();
        let token = store.create("trigger_deployment".to_string(), serde_json::json!({}));

        let _ = store.take(&token).expect("first take must succeed");
        let err = store.take(&token).expect_err("second take must fail");
        assert!(matches!(err, ProposalTakeError::NotFound));
    }

    #[test]
    fn take_unknown_token_returns_not_found() {
        let store = ProposalStore::new();
        let err = store
            .take("no-such-token")
            .expect_err("unknown token must fail");
        assert!(matches!(err, ProposalTakeError::NotFound));
    }
}
