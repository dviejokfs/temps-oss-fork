//! Interactive terminal for a sandbox: `GET /v1/sandboxes/{id}/terminal`.
//!
//! Bridges a browser/CLI WebSocket to the in-sandbox `temps-pty-agent`
//! (ADR-008). The agent owns the PTY and keeps it alive with zero
//! subscribers, so dropping this connection does **not** kill whatever is
//! running in the tab — closing your laptop mid-`claude` and reattaching
//! later resumes the same session, with a replay of recent scrollback.
//!
//! # Protocol on the wire
//!
//! Deliberately the same shape `temps-deployments`' container terminal
//! already speaks, so one xterm.js client works against both:
//!
//! - **Binary frames** — raw PTY bytes, both directions.
//! - **Text frames (client → server)** — JSON control:
//!   `{"type":"resize","cols":N,"rows":N}`
//! - **Text frames (server → client)** — JSON events:
//!   `{"type":"ready","tab_id":"…","existed":bool}` once the tab is open,
//!   `{"type":"exit","code":N}` when the program exits,
//!   `{"type":"error","message":"…"}` for anything that went wrong.
//!
//! The agent's own framed protocol is terminated here rather than passed
//! through, so clients don't need to implement it.
//!
//! # Why the heartbeat exists
//!
//! The expiration sweeper suspends a sandbox that has been idle for
//! `timeout_secs`, and "activity" is otherwise only recorded by exec and
//! filesystem calls. Someone sitting in a terminal talking to an AI CLI
//! makes neither — so without the periodic `touch` below, the sweeper would
//! stop the container out from under a live session, killing the agent and
//! every PTY with it (`/run/temps-pty` is tmpfs). An attached terminal is
//! activity; this is what makes that true.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use temps_auth::{permissions::Permission, RequireAuth};
use temps_core::problemdetails::{self, Problem};
use temps_pty_agent::protocol as pty;
use tokio::io::AsyncWriteExt;
use tokio_util::io::StreamReader;
use utoipa::ToSchema;

use super::sandboxes::sandbox_permission_guard;
use super::SandboxAppState;

/// How often an attached terminal reports activity. Comfortably inside the
/// 60s minimum `timeout_secs` so even the shortest-lived sandbox can't be
/// swept while someone is typing in it.
const ACTIVITY_HEARTBEAT: Duration = Duration::from_secs(20);

/// Scrollback replayed on attach. Matches the agent's default ring size —
/// asking for more just gets clamped agent-side.
const REPLAY_BYTES: u32 = 64 * 1024;

/// Query parameters for the terminal upgrade.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TerminalParams {
    /// Which tab to attach to. Re-using a tab id reattaches to the running
    /// program instead of starting a second one — that is how a `claude`
    /// session survives a disconnect. Defaults to `"main"`.
    #[serde(default)]
    pub tab: Option<String>,
    /// Program to run when the tab is created. Ignored when the tab already
    /// exists. Defaults to an interactive login shell.
    #[serde(default)]
    pub cmd: Option<String>,
    /// Initial terminal width in columns. Defaults to 80.
    #[serde(default)]
    pub cols: Option<u16>,
    /// Initial terminal height in rows. Defaults to 24.
    #[serde(default)]
    pub rows: Option<u16>,
}

/// Client → server control message on a text frame.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientControl {
    Resize { cols: u16, rows: u16 },
}

/// Server → client event on a text frame.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerEvent<'a> {
    Ready {
        tab_id: &'a str,
        existed: bool,
    },
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Error {
        message: String,
    },
}

fn default_shell_cmd() -> String {
    // Login shell so ~/.bashrc runs and the AI CLIs are on PATH. The image
    // seeds PATH there precisely because non-login shells miss them.
    "/bin/bash -l".to_string()
}

/// Attach an interactive terminal to a sandbox.
#[utoipa::path(
    tag = "Sandboxes",
    get,
    path = "/v1/sandboxes/{id}/terminal",
    params(
        ("id" = String, Path, description = "Sandbox public ID"),
        ("tab" = Option<String>, Query, description = "Tab to attach to (default \"main\"); reusing an id reattaches to the running program"),
        ("cmd" = Option<String>, Query, description = "Program to launch when the tab is created (default: login shell)"),
        ("cols" = Option<u16>, Query, description = "Initial columns (default 80)"),
        ("rows" = Option<u16>, Query, description = "Initial rows (default 24)"),
    ),
    responses(
        (status = 101, description = "WebSocket established; PTY attached"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Sandbox not found"),
        (status = 409, description = "Sandbox is not in a state that can be attached to"),
    ),
    security(("bearer_auth" = []))
)]
pub async fn terminal(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<SandboxAppState>>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<TerminalParams>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, Problem> {
    sandbox_permission_guard(&auth, Permission::SandboxesWrite, Permission::ProjectsWrite)?;

    // Resolve *before* the upgrade so ownership failures, a missing sandbox,
    // or a stopped ephemeral one surface as a real HTTP status instead of a
    // WebSocket that opens and immediately dies. This also wakes a suspended
    // workspace (ADR-036), which is exactly what should happen when someone
    // opens a terminal on it.
    let (row, internal_id) = state
        .sandbox_service
        .resolve_id(&id, auth.user_id())
        .await?;

    let handle = state
        .sandbox_service
        .registry()
        .get(internal_id, &id)
        .await
        .map_err(|e| {
            let err = crate::services::sandbox_service::SandboxService::provider_err(&id, e);
            Problem::from(err)
        })?;

    let attachment = state
        .sandbox_service
        .registry()
        .provider()
        .attach_pty(&handle)
        .await
        .map_err(|e| {
            // Most likely cause on a real deployment: a sandbox created from
            // a custom image that doesn't carry the PTY agent. Say so, rather
            // than leaving the user with a terminal that never echoes.
            problemdetails::new(StatusCode::CONFLICT)
                .with_title("Terminal Unavailable")
                .with_detail(format!(
                    "Could not attach a terminal to sandbox {}: {}. \
                     Interactive terminals need the temps PTY agent, which \
                     ships in the default sandbox image.",
                    id, e
                ))
        })?;

    let service = state.sandbox_service.clone();
    let timeout_secs = row.timeout_secs;
    let tab = params.tab.unwrap_or_else(|| "main".to_string());
    let cmd = params.cmd.unwrap_or_else(default_shell_cmd);
    let cols = params.cols.unwrap_or(80);
    let rows = params.rows.unwrap_or(24);
    let work_dir = row.work_dir.clone();

    Ok(ws.on_upgrade(move |socket| async move {
        let session = TerminalSession {
            socket,
            attachment,
            service,
            sandbox_id: id,
            internal_id,
            timeout_secs,
            tab,
            cmd,
            cols,
            rows,
            work_dir,
        };
        session.run().await;
    }))
}

struct TerminalSession {
    socket: WebSocket,
    attachment: temps_agents::sandbox::PtyAttachment,
    service: Arc<crate::services::sandbox_service::SandboxService>,
    sandbox_id: String,
    internal_id: i32,
    timeout_secs: i32,
    tab: String,
    cmd: String,
    cols: u16,
    rows: u16,
    work_dir: String,
}

impl TerminalSession {
    async fn run(self) {
        let TerminalSession {
            socket,
            attachment,
            service,
            sandbox_id,
            internal_id,
            timeout_secs,
            tab,
            cmd,
            cols,
            rows,
            work_dir,
        } = self;

        let (mut ws_tx, mut ws_rx) = socket.split();
        let mut agent_in = attachment.input;

        // Map the provider's typed errors into io::Error so the frame reader
        // can consume the stream. The message is preserved — it's what the
        // user sees if the relay dies.
        let agent_bytes = attachment
            .output
            .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));
        let mut agent_out = StreamReader::new(agent_bytes);

        // OPEN the tab. Reattaches when it already exists, which is what
        // makes a long-running CLI survive disconnection.
        let open = pty::OpenRequest {
            tab_id: tab.clone(),
            kind: "shell".to_string(),
            cmd,
            cols,
            rows,
            replay_bytes: REPLAY_BYTES,
            label: Some(tab.clone()),
            cwd: Some(work_dir),
            env: None,
        };
        if let Err(e) = pty::write_json_frame(&mut agent_in, pty::OP_OPEN, &open).await {
            send_error(&mut ws_tx, format!("failed to open terminal tab: {}", e)).await;
            return;
        }

        tracing::info!(
            "Terminal attached to sandbox {} (internal {}), tab '{}'",
            sandbox_id,
            internal_id,
            tab
        );

        let mut heartbeat = tokio::time::interval(ACTIVITY_HEARTBEAT);
        // The first tick fires immediately: attaching is itself activity, and
        // it means a sandbox seconds from expiry is rescued on connect.
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Keep the sweeper honest while someone is attached.
                _ = heartbeat.tick() => {
                    service.touch(internal_id, timeout_secs).await;
                }

                // Agent → client.
                frame = pty::read_frame(&mut agent_out) => {
                    match frame {
                        Ok(Some((op, payload))) => {
                            if !forward_agent_frame(&mut ws_tx, op, payload).await {
                                break;
                            }
                        }
                        // Clean EOF: the relay exited (container stopped, or
                        // the agent went away). Nothing more to read.
                        Ok(None) => break,
                        Err(e) => {
                            send_error(&mut ws_tx, format!("terminal stream failed: {}", e)).await;
                            break;
                        }
                    }
                }

                // Client → agent.
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(Message::Binary(data))) => {
                            if pty::write_frame(&mut agent_in, pty::OP_INPUT, &data).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Text(text))) => {
                            if !handle_client_control(&mut agent_in, &text).await {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                        Some(Err(_)) => break,
                    }
                }
            }
        }

        // DETACH rather than KILL: the tab and its program stay alive for the
        // next attach. This is the whole point — a dropped connection must
        // not cost the user their session.
        let _ = pty::write_frame(&mut agent_in, pty::OP_DETACH, &[]).await;
        let _ = agent_in.shutdown().await;
        let _ = ws_tx.close().await;

        // One last activity record so a sandbox isn't swept moments after a
        // long session ends.
        service.touch(internal_id, timeout_secs).await;

        tracing::debug!(
            "Terminal detached from sandbox {}, tab '{}'",
            sandbox_id,
            tab
        );
    }
}

/// Translate one agent frame onto the WebSocket. Returns false when the
/// session should end.
async fn forward_agent_frame(
    ws_tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    op: u8,
    payload: Vec<u8>,
) -> bool {
    match op {
        pty::OP_OUTPUT => ws_tx.send(Message::Binary(payload.into())).await.is_ok(),
        pty::OP_OPENED => {
            let opened: Option<pty::OpenedResponse> = serde_json::from_slice(&payload).ok();
            let (tab_id, existed) = opened
                .as_ref()
                .map(|o| (o.tab_id.as_str(), o.existed))
                .unwrap_or(("", false));
            send_event(ws_tx, ServerEvent::Ready { tab_id, existed }).await
        }
        pty::OP_EXIT => {
            let exit: Option<pty::ExitEvent> = serde_json::from_slice(&payload).ok();
            let (code, signal) = exit.map(|e| (e.code, e.signal)).unwrap_or((None, None));
            send_event(ws_tx, ServerEvent::Exit { code, signal }).await;
            // The program is gone; nothing left to relay.
            false
        }
        pty::OP_ERROR => {
            let err: Option<pty::ErrorPayload> = serde_json::from_slice(&payload).ok();
            let message = err
                .map(|e| format!("{}: {}", e.code, e.message))
                .unwrap_or_else(|| "terminal agent reported an error".to_string());
            send_event(ws_tx, ServerEvent::Error { message }).await;
            false
        }
        // TABS/PONG are for clients that enumerate or ping; this endpoint
        // drives a single tab, so they're not interesting. Ignore rather
        // than error — a newer agent may send frames we don't know yet.
        _ => true,
    }
}

/// Apply a client control message. Returns false when the session should end.
async fn handle_client_control(
    agent_in: &mut std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    text: &str,
) -> bool {
    match serde_json::from_str::<ClientControl>(text) {
        Ok(ClientControl::Resize { cols, rows }) => {
            let payload = pty::encode_resize(cols, rows);
            pty::write_frame(agent_in, pty::OP_RESIZE, &payload)
                .await
                .is_ok()
        }
        // An unparsable control message is a client bug, not a reason to
        // drop someone's shell. Ignore it and keep going.
        Err(e) => {
            tracing::debug!("Ignoring unparsable terminal control message: {}", e);
            true
        }
    }
}

async fn send_event(
    ws_tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    event: ServerEvent<'_>,
) -> bool {
    match serde_json::to_string(&event) {
        Ok(text) => ws_tx.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => true,
    }
}

async fn send_error(ws_tx: &mut futures::stream::SplitSink<WebSocket, Message>, message: String) {
    tracing::warn!("Sandbox terminal error: {}", message);
    send_event(ws_tx, ServerEvent::Error { message }).await;
    let _ = ws_tx.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shell_is_a_login_shell() {
        // Non-login shells don't source ~/.bashrc, and that's where the image
        // puts claude/codex/opencode on PATH. Losing the `-l` makes every AI
        // CLI mysteriously "not found" inside the terminal.
        assert!(default_shell_cmd().contains("-l"));
    }

    #[test]
    fn heartbeat_stays_inside_the_shortest_idle_window() {
        // MIN_TIMEOUT_SECS is 60. A heartbeat at or above that would let the
        // sweeper stop a sandbox between two beats of a live session.
        assert!(ACTIVITY_HEARTBEAT.as_secs() < 60);
        // ...and not so chatty that an idle terminal hammers the database.
        assert!(ACTIVITY_HEARTBEAT.as_secs() >= 5);
    }

    #[test]
    fn client_resize_control_parses() {
        let parsed: ClientControl =
            serde_json::from_str(r#"{"type":"resize","cols":120,"rows":40}"#).expect("resize");
        match parsed {
            ClientControl::Resize { cols, rows } => {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
        }
    }

    #[test]
    fn server_events_are_tagged_for_xterm_clients() {
        let ready = serde_json::to_string(&ServerEvent::Ready {
            tab_id: "main",
            existed: true,
        })
        .expect("serialize");
        assert!(ready.contains(r#""type":"ready""#), "{}", ready);
        assert!(ready.contains(r#""existed":true"#), "{}", ready);

        let exit = serde_json::to_string(&ServerEvent::Exit {
            code: Some(0),
            signal: None,
        })
        .expect("serialize");
        assert!(exit.contains(r#""type":"exit""#), "{}", exit);
    }

    #[test]
    fn replay_request_is_bounded() {
        // The agent clamps this, but sending an absurd value would be a
        // pointless round trip. 64 KiB matches the agent's ring size.
        assert_eq!(REPLAY_BYTES, 64 * 1024);
    }
}
