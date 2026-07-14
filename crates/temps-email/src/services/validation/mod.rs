//! Native email-address validation engine.
//!
//! Replaces the former `check-if-email-exists` dependency (AGPL-licensed, and
//! pinned to an old `hickory` with open CVEs). Four stages — syntax, MX,
//! misc signals, and SMTP probing — combine into an overall reachability
//! verdict, without ever delivering a message.

mod misc;
mod mx;
mod smtp;
mod syntax;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::errors::EmailError;

/// SOCKS5 proxy configuration for routing SMTP probes.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

const PROXY_HOST_ENV: &str = "TEMPS_EMAIL_VALIDATION_PROXY_HOST";
const PROXY_PORT_ENV: &str = "TEMPS_EMAIL_VALIDATION_PROXY_PORT";
const PROXY_USERNAME_ENV: &str = "TEMPS_EMAIL_VALIDATION_PROXY_USERNAME";
const PROXY_PASSWORD_ENV: &str = "TEMPS_EMAIL_VALIDATION_PROXY_PASSWORD";

/// Configuration for the validation service.
#[derive(Debug, Clone, Default)]
pub struct ValidationConfig {
    /// Operator-configured SOCKS5 proxy applied to every probe.
    pub proxy: Option<ProxyConfig>,
    /// Envelope sender used in `MAIL FROM` during SMTP probing.
    pub from_email: Option<String>,
    /// Name announced in the SMTP `EHLO` command.
    pub hello_name: Option<String>,
}

impl ValidationConfig {
    /// Load SMTP validation settings controlled by the Temps operator.
    pub fn from_env() -> Result<Self, EmailError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, EmailError> {
        let host = lookup(PROXY_HOST_ENV);
        let port = lookup(PROXY_PORT_ENV);
        let username = lookup(PROXY_USERNAME_ENV);
        let password = lookup(PROXY_PASSWORD_ENV);

        let proxy = match (host, port) {
            (None, None) if username.is_none() && password.is_none() => None,
            (Some(host), Some(port)) => {
                let port = port.parse::<u16>().map_err(|source| {
                    EmailError::InvalidValidationProxyPort {
                        variable: PROXY_PORT_ENV,
                        value: port.clone(),
                        source,
                    }
                })?;
                if username.is_some() != password.is_some() {
                    return Err(EmailError::InvalidValidationProxyConfig {
                        reason: format!(
                            "{PROXY_USERNAME_ENV} and {PROXY_PASSWORD_ENV} must be set together"
                        ),
                    });
                }
                Some(ProxyConfig {
                    host,
                    port,
                    username,
                    password,
                })
            }
            _ => {
                return Err(EmailError::InvalidValidationProxyConfig {
                    reason: format!("{PROXY_HOST_ENV} and {PROXY_PORT_ENV} must be set together"),
                });
            }
        };

        Ok(Self {
            proxy,
            from_email: lookup("TEMPS_EMAIL_VALIDATION_FROM_EMAIL"),
            hello_name: lookup("TEMPS_EMAIL_VALIDATION_HELLO_NAME"),
        })
    }
}

/// Request to validate a single email address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateEmailRequest {
    pub email: String,
}

/// Overall deliverability verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReachabilityStatus {
    /// Safe to send to.
    Safe,
    /// May bounce — proceed with caution.
    Risky,
    /// Invalid; will definitely bounce.
    Invalid,
    /// Could not be determined.
    Unknown,
}

/// Syntax-stage result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxResult {
    pub is_valid_syntax: bool,
    pub domain: Option<String>,
    pub username: Option<String>,
    pub suggestion: Option<String>,
}

/// MX-stage result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MxResult {
    pub accepts_mail: bool,
    pub records: Vec<String>,
    pub error: Option<String>,
}

/// Misc-signals result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiscResult {
    pub is_disposable: bool,
    pub is_role_account: bool,
    pub is_b2c: bool,
    pub gravatar_url: Option<String>,
}

/// SMTP-stage result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpResult {
    pub can_connect_smtp: bool,
    pub has_full_inbox: bool,
    pub is_catch_all: bool,
    pub is_deliverable: bool,
    pub is_disabled: bool,
    pub error: Option<String>,
}

/// Complete validation response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateEmailResponse {
    pub email: String,
    pub is_reachable: ReachabilityStatus,
    pub syntax: SyntaxResult,
    pub mx: MxResult,
    pub misc: MiscResult,
    pub smtp: SmtpResult,
}

/// Service for validating email addresses.
pub struct ValidationService {
    config: ValidationConfig,
}

impl ValidationService {
    /// Create a validation service with the given configuration.
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }

    /// Create a validation service with default configuration.
    pub fn with_default_config() -> Self {
        Self {
            config: ValidationConfig::default(),
        }
    }

    /// Validate a single email address. Never sends a message — the SMTP
    /// probe stops before `DATA`.
    pub async fn validate(
        &self,
        request: ValidateEmailRequest,
    ) -> Result<ValidateEmailResponse, EmailError> {
        info!("Validating email: {}", request.email);

        // ── Stage 1: syntax ─────────────────────────────────────────────
        let parsed = syntax::parse_email(&request.email);
        let syntax = match &parsed {
            Some(p) => SyntaxResult {
                is_valid_syntax: true,
                domain: Some(p.domain.clone()),
                username: Some(p.local_part.clone()),
                suggestion: syntax::suggest_correction(p),
            },
            None => SyntaxResult {
                is_valid_syntax: false,
                domain: None,
                username: None,
                suggestion: None,
            },
        };

        // Invalid syntax is terminal — nothing else is worth checking.
        let Some(parsed) = parsed else {
            return Ok(ValidateEmailResponse {
                email: request.email.clone(),
                is_reachable: ReachabilityStatus::Invalid,
                syntax,
                mx: MxResult {
                    accepts_mail: false,
                    records: Vec::new(),
                    error: None,
                },
                misc: MiscResult {
                    is_disposable: false,
                    is_role_account: false,
                    is_b2c: false,
                    gravatar_url: None,
                },
                smtp: SmtpResult {
                    can_connect_smtp: false,
                    has_full_inbox: false,
                    is_catch_all: false,
                    is_deliverable: false,
                    is_disabled: false,
                    error: None,
                },
            });
        };

        // ── Stage 2: misc signals (no network) ──────────────────────────
        let misc = MiscResult {
            is_disposable: misc::is_disposable(&parsed.domain),
            is_role_account: misc::is_role_account(&parsed.local_part),
            is_b2c: misc::is_b2c(&parsed.domain),
            gravatar_url: Some(misc::gravatar_url(&request.email)),
        };

        // ── Stage 3: MX lookup ──────────────────────────────────────────
        let mx_records = mx::lookup_mx(&parsed.domain).await;
        let mx = MxResult {
            accepts_mail: mx_records.accepts_mail(),
            records: mx_records.hosts.clone(),
            error: mx_records.error.clone(),
        };

        // No MX → the domain cannot receive mail; terminal Invalid.
        if !mx.accepts_mail {
            return Ok(ValidateEmailResponse {
                email: request.email.clone(),
                is_reachable: ReachabilityStatus::Invalid,
                syntax,
                mx,
                misc,
                smtp: SmtpResult {
                    can_connect_smtp: false,
                    has_full_inbox: false,
                    is_catch_all: false,
                    is_deliverable: false,
                    is_disabled: false,
                    error: None,
                },
            });
        }

        // ── Stage 4: SMTP probe ─────────────────────────────────────────
        let from_email = self
            .config
            .from_email
            .as_deref()
            .unwrap_or("noreply@temps.sh");
        let hello_name = self.config.hello_name.as_deref().unwrap_or("temps.sh");

        let probe = smtp::probe_mailbox(smtp::SmtpProbeConfig {
            mx_hosts: &mx_records.hosts,
            to_email: &request.email,
            from_email,
            hello_name,
            timeout: Duration::from_secs(10),
            deadline: Duration::from_secs(30),
            proxy: self.config.proxy.as_ref(),
        })
        .await;

        let smtp = SmtpResult {
            can_connect_smtp: probe.can_connect,
            has_full_inbox: probe.has_full_inbox,
            is_catch_all: probe.is_catch_all,
            is_deliverable: probe.is_deliverable,
            is_disabled: probe.is_disabled,
            error: probe.error.clone(),
        };

        let is_reachable = reachability(&misc, &smtp);
        debug!(
            "Email validation result for {}: is_reachable={:?}",
            request.email, is_reachable
        );

        Ok(ValidateEmailResponse {
            email: request.email,
            is_reachable,
            syntax,
            mx,
            misc,
            smtp,
        })
    }

    /// Validate several addresses sequentially.
    pub async fn validate_batch(
        &self,
        emails: Vec<String>,
    ) -> Result<Vec<ValidateEmailResponse>, EmailError> {
        let mut results = Vec::with_capacity(emails.len());
        for email in emails {
            results.push(self.validate(ValidateEmailRequest { email }).await?);
        }
        Ok(results)
    }
}

/// Combine misc + SMTP signals into the overall verdict. Syntax/MX failures
/// are handled before this point and never reach here.
fn reachability(misc: &MiscResult, smtp: &SmtpResult) -> ReachabilityStatus {
    // Could not reach any mail server, or the server wouldn't tell us — we
    // genuinely do not know.
    if !smtp.can_connect_smtp {
        return ReachabilityStatus::Unknown;
    }
    if smtp.error.is_some() && !smtp.is_deliverable {
        return ReachabilityStatus::Unknown;
    }

    // Mailbox explicitly does not exist (server reached, not deliverable, not
    // catch-all, no soft error) → Invalid.
    if !smtp.is_deliverable && !smtp.is_catch_all {
        return ReachabilityStatus::Invalid;
    }

    // From here the address is accepted. Decide Safe vs Risky.
    if smtp.is_catch_all
        || smtp.is_disabled
        || smtp.has_full_inbox
        || misc.is_disposable
        || misc.is_role_account
    {
        return ReachabilityStatus::Risky;
    }

    ReachabilityStatus::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smtp(can_connect: bool, deliverable: bool) -> SmtpResult {
        SmtpResult {
            can_connect_smtp: can_connect,
            has_full_inbox: false,
            is_catch_all: false,
            is_deliverable: deliverable,
            is_disabled: false,
            error: None,
        }
    }

    fn misc(disposable: bool, role: bool) -> MiscResult {
        MiscResult {
            is_disposable: disposable,
            is_role_account: role,
            is_b2c: false,
            gravatar_url: None,
        }
    }

    #[test]
    fn test_reachability_safe() {
        assert_eq!(
            reachability(&misc(false, false), &smtp(true, true)),
            ReachabilityStatus::Safe
        );
    }

    #[test]
    fn test_reachability_invalid_mailbox() {
        assert_eq!(
            reachability(&misc(false, false), &smtp(true, false)),
            ReachabilityStatus::Invalid
        );
    }

    #[test]
    fn test_reachability_unknown_when_unreachable() {
        assert_eq!(
            reachability(&misc(false, false), &smtp(false, false)),
            ReachabilityStatus::Unknown
        );
    }

    #[test]
    fn test_reachability_risky_disposable() {
        // Deliverable but from a disposable provider → Risky, not Safe.
        assert_eq!(
            reachability(&misc(true, false), &smtp(true, true)),
            ReachabilityStatus::Risky
        );
    }

    #[test]
    fn test_reachability_risky_role_account() {
        assert_eq!(
            reachability(&misc(false, true), &smtp(true, true)),
            ReachabilityStatus::Risky
        );
    }

    #[test]
    fn test_reachability_risky_catch_all() {
        let mut s = smtp(true, false);
        s.is_catch_all = true;
        assert_eq!(
            reachability(&misc(false, false), &s),
            ReachabilityStatus::Risky
        );
    }

    #[test]
    fn test_reachability_unknown_on_soft_error() {
        let mut s = smtp(true, false);
        s.error = Some("MAIL FROM rejected: 421 try later".to_string());
        assert_eq!(
            reachability(&misc(false, false), &s),
            ReachabilityStatus::Unknown
        );
    }

    #[tokio::test]
    async fn test_validate_invalid_syntax() {
        let service = ValidationService::with_default_config();
        let resp = service
            .validate(ValidateEmailRequest {
                email: "not-an-email".to_string(),
            })
            .await
            .unwrap();
        assert!(!resp.syntax.is_valid_syntax);
        assert_eq!(resp.is_reachable, ReachabilityStatus::Invalid);
        // Invalid syntax short-circuits before any network call.
        assert!(!resp.mx.accepts_mail);
        assert!(!resp.smtp.can_connect_smtp);
    }

    #[tokio::test]
    async fn test_validate_syntax_ok_extracts_parts() {
        let service = ValidationService::with_default_config();
        // Use an MX-less reserved domain so the test stays offline-safe:
        // .invalid never resolves, so validation stops at the MX stage.
        let resp = service
            .validate(ValidateEmailRequest {
                email: "alice@nonexistent-temps-test.invalid".to_string(),
            })
            .await
            .unwrap();
        assert!(resp.syntax.is_valid_syntax);
        assert_eq!(resp.syntax.username.as_deref(), Some("alice"));
        assert_eq!(
            resp.syntax.domain.as_deref(),
            Some("nonexistent-temps-test.invalid")
        );
    }

    #[test]
    fn test_config_default() {
        let c = ValidationConfig::default();
        assert!(c.proxy.is_none() && c.from_email.is_none() && c.hello_name.is_none());
    }

    #[test]
    fn test_config_loads_operator_proxy() {
        let values = std::collections::HashMap::from([
            (PROXY_HOST_ENV, "proxy.example.com"),
            (PROXY_PORT_ENV, "1080"),
            (PROXY_USERNAME_ENV, "smtp-user"),
            (PROXY_PASSWORD_ENV, "secret"),
            ("TEMPS_EMAIL_VALIDATION_FROM_EMAIL", "probe@example.com"),
            ("TEMPS_EMAIL_VALIDATION_HELLO_NAME", "mx.example.com"),
        ]);
        let config = ValidationConfig::from_lookup(|name| {
            values.get(name).map(|value| (*value).to_string())
        })
        .expect("complete operator proxy configuration should load");

        let proxy = config.proxy.expect("proxy should be configured");
        assert_eq!(proxy.host, "proxy.example.com");
        assert_eq!(proxy.port, 1080);
        assert_eq!(proxy.username.as_deref(), Some("smtp-user"));
        assert_eq!(proxy.password.as_deref(), Some("secret"));
        assert_eq!(config.from_email.as_deref(), Some("probe@example.com"));
        assert_eq!(config.hello_name.as_deref(), Some("mx.example.com"));
    }

    #[test]
    fn test_config_rejects_partial_or_invalid_proxy() {
        for values in [
            std::collections::HashMap::from([(PROXY_HOST_ENV, "proxy.example.com")]),
            std::collections::HashMap::from([
                (PROXY_HOST_ENV, "proxy.example.com"),
                (PROXY_PORT_ENV, "not-a-port"),
            ]),
            std::collections::HashMap::from([
                (PROXY_HOST_ENV, "proxy.example.com"),
                (PROXY_PORT_ENV, "1080"),
                (PROXY_USERNAME_ENV, "smtp-user"),
            ]),
        ] {
            let result = ValidationConfig::from_lookup(|name| {
                values.get(name).map(|value| (*value).to_string())
            });
            assert!(result.is_err(), "invalid proxy values must fail startup");
        }
    }

    #[test]
    fn test_proxy_config_debug_redacts_password() {
        let proxy = ProxyConfig {
            host: "proxy.example.com".to_string(),
            port: 1080,
            username: Some("smtp-user".to_string()),
            password: Some("super-secret-password".to_string()),
        };

        let debug = format!("{proxy:?}");
        assert!(!debug.contains("super-secret-password"));
        assert!(debug.contains("***"));
    }
}
