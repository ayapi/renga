//! IPC server: accepts connections on a named endpoint and forwards
//! each request to the App's command channel.
//!
//! Wire protocol: newline-delimited JSON. A connection must start with
//! a `Hello` request; the server replies with a [`Response::Hello`]
//! carrying its PID and a per-instance session token. The client then
//! sends exactly one command and reads exactly one response before the
//! server closes its side.
//!
//! Threading model:
//! - One accept thread lives for the process lifetime and blocks on
//!   `listener.incoming()`.
//! - Each connection is handed to a short-lived worker thread so a slow
//!   client can't starve the accept loop.
//! - Workers communicate with the App by pushing an [`AppCommand`] into
//!   the shared `Sender<AppCommand>` and blocking on a [`oneshot`] reply
//!   with a timeout, so an unresponsive App can never hang a worker
//!   indefinitely.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Stream};

use super::endpoint::{EndpointKind, EndpointName};
use super::events::EventBus;
use super::{err_code, Event, Request, Response, APP_REPLY_TIMEOUT};
use crate::app::AppCommand;
#[cfg(test)]
use crate::app::SplitOutcome;

/// Upper bound for waiting on the accept thread during shutdown.
/// `Drop` must not hang on an uncooperative accept thread — if the
/// self-connect wakeup somehow fails and the thread stays blocked in
/// `listener.incoming()`, we'd rather leak the thread (the OS reaps
/// it on process exit) than stall the whole process from teardown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct IpcServer {
    pub endpoint: EndpointName,
    stop: Arc<AtomicBool>,
    /// Signaled once the accept thread returns. Using a channel rather
    /// than `JoinHandle::join` so Drop can wait with a timeout.
    done_rx: Option<mpsc::Receiver<()>>,
}

#[derive(Default)]
struct PeerSubscriptionState {
    counts: HashMap<usize, usize>,
    /// Panes seen with the additive `subscribe.pane_id` key. A pane not
    /// in this set may be served by an older peer which predates the key,
    /// so its readiness requests retain the legacy behavior.
    managed_panes: HashSet<usize>,
}

/// Server-owned view of live MCP event streams. Generic subscribers such
/// as `renga events` never enter this registry.
#[derive(Clone, Default)]
struct PeerSubscriptionRegistry {
    state: Arc<Mutex<PeerSubscriptionState>>,
}

impl PeerSubscriptionRegistry {
    fn register(&self, pane_id: usize) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.managed_panes.insert(pane_id);
        *state.counts.entry(pane_id).or_default() += 1;
    }

    /// Decrement a pane's stream count and run `on_last` while still
    /// holding the registry lock. Keeping the zero notification inside
    /// the lock is what orders it before a later registration and its
    /// post-ack readiness request.
    fn unregister_with(&self, pane_id: usize, on_last: impl FnOnce()) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(count) = state.counts.get_mut(&pane_id) else {
            return;
        };
        if *count > 1 {
            *count -= 1;
            return;
        }
        state.counts.remove(&pane_id);
        on_last();
    }

    fn unregister(&self, pane_id: usize, command_tx: &Sender<AppCommand>) {
        self.unregister_with(pane_id, || {
            let _ = command_tx.send(AppCommand::PeerSubscriberGone { pane_id });
        });
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Orderly shutdown so the accept thread exits before we remove
        // the socket file, avoiding the "stale listener, new path"
        // rebinding race: (1) flip the stop flag, (2) self-connect to
        // unblock the blocked `accept()` call, (3) wait for the thread
        // to signal completion (bounded), then (4) unlink on Unix.
        self.stop.store(true, Ordering::Release);
        unblock_accept(&self.endpoint);
        if let Some(rx) = self.done_rx.take() {
            let _ = rx.recv_timeout(SHUTDOWN_TIMEOUT);
        }
        if self.endpoint.kind() == EndpointKind::Socket {
            let _ = std::fs::remove_file(self.endpoint.as_str());
        }
    }
}

/// Open and immediately drop a client connection to the server's own
/// endpoint. This wakes the blocked `Listener::incoming()` call so the
/// accept thread can observe the stop flag and exit. Any error is
/// ignored — the endpoint may already be torn down from an earlier
/// Drop pass.
fn unblock_accept(endpoint: &EndpointName) {
    let name = match endpoint_to_name(endpoint) {
        Ok(n) => n,
        Err(_) => return,
    };
    let _ = Stream::connect(name);
}

fn endpoint_to_name(endpoint: &EndpointName) -> Result<interprocess::local_socket::Name<'_>> {
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::NamedPipe;
        Ok(endpoint.as_str().to_fs_name::<NamedPipe>()?)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        Ok(endpoint.as_str().to_fs_name::<GenericFilePath>()?)
    }
}

impl IpcServer {
    /// Bind the listener and start accepting in a background thread.
    pub fn spawn(
        endpoint: EndpointName,
        command_tx: Sender<AppCommand>,
        session_token: String,
        event_bus: EventBus,
    ) -> Result<Self> {
        let listener = bind_listener(&endpoint)
            .with_context(|| format!("bind IPC endpoint {}", endpoint.as_str()))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let token_for_thread = session_token.clone();
        let endpoint_for_log = endpoint.as_str().to_string();
        let (done_tx, done_rx) = mpsc::channel();
        let peer_subscriptions = PeerSubscriptionRegistry::default();
        thread::Builder::new()
            .name("renga-ipc-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    command_tx,
                    token_for_thread,
                    endpoint_for_log,
                    stop_for_thread,
                    event_bus,
                    peer_subscriptions,
                );
                // Signal Drop that the accept loop has returned. If the
                // receiver is already gone (Drop finished first because
                // of the timeout) the send errors out silently; we
                // don't care.
                let _ = done_tx.send(());
            })
            .context("spawn IPC accept thread")?;

        // Token is consumed by the accept thread via `token_for_thread`;
        // we don't need to keep a copy on the struct.
        drop(session_token);
        Ok(Self {
            endpoint,
            stop,
            done_rx: Some(done_rx),
        })
    }
}

fn bind_listener(endpoint: &EndpointName) -> Result<interprocess::local_socket::Listener> {
    // `to_fs_name` lets us pass an OS-native path (both the Windows
    // pipe name `\\.\pipe\…` and a Unix socket path are absolute file
    // names). `try_overwrite(true)` replaces a stale Unix socket file
    // left behind by a crashed previous instance — on Windows the
    // equivalent is a no-op because Named Pipes don't leak files.
    #[cfg(windows)]
    let name = {
        use interprocess::os::windows::local_socket::NamedPipe;
        endpoint.as_str().to_fs_name::<NamedPipe>()?
    };
    #[cfg(unix)]
    let name = {
        use interprocess::local_socket::GenericFilePath;
        endpoint.as_str().to_fs_name::<GenericFilePath>()?
    };

    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    Ok(listener)
}

fn accept_loop(
    listener: interprocess::local_socket::Listener,
    command_tx: Sender<AppCommand>,
    session_token: String,
    endpoint_for_log: String,
    stop: Arc<AtomicBool>,
    event_bus: EventBus,
    peer_subscriptions: PeerSubscriptionRegistry,
) {
    for conn in listener.incoming() {
        // The self-connect triggered by IpcServer::drop returns here;
        // observing the stop flag before handle_connection lets us
        // exit cleanly instead of serving one last spurious request.
        if stop.load(Ordering::Acquire) {
            return;
        }
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                // Accept failures on a shutdown path are expected (the
                // listener got unlinked under us); on a normal path
                // they're transient and shouldn't kill the server.
                if stop.load(Ordering::Acquire) {
                    return;
                }
                eprintln!("renga IPC: accept failed on {endpoint_for_log}: {e}");
                continue;
            }
        };

        let tx = command_tx.clone();
        let token = session_token.clone();
        let bus = event_bus.clone();
        let subscriptions = peer_subscriptions.clone();
        if let Err(e) = thread::Builder::new()
            .name("renga-ipc-worker".into())
            .spawn(move || {
                if let Err(e) = handle_connection(conn, tx, &token, bus, subscriptions) {
                    eprintln!("renga IPC: connection error: {e}");
                }
            })
        {
            // Thread spawn failures are extremely rare (EAGAIN under
            // system pressure). Dropping the connection is safe — the
            // client sees EOF and can retry. We deliberately don't fall
            // back to inline handling because that would block the
            // accept loop behind a slow request.
            eprintln!("renga IPC: worker spawn failed, dropping connection: {e}");
        }
    }
}

fn handle_connection(
    conn: Stream,
    command_tx: Sender<AppCommand>,
    session_token: &str,
    event_bus: EventBus,
    peer_subscriptions: PeerSubscriptionRegistry,
) -> Result<()> {
    // The stream is split by wrapping in BufReader for line-buffered
    // reads; writes go through BufReader::get_mut. We can't construct
    // two BufReader clones without a split, so we borrow mutably.
    let mut reader = BufReader::new(conn);
    let mut line = String::new();

    // ── 1. Handshake ───────────────────────────────────────
    if read_line_or_eof(&mut reader, &mut line)?.is_none() {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return write_response_line(
                reader.get_mut(),
                &Response::err_coded(err_code::PARSE, format!("parse error on hello: {e}")),
            );
        }
    };
    match req {
        Request::Hello { client_pid: _ } => {
            let hello = Response::Hello {
                server_pid: std::process::id(),
                session_token: session_token.to_string(),
            };
            write_response_line(reader.get_mut(), &hello)?;
        }
        _ => {
            write_response_line(
                reader.get_mut(),
                &Response::err_coded(err_code::PROTOCOL, "first message must be hello"),
            )?;
            return Ok(());
        }
    }

    // ── 2. One command ─────────────────────────────────────
    line.clear();
    if read_line_or_eof(&mut reader, &mut line)?.is_none() {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return write_response_line(
                reader.get_mut(),
                &Response::err_coded(err_code::PARSE, format!("parse error: {e}")),
            );
        }
    };
    let req = match req {
        Request::Subscribe { pane_id } => {
            let (sub_id, rx) = begin_subscription(
                reader.get_mut(),
                &event_bus,
                pane_id,
                &peer_subscriptions,
                &command_tx,
            )?;
            return stream_events(
                reader.into_inner(),
                event_bus,
                sub_id,
                rx,
                pane_id,
                peer_subscriptions,
                command_tx,
            );
        }
        other => other,
    };
    let resp = dispatch_request_with_registry(req, &command_tx, Some(&peer_subscriptions));
    write_response_line(reader.get_mut(), &resp)?;
    Ok(())
}

fn begin_subscription<W: Write>(
    sink: &mut W,
    event_bus: &EventBus,
    pane_id: Option<usize>,
    peer_subscriptions: &PeerSubscriptionRegistry,
    command_tx: &Sender<AppCommand>,
) -> Result<(super::events::SubId, std::sync::mpsc::Receiver<Event>)> {
    // Both registrations happen before the ack. Consequently an MCP
    // client's on_ready callback can only publish true while its pane
    // count is already non-zero.
    let (sub_id, rx) = event_bus.subscribe();
    if let Some(pane_id) = pane_id {
        peer_subscriptions.register(pane_id);
    }
    if let Err(e) = write_response_line(sink, &Response::Subscribed) {
        end_subscription(event_bus, sub_id, pane_id, peer_subscriptions, command_tx);
        return Err(e);
    }
    Ok((sub_id, rx))
}

/// Drain events from the bus into the wire until the connection dies
/// or the subscriber is unregistered. The subscription was already
/// registered by `handle_connection` before the Subscribed ack was
/// written, so any event observed from here on is part of the
/// post-ack stream the client can rely on.
///
/// If no real event shows up within [`HEARTBEAT_INTERVAL`], the loop
/// wakes up and writes a [`Event::Heartbeat`] to the wire. Its only
/// purpose is to force an I/O write: if the peer's read side is dead
/// (half-close) and the OS send buffer has filled, the write fails
/// and we unsubscribe promptly instead of holding a stale subscriber
/// slot until the next pane lifecycle event — which may be hours
/// away on a quiet session.
fn stream_events(
    conn: Stream,
    event_bus: EventBus,
    sub_id: super::events::SubId,
    rx: std::sync::mpsc::Receiver<super::Event>,
    pane_id: Option<usize>,
    peer_subscriptions: PeerSubscriptionRegistry,
    command_tx: Sender<AppCommand>,
) -> Result<()> {
    stream_events_inner(conn, rx, HEARTBEAT_INTERVAL);
    end_subscription(
        &event_bus,
        sub_id,
        pane_id,
        &peer_subscriptions,
        &command_tx,
    );
    Ok(())
}

fn end_subscription(
    event_bus: &EventBus,
    sub_id: super::events::SubId,
    pane_id: Option<usize>,
    peer_subscriptions: &PeerSubscriptionRegistry,
    command_tx: &Sender<AppCommand>,
) {
    event_bus.unsubscribe(sub_id);
    if let Some(pane_id) = pane_id {
        peer_subscriptions.unregister(pane_id, command_tx);
    }
}

/// Inner loop split out so tests can drive it with a `Vec<u8>` sink
/// and a sub-second interval.
fn stream_events_inner<W: Write>(
    mut sink: W,
    rx: std::sync::mpsc::Receiver<super::Event>,
    heartbeat_interval: Duration,
) {
    loop {
        let event = match rx.recv_timeout(heartbeat_interval) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Event::Heartbeat {
                ts_ms: now_ms_ipc(),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut json = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(_) => continue,
        };
        json.push('\n');
        if sink.write_all(json.as_bytes()).is_err() || sink.flush().is_err() {
            break;
        }
    }
}

/// How often the subscribe stream emits a keep-alive when idle.
/// 30 s is short enough that a dropped client is released from the
/// subscriber table before it matters, and long enough that chatty
/// heartbeat noise in logs stays negligible.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

fn now_ms_ipc() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_line_or_eof<R: BufRead>(reader: &mut R, buf: &mut String) -> Result<Option<()>> {
    let n = reader.read_line(buf)?;
    Ok(if n == 0 { None } else { Some(()) })
}

fn write_response_line<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    let mut json = serde_json::to_string(resp)?;
    json.push('\n');
    w.write_all(json.as_bytes())?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
fn dispatch_request(req: Request, command_tx: &Sender<AppCommand>) -> Response {
    dispatch_request_with_registry(req, command_tx, None)
}

fn dispatch_request_with_registry(
    req: Request,
    command_tx: &Sender<AppCommand>,
    peer_subscriptions: Option<&PeerSubscriptionRegistry>,
) -> Response {
    match req {
        Request::Hello { .. } => {
            Response::err_coded(err_code::PROTOCOL, "unexpected duplicate hello")
        }
        Request::List => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::List { reply: reply_tx })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(list) => match serde_json::to_value(&list) {
                    Ok(v) => Response::ok_value(v),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize pane list: {e}"))
                    }
                },
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::Send {
            target,
            data,
            append_enter,
        } => forward_unit(command_tx, |reply| AppCommand::Send {
            target,
            data: data.into_bytes(),
            append_enter,
            reply,
        }),
        Request::Focus { target } => {
            forward_unit(command_tx, |reply| AppCommand::Focus { target, reply })
        }
        Request::Close { target } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Close {
                    target,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(closed_id)) => {
                    Response::ok_value(serde_json::json!({ "id": closed_id, "closed": true }))
                }
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::Split {
            target,
            direction,
            command,
            id,
            role,
            cwd,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Split {
                    target,
                    direction,
                    command,
                    name: id,
                    role,
                    cwd,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(outcome)) => Response::ok_value(serde_json::json!({
                    "id": outcome.id,
                    "startup_command": outcome.startup_command,
                })),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::NewTab {
            command,
            id,
            label,
            role,
            cwd,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::NewTab {
                    command,
                    name: id,
                    label,
                    role,
                    cwd,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(outcome)) => Response::ok_value(serde_json::json!({
                    "id": outcome.id,
                    "startup_command": outcome.startup_command,
                })),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        // Subscribe is handled by the connection handler directly — it
        // switches the wire into event-stream mode rather than
        // round-tripping through App commands. If we see it here, the
        // handler called us by mistake; refuse rather than hang.
        Request::Subscribe { .. } => {
            Response::err_coded(err_code::PROTOCOL, "subscribe should be handled inline")
        }
        Request::Inspect {
            target,
            lines,
            include_cursor,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Inspect {
                    target,
                    lines,
                    include_cursor,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(payload)) => Response::ok_value(payload),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::PeerList { from_pane } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::PeerList {
                    from_pane,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(peers)) => match serde_json::to_value(&peers) {
                    Ok(v) => Response::ok_value(v),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize peers: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::PeerSend {
            from_pane,
            target,
            body,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::PeerSend {
                    from_pane,
                    target,
                    body,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(outcome)) => Response::ok_value(serde_json::json!({
                    "delivery": outcome
                })),
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::PeerRegisterClient { pane_id, kind } => {
            forward_unit(command_tx, |reply| AppCommand::PeerRegisterClient {
                pane_id,
                kind,
                reply,
            })
        }
        Request::PeerSetReady {
            pane_id,
            kind,
            ready,
        } => forward_peer_set_ready(command_tx, peer_subscriptions, pane_id, kind, ready),
        Request::SetSummary { from_pane, summary } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::SetSummary {
                    pane_id: from_pane,
                    summary,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(pane)) => match serde_json::to_value(&pane) {
                    Ok(v) => Response::ok_value(serde_json::json!({ "pane": v })),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize pane: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
        Request::SetPaneIdentity { target, name, role } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::SetPaneIdentity {
                    target,
                    name,
                    role,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(pane)) => match serde_json::to_value(&pane) {
                    Ok(v) => Response::ok_value(serde_json::json!({ "pane": v })),
                    Err(e) => {
                        Response::err_coded(err_code::INTERNAL, format!("serialize pane: {e}"))
                    }
                },
                Ok(Err(err)) => err.into_response(),
                Err(e) => {
                    Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}"))
                }
            }
        }
    }
}

fn forward_peer_set_ready(
    command_tx: &Sender<AppCommand>,
    peer_subscriptions: Option<&PeerSubscriptionRegistry>,
    pane_id: usize,
    kind: super::PeerClientKind,
    ready: bool,
) -> Response {
    let (reply_tx, reply_rx) = oneshot::channel();
    let command = AppCommand::PeerSetReady {
        pane_id,
        kind,
        ready,
        reply: reply_tx,
    };

    let send_result = if let Some(registry) = peer_subscriptions {
        let state = registry.state.lock().unwrap_or_else(|p| p.into_inner());
        let managed = state.managed_panes.contains(&pane_id);
        let active = state.counts.get(&pane_id).copied().unwrap_or(0) > 0;
        // Missing subscribe.pane_id means an older peer may own the
        // stream, so preserve the legacy behavior until the pane has
        // participated in the managed protocol. For managed panes,
        // true is valid only while a stream is live. A cooperative
        // false from an old overlapping process must not clear a newer
        // live subscription.
        if managed && ((ready && !active) || (!ready && active)) {
            return Response::ok_unit();
        }
        // Enqueue while the registry lock is held. This serializes the
        // separate readiness connection with stream registration and
        // teardown workers.
        command_tx.send(command)
    } else {
        command_tx.send(command)
    };

    if send_result.is_err() {
        return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
    }
    match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
        Ok(Ok(_)) => Response::ok_unit(),
        Ok(Err(err)) => err.into_response(),
        Err(e) => Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}")),
    }
}

/// Forward a command whose success result is `()` and translate the
/// reply into a [`Response`]. Factored out because three of the four
/// variants share this exact shape.
fn forward_unit(
    command_tx: &Sender<AppCommand>,
    build: impl FnOnce(oneshot::Sender<std::result::Result<(), super::CodedError>>) -> AppCommand,
) -> Response {
    let (reply_tx, reply_rx) = oneshot::channel();
    if command_tx.send(build(reply_tx)).is_err() {
        return Response::err_coded(err_code::SHUTTING_DOWN, "app shutting down");
    }
    match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
        Ok(Ok(_)) => Response::ok_unit(),
        // App-originated error strings pass through uncoded for now —
        // plumbing a stable code through AppCommand replies is a
        // follow-up (see Issue #28 non-goals).
        Ok(Err(err)) => err.into_response(),
        Err(e) => Response::err_coded(err_code::APP_TIMEOUT, format!("app did not respond: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Direction, PaneRef, Request};
    use std::sync::mpsc;

    #[test]
    fn dispatch_list_ok_when_app_replies() {
        // Pretend to be the App: spawn a thread that pulls a List
        // command off the channel and replies with an empty list.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::List { reply }) = rx.recv() {
                reply.send(Vec::new()).unwrap();
            }
        });

        let resp = dispatch_request(Request::List, &tx);
        handle.join().unwrap();

        match resp {
            Response::Ok { data } => {
                // An empty Vec<PaneInfo> serializes to a JSON array.
                assert!(data.is_array(), "expected array, got {data:?}");
                assert_eq!(data.as_array().map(|a| a.len()), Some(0));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_focus_ok_when_app_replies_ok() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply.send(Ok(())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Focused,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_focus_routes_app_coded_error_to_wire() {
        // When the App reply carries a code (new behavior on the
        // AppCommand reply type), the wire Response::Err must
        // surface that same code so clients can match on it.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply
                    .send(Err(super::super::CodedError::new(
                        super::super::err_code::PANE_NOT_FOUND,
                        "pane not found: Id(999)",
                    )))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Id(999),
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { message, code } => {
                assert!(message.contains("pane not found"));
                assert_eq!(
                    code.as_deref(),
                    Some(super::super::err_code::PANE_NOT_FOUND)
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_focus_err_when_app_replies_err() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply.send(Err("pane not found".into())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Id(999),
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { message, .. } => assert!(message.contains("pane not found")),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_split_returns_new_id() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Split { reply, .. }) = rx.recv() {
                reply
                    .send(Ok(SplitOutcome {
                        id: 42,
                        startup_command: Some("cargo test".into()),
                    }))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Split {
                target: PaneRef::Focused,
                direction: Direction::Vertical,
                command: None,
                id: None,
                role: None,
                cwd: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(42));
                assert_eq!(
                    data.get("startup_command").and_then(|v| v.as_str()),
                    Some("cargo test")
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_send_forwards_data_and_enter() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Send {
                data,
                append_enter,
                reply,
                ..
            }) = rx.recv()
            {
                assert_eq!(data, b"hello");
                assert!(append_enter);
                reply.send(Ok(())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Send {
                target: PaneRef::Name("engineering".into()),
                data: "hello".into(),
                append_enter: true,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_new_tab_returns_new_id() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::NewTab { reply, .. }) = rx.recv() {
                reply
                    .send(Ok(SplitOutcome {
                        id: 11,
                        startup_command: Some("cce".into()),
                    }))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::NewTab {
                command: Some("cce".into()),
                id: Some("engineering".into()),
                label: None,
                role: None,
                cwd: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(11));
                assert_eq!(
                    data.get("startup_command").and_then(|v| v.as_str()),
                    Some("cce")
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn forward_unit_shutting_down_is_coded() {
        // Drop the receiver to simulate the App having shut down; the
        // send should fail and the error must carry the SHUTTING_DOWN
        // code, not just a free-form message.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        drop(rx);
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Focused,
            },
            &tx,
        );
        match resp {
            Response::Err { code, .. } => {
                assert_eq!(code.as_deref(), Some(err_code::SHUTTING_DOWN))
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn stream_events_emits_heartbeat_when_idle() {
        use std::io::Cursor;
        use std::sync::mpsc as m;
        let (tx, rx) = m::channel::<Event>();
        // Run the inner loop on a worker with a short heartbeat
        // interval. Sleep long enough to cover multiple intervals
        // with generous slack for slow CI runners (macOS in GHA has
        // been observed not firing inside a tight 150 ms budget).
        let handle = thread::spawn(move || {
            let mut sink = Cursor::new(Vec::<u8>::new());
            stream_events_inner(&mut sink, rx, Duration::from_millis(50));
            sink.into_inner()
        });
        thread::sleep(Duration::from_millis(600));
        drop(tx);
        let bytes = handle.join().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"heartbeat\""), "no heartbeat in {text:?}");
    }

    #[test]
    fn peer_subscription_is_registered_before_subscribed_ack() {
        struct AckObserver {
            registry: PeerSubscriptionRegistry,
            pane_id: usize,
            saw_registered: bool,
        }

        impl Write for AckObserver {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let state = self
                    .registry
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                self.saw_registered = state.counts.get(&self.pane_id) == Some(&1);
                assert!(
                    self.saw_registered,
                    "subscribe.pane_id must be counted before the ack is written"
                );
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = PeerSubscriptionRegistry::default();
        let event_bus = EventBus::new();
        let (command_tx, _command_rx) = mpsc::channel();
        let mut writer = AckObserver {
            registry: registry.clone(),
            pane_id: 7,
            saw_registered: false,
        };

        let (sub_id, _rx) =
            begin_subscription(&mut writer, &event_bus, Some(7), &registry, &command_tx).unwrap();

        assert!(writer.saw_registered);
        end_subscription(&event_bus, sub_id, Some(7), &registry, &command_tx);
    }

    #[test]
    fn failed_subscribed_ack_rolls_back_peer_registration() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "subscriber disconnected before ack",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = PeerSubscriptionRegistry::default();
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = mpsc::channel();

        assert!(begin_subscription(
            &mut FailingWriter,
            &event_bus,
            Some(17),
            &registry,
            &command_tx,
        )
        .is_err());
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(AppCommand::PeerSubscriberGone { pane_id: 17 })
        ));
        let state = registry.state.lock().unwrap_or_else(|p| p.into_inner());
        assert!(!state.counts.contains_key(&17));
    }

    #[test]
    fn generic_subscription_disconnect_does_not_touch_peer_readiness() {
        let registry = PeerSubscriptionRegistry::default();
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = mpsc::channel();
        let mut ack = Vec::new();
        let (sub_id, _rx) =
            begin_subscription(&mut ack, &event_bus, None, &registry, &command_tx).unwrap();

        end_subscription(&event_bus, sub_id, None, &registry, &command_tx);

        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn server_teardown_alone_enqueues_peer_gone() {
        let registry = PeerSubscriptionRegistry::default();
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = mpsc::channel();
        let mut ack = Vec::new();
        let (sub_id, _rx) =
            begin_subscription(&mut ack, &event_bus, Some(8), &registry, &command_tx).unwrap();

        // No PeerSetReady(false) is sent: stream teardown is sufficient.
        end_subscription(&event_bus, sub_id, Some(8), &registry, &command_tx);

        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(AppCommand::PeerSubscriberGone { pane_id: 8 })
        ));
    }

    #[test]
    fn only_last_peer_subscription_disconnect_enqueues_gone() {
        let registry = PeerSubscriptionRegistry::default();
        let (command_tx, command_rx) = mpsc::channel();
        registry.register(9);
        registry.register(9);

        registry.unregister(9, &command_tx);
        assert!(command_rx.try_recv().is_err());

        registry.unregister(9, &command_tx);
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(AppCommand::PeerSubscriberGone { pane_id: 9 })
        ));
    }

    #[test]
    fn zero_notification_precedes_new_ack_and_ready() {
        let registry = PeerSubscriptionRegistry::default();
        registry.register(11);
        let (command_tx, command_rx) = mpsc::channel();
        let (inside_tx, inside_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let old_registry = registry.clone();
        let old_command_tx = command_tx.clone();
        let old = thread::spawn(move || {
            old_registry.unregister_with(11, || {
                inside_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                old_command_tx
                    .send(AppCommand::PeerSubscriberGone { pane_id: 11 })
                    .unwrap();
            });
        });
        inside_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let new_registry = registry.clone();
        let (acked_tx, acked_rx) = mpsc::channel();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let new = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            new_registry.register(11);
            // Models the Subscribed ack: production also sends it only
            // after register returns.
            acked_tx.send(()).unwrap();
        });
        attempted_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            acked_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "new subscription must wait until zero notification is enqueued"
        );

        release_tx.send(()).unwrap();
        old.join().unwrap();
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(AppCommand::PeerSubscriberGone { pane_id: 11 })
        ));
        acked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        new.join().unwrap();

        let responder = thread::spawn(move || match command_rx.recv().unwrap() {
            AppCommand::PeerSetReady {
                pane_id,
                ready,
                reply,
                ..
            } => {
                assert_eq!(pane_id, 11);
                assert!(ready);
                reply.send(Ok(())).unwrap();
            }
            other => panic!("expected readiness after gone, got {other:?}"),
        });
        let response = dispatch_request_with_registry(
            Request::PeerSetReady {
                pane_id: 11,
                kind: super::super::PeerClientKind::Claude,
                ready: true,
            },
            &command_tx,
            Some(&registry),
        );
        assert!(matches!(response, Response::Ok { .. }));
        responder.join().unwrap();
    }

    #[test]
    fn late_ready_after_server_observed_disconnect_is_rejected() {
        let registry = PeerSubscriptionRegistry::default();
        let (command_tx, command_rx) = mpsc::channel();
        registry.register(13);
        registry.unregister(13, &command_tx);
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(AppCommand::PeerSubscriberGone { pane_id: 13 })
        ));

        let response = dispatch_request_with_registry(
            Request::PeerSetReady {
                pane_id: 13,
                kind: super::super::PeerClientKind::Codex,
                ready: true,
            },
            &command_tx,
            Some(&registry),
        );

        assert!(matches!(response, Response::Ok { .. }));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn cooperative_false_does_not_clear_overlapping_live_subscription() {
        let registry = PeerSubscriptionRegistry::default();
        let (command_tx, command_rx) = mpsc::channel();
        registry.register(15);

        let response = dispatch_request_with_registry(
            Request::PeerSetReady {
                pane_id: 15,
                kind: super::super::PeerClientKind::Claude,
                ready: false,
            },
            &command_tx,
            Some(&registry),
        );

        assert!(matches!(response, Response::Ok { .. }));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_refuses_second_hello() {
        // Duplicate hello after handshake should be an error path.
        let (tx, _rx) = mpsc::channel::<AppCommand>();
        let resp = dispatch_request(Request::Hello { client_pid: 1 }, &tx);
        match resp {
            Response::Err { message, .. } => assert!(message.contains("hello")),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_split_forwards_role() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Split { role, reply, .. }) = rx.recv() {
                assert_eq!(role.as_deref(), Some("worker"));
                reply
                    .send(Ok(SplitOutcome {
                        id: 7,
                        startup_command: None,
                    }))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Split {
                target: PaneRef::Focused,
                direction: Direction::Vertical,
                command: None,
                id: None,
                role: Some("worker".into()),
                cwd: None,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_new_tab_forwards_role() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::NewTab { role, reply, .. }) = rx.recv() {
                assert_eq!(role.as_deref(), Some("leader"));
                reply
                    .send(Ok(SplitOutcome {
                        id: 9,
                        startup_command: Some("claude".into()),
                    }))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::NewTab {
                command: None,
                id: None,
                label: None,
                role: Some("leader".into()),
                cwd: None,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_inspect_forwards_payload() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Inspect {
                lines,
                include_cursor,
                reply,
                ..
            }) = rx.recv()
            {
                assert_eq!(lines, Some(3));
                assert!(include_cursor);
                let payload = serde_json::json!({
                    "pane": { "id": 7, "name": "worker-foo" },
                    "screen": { "rows": 24, "cols": 80, "line_start": 21, "line_count": 3 },
                    "lines": [
                        { "row": 21, "text": "" },
                        { "row": 22, "text": "" },
                        { "row": 23, "text": "Allow this tool use? (y/n)" },
                    ],
                    "text": "\n\nAllow this tool use? (y/n)",
                    "cursor": { "visible": true, "row": 23, "col": 0 },
                });
                reply.send(Ok(payload)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Inspect {
                target: PaneRef::Name("worker-foo".into()),
                lines: Some(3),
                include_cursor: true,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(
                    data.get("pane")
                        .and_then(|p| p.get("id"))
                        .and_then(|v| v.as_u64()),
                    Some(7)
                );
                assert_eq!(
                    data.get("cursor")
                        .and_then(|c| c.get("visible"))
                        .and_then(|v| v.as_bool()),
                    Some(true)
                );
                let lines = data.get("lines").and_then(|v| v.as_array()).unwrap();
                assert_eq!(lines.len(), 3);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_close_returns_id_and_closed_flag() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Close { reply, .. }) = rx.recv() {
                reply.send(Ok(13)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Close {
                target: PaneRef::Name("worker-foo".into()),
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(13));
                assert_eq!(data.get("closed").and_then(|v| v.as_bool()), Some(true));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_close_surfaces_last_pane_code() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Close { reply, .. }) = rx.recv() {
                reply
                    .send(Err(super::super::CodedError::new(
                        super::super::err_code::LAST_PANE,
                        "cannot close the last pane of the only tab",
                    )))
                    .unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Close {
                target: PaneRef::Focused,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { code, .. } => {
                assert_eq!(code.as_deref(), Some(super::super::err_code::LAST_PANE));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn drop_removes_unix_socket_file() {
        use std::path::PathBuf;

        // Bind on a unique temp path so the test doesn't race with a
        // real renga instance or other tests.
        let dir = std::env::temp_dir().join(format!(
            "renga-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path: PathBuf = dir.join("renga-test.sock");
        let endpoint = EndpointName::socket(sock_path.clone());

        let (tx, _rx) = mpsc::channel::<AppCommand>();
        let server = IpcServer::spawn(endpoint, tx, "test-token".into(), EventBus::new()).unwrap();

        // Socket file should exist after binding.
        assert!(sock_path.exists(), "socket file not created");

        // Dropping IpcServer should remove it.
        drop(server);
        assert!(!sock_path.exists(), "socket file not removed on drop");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
