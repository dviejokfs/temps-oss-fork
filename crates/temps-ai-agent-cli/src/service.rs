//! [`AgentCliAiService`]: an [`AiService`] implementation that delegates
//! eligible workloads to a subscription-backed agent CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use temps_agents::ai_cli::{AiCliProvider, AiRunConfig, OnEventCallback};
use temps_agents::error::AgentError;
use temps_ai::{
    extract_json_block, AiError, AiRequest, AiResponse, AiService, ChatTurnRequest, TokenStream,
};

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
/// ADR-037 Decision §1. `chat()` and `chat_stream_turn()` inherit the trait's
/// default implementations, which mean:
/// - `chat()` → `Err(AiError::NotAvailable)`
/// - `chat_stream_turn()` → delegates to `chat_stream()`, which rejects
///   tool-bearing requests, so tool-calling chat also returns
///   `Err(AiError::NotAvailable)`.
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
    pub fn new(
        provider: Arc<dyn AiCliProvider>,
        scratch_dir: PathBuf,
        timeout: Duration,
        concurrency_limit: usize,
    ) -> Self {
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
fn map_agent_error(purpose: &str, err: AgentError) -> AiError {
    AiError::Provider {
        purpose: purpose.to_owned(),
        reason: err.to_string(),
    }
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

        let _permit = Arc::clone(&self.concurrency)
            .try_acquire_owned()
            .map_err(|_| AiError::Provider {
                purpose: purpose.clone(),
                reason: "agent CLI concurrency limit reached — try again shortly".into(),
            })?;

        let run_dir = tempfile::tempdir_in(&self.scratch_dir).map_err(|e| AiError::Provider {
            purpose: purpose.clone(),
            reason: format!("failed to create scratch tempdir: {}", e),
        })?;

        let cfg = AiRunConfig {
            work_dir: run_dir.path().to_owned(),
            prompt: build_prompt(&request),
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

        let permit = Arc::clone(&self.concurrency)
            .try_acquire_owned()
            .map_err(|_| AiError::Provider {
                purpose: purpose.clone(),
                reason: "agent CLI concurrency limit reached — try again shortly".into(),
            })?;

        let run_dir = tempfile::tempdir_in(&self.scratch_dir).map_err(|e| AiError::Provider {
            purpose: purpose.clone(),
            reason: format!("failed to create scratch tempdir: {}", e),
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
            prompt: build_chat_prompt(&request),
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

    // chat() and chat_stream_turn() are intentionally NOT overridden.
    //
    // - chat() defaults to Err(AiError::NotAvailable): agent CLIs have no
    //   non-streaming function-calling path.
    // - chat_stream_turn() defaults to calling self.chat_stream(request):
    //   when tools is non-empty chat_stream() returns NotAvailable, so
    //   chat_stream_turn() correctly rejects tool-calling requests too
    //   (ADR-037 Phase 1 §2).
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
}
