//! [`DispatchingAiService`]: a delegating wrapper around an existing
//! [`AiService`] that establishes the dispatch seam for Phase 2/3.

use std::sync::Arc;

use async_trait::async_trait;

use temps_ai::{
    AiError, AiRequest, AiResponse, AiService, ChatTurnRequest, ChatTurnResponse, ChatTurnStream,
    TokenStream,
};

/// A pass-through [`AiService`] wrapper that establishes the dispatch seam
/// described in ADR-037.
///
/// # Phase 1 behaviour
///
/// Every method delegates directly to the wrapped `gateway`. Runtime behaviour
/// is unchanged — all requests still flow through [`temps_ai_gateway`]'s
/// `GatewayAiService`.
///
/// # Why this type exists now
///
/// Adding the wrapper in Phase 1 lets Phases 2/3 introduce routing logic
/// (reading `ai_gateway_config.provider_type`, constructing an
/// [`AgentCliAiService`](crate::AgentCliAiService) per-request) **without
/// touching the DI registration** in `temps-ai-gateway`'s `plugin.rs` again.
/// The call site moves once; all future dispatch changes stay in this type.
pub struct DispatchingAiService {
    gateway: Arc<dyn AiService>,
}

impl DispatchingAiService {
    pub fn new(gateway: Arc<dyn AiService>) -> Self {
        Self { gateway }
    }
}

#[async_trait]
impl AiService for DispatchingAiService {
    async fn is_available(&self) -> bool {
        self.gateway.is_available().await
    }

    async fn complete(&self, request: AiRequest) -> Result<AiResponse, AiError> {
        self.gateway.complete(request).await
    }

    async fn chat_stream(&self, request: ChatTurnRequest) -> Result<TokenStream, AiError> {
        self.gateway.chat_stream(request).await
    }

    async fn chat(&self, request: ChatTurnRequest) -> Result<ChatTurnResponse, AiError> {
        self.gateway.chat(request).await
    }

    async fn chat_stream_turn(&self, request: ChatTurnRequest) -> Result<ChatTurnStream, AiError> {
        self.gateway.chat_stream_turn(request).await
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use temps_ai::{AiError, AiRequest, AiResponse, ChatTurnRequest, TokenStream};

    struct AlwaysOkService {
        tag: &'static str,
    }

    #[async_trait]
    impl AiService for AlwaysOkService {
        async fn is_available(&self) -> bool {
            true
        }
        async fn complete(&self, request: AiRequest) -> Result<AiResponse, AiError> {
            Ok(AiResponse {
                text: format!("{}:{}", self.tag, request.purpose),
                json: None,
                model: "test-model".into(),
            })
        }
        async fn chat_stream(&self, _request: ChatTurnRequest) -> Result<TokenStream, AiError> {
            let s = futures::stream::once(async { Ok::<String, AiError>("chunk".into()) });
            Ok(Box::pin(s))
        }
    }

    #[tokio::test]
    async fn test_dispatching_delegates_complete() {
        let gateway: Arc<dyn AiService> = Arc::new(AlwaysOkService { tag: "gateway" });
        let dispatching = DispatchingAiService::new(gateway);

        let result = dispatching
            .complete(AiRequest {
                purpose: "test".into(),
                prompt: "hello".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(result.text, "gateway:test");
        assert_eq!(result.model, "test-model");
    }

    #[tokio::test]
    async fn test_dispatching_is_available_delegates() {
        let gateway: Arc<dyn AiService> = Arc::new(AlwaysOkService { tag: "gw" });
        let dispatching = DispatchingAiService::new(gateway);
        assert!(dispatching.is_available().await);
    }

    #[tokio::test]
    async fn test_dispatching_chat_stream_delegates() {
        use futures::StreamExt;

        let gateway: Arc<dyn AiService> = Arc::new(AlwaysOkService { tag: "gw" });
        let dispatching = DispatchingAiService::new(gateway);

        let mut stream = dispatching
            .chat_stream(ChatTurnRequest::default())
            .await
            .unwrap();

        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk, "chunk");
    }
}
