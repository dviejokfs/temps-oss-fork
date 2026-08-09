//! ADR-037: subscription-backed agent CLI providers for the Temps AI foundation.
//!
//! This crate introduces two types:
//!
//! - [`AgentCliAiService`]: implements [`temps_ai::AiService`] by delegating
//!   to an [`temps_agents::ai_cli::AiCliProvider`] (Claude Code, Codex,
//!   OpenCode). Subscription credentials are ambient on the host — no API key
//!   is read or forwarded.
//!
//! - [`DispatchingAiService`]: wraps an existing `Arc<dyn AiService>` and
//!   delegates every call through unchanged. In Phase 1 this is a pure
//!   pass-through. Phases 2/3 will extend it to route per-scope configuration
//!   (from `ai_gateway_config`) to either the gateway or an
//!   `AgentCliAiService` without changing the DI registration.

pub mod dispatch;
pub mod service;

pub use dispatch::DispatchingAiService;
pub use service::AgentCliAiService;
