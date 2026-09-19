// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ADR-045 §4: the adapter that turns a `ConsoleOidcConfig`/`ConsoleOidcRevoke`
//! frame arriving on the console-proxy connection into a managed
//! `oidc_providers` row, via [`CloudService`]/`temps-auth`'s `OidcService`.
//!
//! # Wiring point (deliberately not done here)
//!
//! `crates/temps-cloud-client/src/console_proxy.rs` is being written in a
//! sibling worktree and is expected to expose:
//!
//! ```ignore
//! impl ConsoleProxyWorker {
//!     pub fn spawn(
//!         link: Arc<CloudLink>,
//!         dispatch_target: /* the streaming router handle, §3 */,
//!         enabled: watch::Receiver<bool>,
//!         sink: Arc<dyn ConsoleOidcSink>,
//!         cancel: watch::Receiver<bool>,
//!     ) -> JoinHandle<()>;
//! }
//!
//! #[async_trait]
//! pub trait ConsoleOidcSink: Send + Sync {
//!     async fn on_config(&self, config: ConsoleOidcConfig);
//!     async fn on_revoke(&self);
//! }
//! ```
//!
//! [`ConsoleOidcAdapter`] below is written against that exact shape (see the
//! local [`ConsoleOidcSink`] trait, which mirrors it field-for-field) so that
//! once the real trait lands, wiring is the one-line change the task
//! description promises:
//!
//! ```ignore
//! // Before (this crate, today):
//! impl crate::console_oidc::ConsoleOidcSink for ConsoleOidcAdapter { ... }
//!
//! // After (once console_proxy.rs exists):
//! impl temps_cloud_client::console_proxy::ConsoleOidcSink for ConsoleOidcAdapter { ... }
//! ```
//!
//! And the actual `spawn` call, from wherever the console-proxy connection is
//! started (expected: `CloudPlugin::initialize_plugin_services`, the same
//! place `start_heartbeat_sender`/`start_backup_mirror` are started today):
//!
//! ```ignore
//! let adapter = Arc::new(ConsoleOidcAdapter::new(service.clone()));
//! let cancel = /* CloudService's existing shutdown watch::Receiver */;
//! ConsoleProxyWorker::spawn(link, dispatch_target, service.console_access_enabled_rx(), adapter, cancel);
//! ```

use std::sync::Arc;

use crate::CloudService;

/// Local stand-in for the trait `temps-cloud-client`'s console-proxy worker
/// will define -- see the module doc for why this exists and how it is
/// meant to be replaced with a one-line `impl` change once that crate lands.
#[async_trait::async_trait]
pub trait ConsoleOidcSink: Send + Sync {
    async fn on_config(&self, issuer: String, client_id: String, client_secret: String);
    async fn on_revoke(&self);
}

/// Adapts [`CloudService`]'s managed-OIDC-provider methods to the
/// [`ConsoleOidcSink`] shape a console-proxy connection drives.
pub struct ConsoleOidcAdapter {
    service: Arc<CloudService>,
}

impl ConsoleOidcAdapter {
    pub fn new(service: Arc<CloudService>) -> Self {
        Self { service }
    }
}

#[async_trait::async_trait]
impl ConsoleOidcSink for ConsoleOidcAdapter {
    async fn on_config(&self, issuer: String, client_id: String, client_secret: String) {
        let config = temps_auth::oidc_service::ManagedCloudOidcConfig {
            issuer,
            client_id,
            client_secret,
        };
        if let Err(error) = self.service.apply_console_oidc_config(config).await {
            tracing::error!(
                %error,
                "failed to apply the managed console-access OIDC configuration Temps Cloud sent"
            );
        }
    }

    async fn on_revoke(&self) {
        if let Err(error) = self.service.revoke_console_oidc_provider().await {
            tracing::error!(
                %error,
                "failed to revoke the managed console-access OIDC provider on Cloud's request"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A fake sink that just records whether each hook fired, for testing
    /// call sites that only depend on the [`ConsoleOidcSink`] trait rather
    /// than on a real [`CloudService`] (which needs a live database).
    #[derive(Default)]
    struct RecordingSink {
        configured: AtomicBool,
        revoked: AtomicBool,
    }

    #[async_trait::async_trait]
    impl ConsoleOidcSink for RecordingSink {
        async fn on_config(&self, _issuer: String, _client_id: String, _client_secret: String) {
            self.configured.store(true, Ordering::SeqCst);
        }

        async fn on_revoke(&self) {
            self.revoked.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn sink_trait_is_object_safe_and_dispatches() {
        let sink: Arc<dyn ConsoleOidcSink> = Arc::new(RecordingSink::default());
        sink.on_config(
            "https://cloud.example.com".to_string(),
            "client".to_string(),
            "secret".to_string(),
        )
        .await;
        sink.on_revoke().await;
        // Reaching here at all proves `dyn ConsoleOidcSink` is object-safe --
        // the shape a `ConsoleProxyWorker::spawn(..., sink: Arc<dyn
        // ConsoleOidcSink>, ...)` parameter requires.
    }
}
