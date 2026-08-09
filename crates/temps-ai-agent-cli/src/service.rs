//! [`AgentCliAiService`]: an [`AiService`] implementation that delegates
//! eligible workloads to a subscription-backed agent CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use temps_agents::ai_cli::{scrub_and_bound, AiCliProvider, AiRunConfig, OnEventCallback};
use temps_agents::error::AgentError;
use temps_ai::{
    extract_json_block, AiError, AiRequest, AiResponse, AiService, ChatTurnRequest, ChatTurnStream,
    TokenStream,
};

/// Hard cap on the flattened prompt size sent to an agent CLI subprocess.
/// Without this, a caller-controlled `AiRequest`/`ChatTurnRequest` could hold
/// a semaphore permit for the full timeout window with a multi-MB prompt,
/// pressuring subprocess memory and starving other tenants of the small
/// (default 2) concurrency budget.
const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// An [`AiService`] implementation that delegates eligible workloads to an
/// [`AiCliProvider`] (Claude Code, Codex, OpenCode).
///
/// # Subscription mode
///
/// `api_key` in every [`AiRunConfig`] is deliberately `""`. The operator's
/// credential is already seeded into the CLI's standard config path on the host
/// (`~/.claude/.credentials.json`, etc.) by the existing settings flow. This
/// service never reads or forwards the credential (ADR-037 §4).
///
/// # Not-delegatable workloads
///
/// Tool-calling (`chat_stream` with non-empty `tools`) returns
/// [`AiError::NotAvailable`] immediately — agent CLIs cannot be fed an
/// external function-calling protocol. See the workload eligibility table in
/// ADR-037 Decision §1. `chat()` inherits the trait's default implementation
/// (`Err(AiError::NotAvailable)`, unconditionally — agent CLIs have no
/// non-streaming function-calling path). `chat_stream_turn()` is explicitly
/// overridden to always return `Err(AiError::NotAvailable)` rather than
/// inheriting the trait default (which would delegate to `chat_stream()` and
/// — correctly, but only incidentally — execute the CLI for a *tool-less*
/// multi-turn request). Multi-turn conversation entry points must stay on the
/// BYOK gateway unconditionally; this keeps that guarantee a property of the
/// type itself, not of `chat_stream()`'s gating staying correct over time.
pub struct AgentCliAiService {
    provider: Arc<dyn AiCliProvider>,
    /// Root directory for per-invocation tempdirs. Must exist before any call.
    scratch_dir: PathBuf,
    /// Hard deadline for every `provider.run()` call (default: 30s).
    timeout: Duration,
    /// Limits concurrent CLI subprocesses on the host.
    concurrency: Arc<Semaphore>,
}

impl AgentCliAiService {
    /// Create a new service.
    ///
    /// `concurrency_limit` caps how many CLI subprocesses may run concurrently
    /// on the host (ADR-037 §5 recommends 2). `timeout` applies to every
    /// `provider.run()` invocation (ADR-037 §5 recommends 30s).
    ///
    /// # Panics
    ///
    /// Panics if `concurrency_limit` is `0`. A zero-capacity semaphore would
    /// make every call fail with "concurrency limit reached" silently — this
    /// is a misconfiguration that must fail loudly at construction, not
    /// degrade into a service that appears registered but never runs.
    pub fn new(
        provider: Arc<dyn AiCliProvider>,
        scratch_dir: PathBuf,
        timeout: Duration,
        concurrency_limit: usize,
    ) -> Self {
        assert!(
            concurrency_limit > 0,
            "AgentCliAiService concurrency_limit must be at least 1, got 0"
        );
        Self {
            provider,
            scratch_dir,
            timeout,
            concurrency: Arc::new(Semaphore::new(concurrency_limit)),
        }
    }
}

// ---------------------------------------------------------------------------
// Prompt construction helpers
// ---------------------------------------------------------------------------

/// Compose an [`AiRequest`] into a flat text prompt. When a system instruction
/// is present it is prepended with a `[System]` header so CLI models that lack
/// a native system-prompt channel still receive the full context.
fn build_prompt(request: &AiRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(system) = &request.system {
        let s = system.trim();
        if !s.is_empty() {
            parts.push(format!("[System]\n{}", s));
        }
    }
    parts.push(request.prompt.clone());
    parts.join("\n\n")
}

/// Concatenate a `ChatTurnRequest`'s message history into a flat prompt,
/// suitable for passing to a CLI that has no native multi-turn API.
fn build_chat_prompt(request: &ChatTurnRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| !m.content.is_empty())
        .map(|m| format!("[{}]\n{}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Map an [`AgentError`] to an [`AiError::Provider`] with the request's
/// purpose tag and a descriptive reason.
///
/// Defensively re-scrubs the error text through [`scrub_and_bound`] before it
/// reaches `AiError::Provider.reason` (which callers may surface to end users
/// or ship to logs). Today every `AgentError::AiCliFailed` already passes
/// through `summarize_cli_failure` (which scrubs) before reaching here, but
/// this function accepts any `AgentError` — a future provider or error path
/// that skips that upstream scrub must not be able to leak a credential
/// pattern through this boundary.
fn map_agent_error(purpose: &str, err: AgentError) -> AiError {
    AiError::Provider {
        purpose: purpose.to_owned(),
        reason: scrub_and_bound(&err.to_string()),
    }
}

/// Reject prompts over [`MAX_PROMPT_BYTES`] before any resource (semaphore
/// permit, tempdir, subprocess) is acquired for them.
fn check_prompt_size(purpose: &str, prompt: &str) -> Result<(), AiError> {
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(AiError::Provider {
            purpose: purpose.to_owned(),
            reason: format!(
                "prompt exceeds maximum size ({} bytes > {MAX_PROMPT_BYTES} byte limit)",
                prompt.len()
            ),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AiService implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl AiService for AgentCliAiService {
    /// Returns `true` when the underlying CLI reports both `installed` and
    /// `authenticated`. Callers should gate prompt construction on this check.
    async fn is_available(&self) -> bool {
        let status = self.provider.get_status().await;
        status.installed && status.authenticated
    }

    /// Single-pass completion through the agent CLI.
    ///
    /// Acquires one semaphore permit (non-blocking) before starting the
    /// subprocess. The CLI executes in a throwaway `tempdir` so it has no
    /// access to project files. JSON is extracted from the output on a
    /// best-effort basis (useful for [`temps_ai::complete_typed`] callers;
    /// note that no `response_format` enforcement is possible with CLI
    /// providers — see ADR-037 Consequences).
    async fn complete(&self, request: AiRequest) -> Result<AiResponse, AiError> {
        let purpose = request.purpose.clone();
        let prompt = build_prompt(&request);
        check_prompt_size(&purpose, &prompt)?;

        let _permit = Arc::clone(&self.concurrency)
            .try_acquire_owned()
            .map_err(|_| AiError::Provider {
                purpose: purpose.clone(),
                reason: "agent CLI concurrency limit reached — try again shortly".into(),
            })?;

        let run_dir = tempfile::tempdir_in(&self.scratch_dir).map_err(|e| {
            tracing::error!(
                purpose = %purpose,
                scratch_dir = %self.scratch_dir.display(),
                error = %e,
                "failed to create agent CLI scratch tempdir"
            );
            AiError::Provider {
                purpose: purpose.clone(),
                reason: "scratch directory unavailable; contact your administrator".into(),
            }
        })?;

        let cfg = AiRunConfig {
            work_dir: run_dir.path().to_owned(),
            prompt,
            api_key: String::new(), // subscription mode — ambient credential
            max_turns: 1,
            timeout: self.timeout,
            model: request.model.clone(),
            on_event: None, // single-pass; no streaming overhead needed
        };

        let result = tokio::time::timeout(self.timeout, self.provider.run(cfg))
            .await
            .map_err(|_| AiError::Provider {
                purpose: purpose.clone(),
                reason: format!("CLI timed out after {}s", self.timeout.as_secs()),
            })?
            .map_err(|e| map_agent_error(&purpose, e))?;

        let text = result.output.trim().to_owned();
        let json = extract_json_block(&text);

        Ok(AiResponse {
            text,
            json,
            model: result.model.unwrap_or_default(),
        })
    }

    /// Tool-less streaming completion through the agent CLI.
    ///
    /// Returns [`AiError::NotAvailable`] immediately when `request.tools` is
    /// non-empty. Agent CLIs cannot be fed an external function-calling
    /// protocol; tool-calling workloads must continue to route through the
    /// gateway (ADR-037 Decision §1).
    ///
    /// For tool-less requests each line emitted by the CLI via its `on_event`
    /// callback is forwarded as a stream chunk. The semaphore permit is held
    /// for the lifetime of the CLI subprocess (the spawned task), not just
    /// until the stream consumer is dropped.
    async fn chat_stream(&self, request: ChatTurnRequest) -> Result<TokenStream, AiError> {
        if !request.tools.is_empty() {
            return Err(AiError::NotAvailable);
        }

        let purpose = request.purpose.clone();
        let prompt = build_chat_prompt(&request);
        check_prompt_size(&purpose, &prompt)?;

        let permit = Arc::clone(&self.concurrency)
            .try_acquire_owned()
            .map_err(|_| AiError::Provider {
                purpose: purpose.clone(),
                reason: "agent CLI concurrency limit reached — try again shortly".into(),
            })?;

        let run_dir = tempfile::tempdir_in(&self.scratch_dir).map_err(|e| {
            tracing::error!(
                purpose = %purpose,
                scratch_dir = %self.scratch_dir.display(),
                error = %e,
                "failed to create agent CLI scratch tempdir"
            );
            AiError::Provider {
                purpose: purpose.clone(),
                reason: "scratch directory unavailable; contact your administrator".into(),
            }
        })?;

        // Channel capacity 64 provides enough buffer for a burst of lines
        // without back-pressure stalling the CLI subprocess.
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);

        // Build on_event: clone tx so the original can be dropped immediately,
        // making on_event the sole remaining sender. When provider.run()
        // finishes and on_event is dropped, the channel closes automatically.
        let tx_for_event = tx.clone();
        let on_event: OnEventCallback = Arc::new(move |line: String| {
            let tx = tx_for_event.clone();
            Box::pin(async move {
                let _ = tx.send(line).await;
            })
        });
        drop(tx);

        let work_dir = run_dir.path().to_owned();
        let cfg = AiRunConfig {
            work_dir,
            prompt,
            api_key: String::new(),
            max_turns: 1,
            timeout: self.timeout,
            model: request.model.clone(),
            on_event: Some(on_event),
        };

        let timeout = self.timeout;
        let provider = self.provider.clone();

        // Spawn the CLI subprocess. The permit is moved into this task so it
        // is held for the full CLI lifetime, not just while the stream consumer
        // is alive. When the CLI finishes (or times out), `on_event` is
        // dropped, closing the channel and terminating the stream.
        tokio::spawn(async move {
            let _permit = permit;
            let _tempdir = run_dir; // keep tempdir alive for the run
            let _ = tokio::time::timeout(timeout, provider.run(cfg)).await;
        });

        // Wrap the receiver as a TokenStream using unfold. This is Send
        // because Receiver<String>: Send.
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|line| (Ok::<String, AiError>(line), rx))
        });

        Ok(Box::pin(stream))
    }

    // chat() is intentionally NOT overridden: it defaults to
    // Err(AiError::NotAvailable), which is exactly right — agent CLIs have no
    // non-streaming function-calling path.

    /// Always returns [`AiError::NotAvailable`], regardless of whether
    /// `request.tools` is empty.
    ///
    /// This is a defensive override. The trait's default implementation of
    /// `chat_stream_turn` delegates to [`Self::chat_stream`], which already
    /// rejects tool-bearing requests — so relying on the default would still
    /// be *correct* for tool-calling callers, but it would also *execute* the
    /// CLI for a tool-less multi-turn request, since `chat_stream`'s gate
    /// only checks `tools.is_empty()`. Multi-turn conversation entry points
    /// (debug chat, write actions) must stay on the BYOK gateway
    /// unconditionally (ADR-037 Decision §1 / §5 scope constraint) —
    /// overriding this method makes that a property of `AgentCliAiService`
    /// itself, not a side effect of `chat_stream`'s gating happening to be
    /// correct.
    async fn chat_stream_turn(&self, _request: ChatTurnRequest) -> Result<ChatTurnStream, AiError> {
        Err(AiError::NotAvailable)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use temps_agents::ai_cli::{AiCliStatus, AiRunResult};
    use temps_agents::error::AgentError;
    use temps_ai::streaming::ChatTool;
    use temps_ai::AiRequest;

    // -----------------------------------------------------------------------
    // Mock provider helpers
    // -----------------------------------------------------------------------

    fn available_status() -> AiCliStatus {
        AiCliStatus {
            provider: "mock".into(),
            installed: true,
            version: Some("1.0.0".into()),
            authenticated: true,
            auth_method: Some("oauth".into()),
            email: None,
            subscription_type: None,
            setup_hint: None,
        }
    }

    fn unavailable_status() -> AiCliStatus {
        AiCliStatus {
            provider: "mock".into(),
            installed: true,
            version: Some("1.0.0".into()),
            authenticated: false,
            auth_method: None,
            email: None,
            subscription_type: None,
            setup_hint: Some("Run: claude auth login".into()),
        }
    }

    fn fixed_result(output: &str, model: Option<&str>) -> AiRunResult {
        AiRunResult {
            output: output.into(),
            exit_code: 0,
            tokens_input: Some(10),
            tokens_output: Some(20),
            model: model.map(String::from),
            changed_files: None,
            session_id: None,
            is_max_turns_error: false,
        }
    }

    /// A mock that returns a fixed output string. Uses a shared AtomicBool so
    /// tests can assert whether run() was called without owning the mock.
    struct MockProvider {
        status: AiCliStatus,
        output: String,
        model: Option<String>,
        called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AiCliProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        async fn check_installed(&self) -> bool {
            self.status.installed
        }
        async fn get_status(&self) -> AiCliStatus {
            self.status.clone()
        }
        async fn run(&self, _config: AiRunConfig) -> Result<AiRunResult, AgentError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(fixed_result(&self.output, self.model.as_deref()))
        }
        async fn continue_conversation(
            &self,
            config: AiRunConfig,
        ) -> Result<AiRunResult, AgentError> {
            self.run(config).await
        }
    }

    /// A mock that sleeps for a long time, used to trigger the timeout path.
    struct SlowProvider;

    #[async_trait]
    impl AiCliProvider for SlowProvider {
        fn name(&self) -> &str {
            "slow"
        }
        async fn check_installed(&self) -> bool {
            true
        }
        async fn get_status(&self) -> AiCliStatus {
            available_status()
        }
        async fn run(&self, _config: AiRunConfig) -> Result<AiRunResult, AgentError> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(fixed_result("", None))
        }
        async fn continue_conversation(
            &self,
            config: AiRunConfig,
        ) -> Result<AiRunResult, AgentError> {
            self.run(config).await
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: complete() maps AiRunResult → AiResponse correctly
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_complete_maps_to_ai_response() {
        let called = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: "Hello, world!".into(),
            model: Some("claude-3-5-sonnet".into()),
            called: called.clone(),
        });

        let scratch = tempfile::tempdir().unwrap();
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            2,
        );

        let result = service
            .complete(AiRequest {
                purpose: "test.complete".into(),
                prompt: "Say hello".into(),
                ..Default::default()
            })
            .await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let response = result.unwrap();
        assert_eq!(response.text, "Hello, world!");
        assert_eq!(response.model, "claude-3-5-sonnet");
        assert!(
            called.load(Ordering::SeqCst),
            "provider.run() was not called"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: complete() extracts JSON from prose output
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_complete_extracts_json_from_output() {
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: r#"Here is the result: {"status": "ok", "count": 42}"#.into(),
            model: None,
            called: Arc::new(AtomicBool::new(false)),
        });

        let scratch = tempfile::tempdir().unwrap();
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            2,
        );

        let response = service
            .complete(AiRequest {
                purpose: "test.json".into(),
                prompt: "Return JSON".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(response.json.is_some(), "expected JSON to be extracted");
        assert_eq!(response.json.unwrap()["count"], 42);
    }

    // -----------------------------------------------------------------------
    // Test 3: provider timeout surfaces as AiError::Provider
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_complete_timeout_surfaces_as_provider_error() {
        let provider: Arc<dyn AiCliProvider> = Arc::new(SlowProvider);

        let scratch = tempfile::tempdir().unwrap();
        // 1ms timeout ensures the SlowProvider (10s sleep) always times out.
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_millis(1),
            2,
        );

        let result = service
            .complete(AiRequest {
                purpose: "test.timeout".into(),
                prompt: "This will time out".into(),
                ..Default::default()
            })
            .await;

        match &result {
            Err(AiError::Provider { purpose, reason }) => {
                assert_eq!(purpose, "test.timeout");
                assert!(
                    reason.contains("timed out"),
                    "expected 'timed out' in reason, got: {}",
                    reason
                );
            }
            other => panic!("expected AiError::Provider with timeout, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 4: chat_stream() with non-empty tools → NotAvailable, no CLI call
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_chat_stream_with_tools_returns_not_available() {
        let called = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: "irrelevant".into(),
            model: None,
            called: called.clone(),
        });

        let scratch = tempfile::tempdir().unwrap();
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            2,
        );

        let request = ChatTurnRequest {
            purpose: "test.chat".into(),
            tools: vec![ChatTool {
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({}),
            }],
            ..Default::default()
        };

        let result = service.chat_stream(request).await;

        assert!(
            matches!(result, Err(AiError::NotAvailable)),
            "expected NotAvailable for tool-bearing request, got: {:?}",
            result.err()
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "provider.run() must not be called when tools are present"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: is_available() reflects CLI status
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_is_available_reflects_status() {
        let scratch = tempfile::tempdir().unwrap();

        // installed + authenticated → true
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: String::new(),
            model: None,
            called: Arc::new(AtomicBool::new(false)),
        });
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            2,
        );
        assert!(
            service.is_available().await,
            "installed+authenticated should be available"
        );

        // installed but NOT authenticated → false
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: unavailable_status(),
            output: String::new(),
            model: None,
            called: Arc::new(AtomicBool::new(false)),
        });
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            2,
        );
        assert!(
            !service.is_available().await,
            "unauthenticated should not be available"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: chat_stream_turn() always returns NotAvailable, even tool-less
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_chat_stream_turn_always_returns_not_available() {
        let called = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: "irrelevant".into(),
            model: None,
            called: called.clone(),
        });

        let scratch = tempfile::tempdir().unwrap();
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            2,
        );

        // Deliberately tool-less: this is the exact request shape that
        // chat_stream() would otherwise happily execute via the CLI.
        let request = ChatTurnRequest {
            purpose: "test.chat_turn".into(),
            tools: vec![],
            ..Default::default()
        };

        let result = service.chat_stream_turn(request).await;

        assert!(
            matches!(result, Err(AiError::NotAvailable)),
            "expected NotAvailable even for a tool-less request, got: {:?}",
            result.err()
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "provider.run() must not be called via chat_stream_turn()"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7: oversized prompt is rejected before any resource is acquired
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_complete_rejects_oversized_prompt_without_acquiring_resources() {
        let called = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: "irrelevant".into(),
            model: None,
            called: called.clone(),
        });

        let scratch = tempfile::tempdir().unwrap();
        // concurrency_limit 1 makes it easy to prove no permit was held: if
        // check_prompt_size() ran after acquiring, a second call would fail
        // with "concurrency limit reached" instead of the size error.
        let service = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            1,
        );

        let oversized_prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        let result = service
            .complete(AiRequest {
                purpose: "test.oversized".into(),
                prompt: oversized_prompt,
                ..Default::default()
            })
            .await;

        match &result {
            Err(AiError::Provider { purpose, reason }) => {
                assert_eq!(purpose, "test.oversized");
                assert!(
                    reason.contains("exceeds maximum size"),
                    "expected a size-limit reason, got: {}",
                    reason
                );
            }
            other => panic!("expected AiError::Provider, got: {:?}", other),
        }
        assert!(
            !called.load(Ordering::SeqCst),
            "provider.run() must not be called for an oversized prompt"
        );

        // The rejected call must not have held the sole permit: a normal
        // request should still succeed right after.
        let ok = service
            .complete(AiRequest {
                purpose: "test.after_oversized".into(),
                prompt: "small".into(),
                ..Default::default()
            })
            .await;
        assert!(
            ok.is_ok(),
            "a normal request after an oversized one should still succeed, got: {:?}",
            ok
        );
    }

    // -----------------------------------------------------------------------
    // Test 8: constructing with concurrency_limit = 0 panics
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[should_panic(expected = "concurrency_limit must be at least 1")]
    async fn test_new_panics_on_zero_concurrency_limit() {
        let provider: Arc<dyn AiCliProvider> = Arc::new(MockProvider {
            status: available_status(),
            output: String::new(),
            model: None,
            called: Arc::new(AtomicBool::new(false)),
        });
        let scratch = tempfile::tempdir().unwrap();
        let _ = AgentCliAiService::new(
            provider,
            scratch.path().to_owned(),
            Duration::from_secs(30),
            0,
        );
    }
}
