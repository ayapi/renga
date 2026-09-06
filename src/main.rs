mod app;
mod claude_monitor;
mod cli;
mod config;
mod conpty_colors;
mod filetree;
mod i18n;
mod input;
mod ipc;
mod layout_config;
mod macos_tip;
mod mcp_peer;
mod pane;
mod preview;
mod ui;
mod version_check;
#[cfg(windows)]
mod win_job;

#[cfg(test)]
pub(crate) static DEBUG_CODEX_PEER_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::io;
use std::panic;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

const PROCESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

struct HeartbeatTracker {
    next_at: Instant,
    frames_since_last: u64,
}

impl HeartbeatTracker {
    fn new(now: Instant) -> Self {
        Self {
            next_at: now + PROCESS_HEARTBEAT_INTERVAL,
            frames_since_last: 0,
        }
    }

    fn record_frame(&mut self) {
        self.frames_since_last = self.frames_since_last.saturating_add(1);
    }

    fn take_due(&mut self, now: Instant) -> Option<u64> {
        if now < self.next_at {
            return None;
        }
        self.next_at = now + PROCESS_HEARTBEAT_INTERVAL;
        Some(std::mem::take(&mut self.frames_since_last))
    }
}

fn executable_identity() -> (Option<std::path::PathBuf>, Option<u128>) {
    let path = std::env::current_exe().ok();
    let modified = path
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis());
    (path, modified)
}

fn log_process_start() {
    let Some(path) = app::codex_peer_debug_log_path() else {
        return;
    };
    let (executable_path, executable_modified_unix_ms) = executable_identity();
    let args: Vec<String> = std::env::args().collect();
    app::append_codex_peer_debug_record(
        &path,
        serde_json::json!({
            "action": "process_start",
            "version": env!("CARGO_PKG_VERSION"),
            "executable_path": executable_path,
            "executable_modified_unix_ms": executable_modified_unix_ms,
            "args_summary": {
                "count": args.len(),
                "has_exec": args.iter().any(|arg| arg == "--exec"),
                "has_layout": args.iter().any(|arg| arg == "--layout"),
            },
        }),
    );
}

fn panic_record(
    message: String,
    thread_name: Option<String>,
    location: Option<(&str, u32, u32)>,
) -> serde_json::Value {
    serde_json::json!({
        "action": "panic",
        "message": message,
        "thread_name": thread_name,
        "location": location.map(|(file, line, column)| serde_json::json!({
            "file": file,
            "line": line,
            "column": column,
        })),
    })
}

fn format_panic_record(info: &panic::PanicHookInfo<'_>) -> serde_json::Value {
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|value| (*value).to_owned())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    let thread_name = std::thread::current().name().map(str::to_owned);
    let location = info
        .location()
        .map(|location| (location.file(), location.line(), location.column()));
    panic_record(message, thread_name, location)
}

fn write_panic_record(record: serde_json::Value) {
    let Some(path) = app::codex_peer_debug_log_path() else {
        return;
    };
    // The writer deliberately contains no panicking operations. A panic hook
    // cannot recover from a second panic, so catch_unwind is not a safeguard.
    app::append_codex_peer_debug_record(&path, record);
}

fn write_panic_record_then(record: serde_json::Value, after_trace: impl FnOnce()) {
    write_panic_record(record);
    after_trace();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventLoopFailureKind {
    EventRead,
    Draw,
    Other,
}

struct EventLoopFailure {
    kind: EventLoopFailureKind,
    error: anyhow::Error,
}

impl EventLoopFailure {
    fn new(kind: EventLoopFailureKind, error: impl Into<anyhow::Error>) -> Self {
        Self {
            kind,
            error: error.into(),
        }
    }
}

fn classify_exit_error(error: &EventLoopFailure) -> &'static str {
    match error.kind {
        EventLoopFailureKind::EventRead => "event_read_error",
        EventLoopFailureKind::Draw => "draw_error",
        EventLoopFailureKind::Other => "error",
    }
}

fn log_process_exit(
    reason: &str,
    error: Option<&anyhow::Error>,
    frames_total: u64,
    uptime: Duration,
) {
    let Some(path) = app::codex_peer_debug_log_path() else {
        return;
    };
    app::append_codex_peer_debug_record(
        &path,
        serde_json::json!({
            "action": "process_exit",
            "reason": reason,
            "error": error.map(|error| format!("{error:#}")),
            "frames_total": frames_total,
            "uptime_ms": uptime.as_millis(),
        }),
    );
}

fn log_heartbeat_if_due(app: &app::App, tracker: &mut HeartbeatTracker, now: Instant) {
    let Some(frames_since_last) = tracker.take_due(now) else {
        return;
    };
    let Some(path) = app::codex_peer_debug_log_path() else {
        return;
    };
    let visible_tab = app.workspaces.get(app.active_tab).map(|workspace| {
        workspace
            .custom_name
            .as_deref()
            .unwrap_or(workspace.name.as_str())
    });
    app::append_codex_peer_debug_record(
        &path,
        serde_json::json!({
            "action": "heartbeat",
            "frames_since_last": frames_since_last,
            "pane_count": app.workspaces.iter().map(|workspace| workspace.panes.len()).sum::<usize>(),
            "visible_tab": visible_tab,
            "trace_write_failures_since_last": app::take_codex_peer_debug_write_failures(),
        }),
    );
}

fn main() -> Result<()> {
    // Internal sidecar mode (`renga __conpty-color-seed <bg> <fg>`): spawned
    // into each pane's ConPTY to seed the console screen buffer colors.
    // Exits the process when requested; must run before clap parsing.
    conpty_colors::run_seed_mode_if_requested();

    // Parse CLI args. clap handles --help / --version and exits cleanly
    // before we enter raw mode below.
    let cli = cli::Cli::parse();
    cli.validate_exec()?;

    // Phase 3: subcommands (`renga list`, `renga send`, …) are IPC
    // clients and MUST be runnable from inside a renga pane — that's
    // the whole point. Dispatch them before the nested-TUI guard kicks
    // in, so the `RENGA=1` env var set by the parent doesn't block
    // legitimate client invocations.
    //
    // `mcp-peer` and `mcp {install,uninstall,status}` are exceptions:
    // the first is a stdio MCP server (not an IPC request) and the
    // second shells out to a client MCP CLI. Route both directly to
    // their handlers before the shared IPC dispatcher.
    if let Some(cmd) = cli.command.as_ref() {
        match cmd {
            cli::IpcCommand::McpPeer => return mcp_peer::run(),
            cli::IpcCommand::Mcp { action } => return mcp_peer::install::run(action),
            _ => return run_ipc_client(cmd),
        }
    }

    // No subcommand: we're about to launch another TUI. Refuse if we're
    // already inside a renga pane, since nesting vt100 parsers in
    // vt100 parsers produces unreadable output and confuses the mouse.
    if std::env::var("RENGA").is_ok() {
        eprintln!("renga: already running inside a renga pane (nested instance not allowed).");
        eprintln!("       Open a new tab with Alt+T or split with Alt+D / Alt+E instead.");
        std::process::exit(1);
    }

    // If a directory is passed as argument, cd into it first
    if let Some(dir) = &cli.dir {
        if dir.is_dir() {
            std::env::set_current_dir(dir)?;
        } else {
            eprintln!("renga: not a directory: {}", dir.display());
            std::process::exit(1);
        }
    }

    run_tui(cli)
}

fn run_tui(cli: cli::Cli) -> Result<()> {
    // Install panic hook to restore terminal state on crash
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        write_panic_record_then(format_panic_record(info), || {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), crossterm::event::DisableMouseCapture);
            let _ = execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            default_hook(info);
        });
    }));

    // Capture the host terminal's OSC 10/11 default colors BEFORE raw mode
    // and the alternate screen, while replies still arrive on our stdin.
    // Pane spawns use them to seed each ConPTY's console color defaults.
    conpty_colors::capture_host_default_colors();

    // Query terminal for graphics protocol support BEFORE raw mode.
    // Falls back to halfblocks if detection fails.
    let image_picker = Some(
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks()),
    );

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    execute!(stdout, crossterm::event::EnableMouseCapture)?;
    execute!(stdout, crossterm::event::EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Get initial terminal size
    let size = terminal.size()?;

    // Phase 3: start the IPC server BEFORE spawning child PTYs so the
    // first `RENGA_SOCKET` value children see is the real one. Children
    // inherit env from this process (via portable-pty's CommandBuilder),
    // and we publish `RENGA` as a "you're inside renga" flag here too.
    let our_pid = std::process::id();
    // Endpoint resolution can fail on Unix if we can't create the
    // owner-only socket directory (read-only FS, permission-constrained
    // mount, …). IPC is non-essential — fall through without it so the
    // TUI still works as a plain multiplexer, mirroring the IpcServer
    // soft-fail path below.
    let ipc_endpoint = match ipc::endpoint::endpoint_for_pid(our_pid) {
        Ok(ep) => Some(ep),
        Err(e) => {
            eprintln!("renga: IPC endpoint unavailable ({e}); external commands disabled.");
            None
        }
    };
    if let Some(ep) = &ipc_endpoint {
        std::env::set_var(ipc::endpoint::ENV_SOCKET, ep.as_str());
    }
    std::env::set_var("RENGA", "1");

    // Session token derived from the process's start nanoseconds so a
    // client connecting through a stale socket file whose PID got
    // re-used cannot be silently fooled — the server echoes this token
    // on hello, and the client verifies it against `RENGA_TOKEN`.
    let session_token = format!(
        "{}-{}",
        our_pid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    // Publish the token before spawning panes so children inherit it.
    std::env::set_var(ipc::endpoint::ENV_TOKEN, &session_token);

    // Load user config + apply CLI override (CLI > file > default).
    let mut user_config = config::Config::load();
    user_config.apply_cli_overrides(config::CliOverrides {
        ime_mode: cli.ime,
        freeze_panes_on_overlay: cli.ime_freeze_panes,
        overlay_catchup_ms: cli.ime_overlay_catchup_ms,
        ui_lang: cli.lang,
        ui_fps: cli.fps,
        ui_file_tree: cli.file_tree_override(),
        shell_program: cli.shell.clone(),
    });
    // Install the pane-shell override before the first pane spawns.
    // Must come after the CLI merge so `--shell` beats `[shell]
    // program`, and before App::new so the initial pane sees it.
    pane::set_shell_override_from_config(user_config.shell.program.as_deref());
    let event_poll_timeout = Duration::from_secs_f64(1.0 / f64::from(user_config.ui.fps));

    // If a layout was requested and its root node is a single pane
    // with an explicit cwd, pre-load the layout so we can spawn the
    // initial pane in that directory instead of the process cwd.
    // Keep the full `apply_layout` call below — this is just a
    // bootstrap detail for the root leaf.
    let preloaded_layout: Option<layout_config::LayoutConfig> = match cli.layout.as_deref() {
        Some(name) => Some(layout_config::LayoutConfig::load(name)?),
        None => None,
    };
    let initial_cwd = preloaded_layout
        .as_ref()
        .and_then(|cfg| cfg.root_pane_cwd())
        .map(|s| {
            let p = std::path::PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(p)
            }
        });

    // Create app (spawns the initial pane, which captures the env above).
    let mut app = app::App::new_with_cwd(size.height, size.width, initial_cwd)?;
    app.apply_config(&user_config);
    app.set_min_pane_size(cli.min_pane_width, cli.min_pane_height);
    app.image_picker = image_picker;

    // First-launch macOS Option-as-Meta tip. Gated on host OS + a
    // zero-byte marker file so returning users never see it twice.
    // Non-macOS hosts and already-dismissed users short-circuit to
    // false here.
    let tip_marker = macos_tip::marker_path();
    if macos_tip::should_show(cli.no_macos_tip, cli.show_macos_tip, tip_marker.as_deref()) {
        app.show_macos_tip(tip_marker);
    }

    // Keep the server handle alive for the process lifetime; its Drop
    // impl cleans up the Unix socket file on exit.
    let _ipc_server = match ipc_endpoint.clone() {
        Some(endpoint) => match ipc::server::IpcServer::spawn(
            endpoint,
            app.command_tx.clone(),
            session_token.clone(),
            app.event_bus.clone(),
        ) {
            Ok(server) => Some(server),
            Err(e) => {
                // IPC is non-essential for the TUI itself — fail soft so users
                // without the required socket permissions can still use renga
                // as a plain multiplexer.
                eprintln!("renga: IPC server failed to start ({e}); external commands disabled.");
                None
            }
        },
        None => None,
    };

    // Phase 1 (--exec): queue the requested command on the initial focused
    // pane. The command will be flushed into the PTY by the main event
    // loop once the shell prompt is ready (see `try_flush_startup`).
    if let Some(cmd) = cli.exec.as_deref() {
        let focused_id = app.ws().focused_pane_id;
        if let Some(pane) = app.ws_mut().panes.get_mut(&focused_id) {
            pane.queue_startup_command(cmd);
        }
    }

    // Phase 2 (--layout): expand a multi-pane layout from a TOML file.
    // Each leaf pane's command (if any) is queued via the same Phase 1
    // mechanism so all panes flush once their shells are ready.
    if let Some(cfg) = preloaded_layout.as_ref() {
        app.apply_layout(cfg)?;
    }

    // Main event loop
    let process_started_at = Instant::now();
    log_process_start();
    let mut frames_total = 0;
    let result = run_event_loop(
        &mut terminal,
        &mut app,
        event_poll_timeout,
        &mut frames_total,
    );

    // Cleanup
    app.shutdown();
    let reason = match &result {
        Err(error) => classify_exit_error(error),
        Ok(()) => "quit_key",
    };
    log_process_exit(
        reason,
        result.as_ref().err().map(|failure| &failure.error),
        frames_total,
        process_started_at.elapsed(),
    );

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture
    )?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste
    )?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result.map_err(|failure| failure.error)
}

/// Handle an IPC subcommand (`renga send …`, `renga list`, etc.).
/// Resolves the endpoint from the `RENGA_SOCKET` env var the parent
/// renga published to its child PTYs; prints the server's response to
/// stdout and exits with a non-zero code on error so shell scripts can
/// branch on it.
fn run_ipc_client(cmd: &cli::IpcCommand) -> Result<()> {
    // `--count 0` on `events` is a degenerate "drain zero events"
    // request. Short-circuit before any environment lookup so the
    // command is a true no-op: it must succeed even when run outside
    // a renga pane (where `RENGA_SOCKET` would be unset).
    if let cli::IpcCommand::Events { count: Some(0), .. } = cmd {
        return Ok(());
    }

    let endpoint = ipc::endpoint::endpoint_from_env()
        .map_err(|e| anyhow::anyhow!("{e}; run this from inside a renga pane"))?;

    // `events` uses the subscription path (long-lived stream), not the
    // single-shot request/response path.
    if let cli::IpcCommand::Events { timeout, count } = cmd {
        return run_events(&endpoint, timeout.map(|d| d.into()), *count);
    }

    let request = cmd.to_request()?;
    let response = ipc::client::send_request(&endpoint, &request)?;
    match response {
        ipc::Response::Ok { data } => {
            // `null` → nothing to print; anything else goes out as
            // pretty JSON so shell scripts can `jq` it. We don't print
            // spurious newlines for empty responses so pipelines stay
            // tight.
            if !data.is_null() {
                let pretty =
                    serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string());
                println!("{pretty}");
            }
            Ok(())
        }
        ipc::Response::Hello { .. } | ipc::Response::Subscribed => {
            // These are handshake replies, never command responses.
            Err(anyhow::anyhow!("unexpected control response to command"))
        }
        ipc::Response::Err { message, code } => {
            if let Some(c) = code {
                Err(anyhow::anyhow!("[{c}] {message}"))
            } else {
                Err(anyhow::anyhow!("{message}"))
            }
        }
    }
}

/// Run `renga events` with optional stop budgets. Bounds the drain so
/// shell callers can poll inside a `/loop` cycle without hanging.
///
/// Architecture: a worker thread holds the subscription and forwards
/// events into a channel; the main thread selects on that channel with
/// a deadline, printing each event and decrementing the count budget
/// as we go. When main returns, the `Receiver` is dropped and the
/// worker's next `tx.send` fails, making its `on_event` callback
/// return `false` so the subscription exits cleanly. The worker may
/// still be blocked in `read_line` at that point; we detach it and
/// let the OS reap on process exit (CLI is short-lived).
fn run_events(
    endpoint: &ipc::endpoint::EndpointName,
    timeout: Option<std::time::Duration>,
    count: Option<usize>,
) -> Result<()> {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // `--count 0` is a degenerate "drain zero events" request; honor it
    // by returning immediately so we never open a connection or spawn
    // a reader for it.
    if let Some(0) = count {
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<ipc::Event>();
    let endpoint_clone = endpoint.clone();
    std::thread::Builder::new()
        .name("renga-events-reader".into())
        .spawn(move || {
            let _ = ipc::client::subscribe_events(&endpoint_clone, |event| tx.send(event).is_ok());
        })
        .map_err(|e| anyhow::anyhow!("spawn events reader: {e}"))?;

    let deadline = timeout.map(|d| Instant::now() + d);
    let mut remaining = count;
    loop {
        let wait = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining_time) => remaining_time,
                None => return Ok(()),
            },
            None => Duration::from_secs(60 * 60 * 24 * 365),
        };
        match rx.recv_timeout(wait) {
            Ok(event) => {
                if let Ok(s) = serde_json::to_string(&event) {
                    println!("{s}");
                    let _ = std::io::stdout().flush();
                }
                if let Some(ref mut n) = remaining {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        return Ok(());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut app::App,
    event_poll_timeout: Duration,
    frames_total: &mut u64,
) -> std::result::Result<(), EventLoopFailure> {
    let mut paste_buffer: Vec<u8> = Vec::new();
    let mut heartbeat = HeartbeatTracker::new(Instant::now());
    app::frame_diagnostics::configure_from_env();

    loop {
        *frames_total = (*frames_total).saturating_add(1);
        heartbeat.record_frame();
        let frame_started_at = Instant::now();
        app::frame_diagnostics::begin_frame(frame_started_at);

        // Drain any PTY output events
        let phase_started_at = app::frame_diagnostics::phase_started();
        app.drain_pty_events();
        app::frame_diagnostics::finish_phase(
            app::frame_diagnostics::PHASE_EVENT_DRAIN,
            phase_started_at,
        );

        // Phase 3: dispatch any commands delivered from the IPC server
        // thread. No-op when the channel is empty, so it's cheap to call
        // every frame.
        let phase_started_at = app::frame_diagnostics::phase_started();
        app.drain_app_commands();
        app::frame_diagnostics::finish_phase(
            app::frame_diagnostics::PHASE_IPC_COMMANDS,
            phase_started_at,
        );

        // Phase 1 (--exec): flush queued startup commands once the shell
        // prompt is observed. This is a no-op for panes without a queued
        // command, so it's safe to run every frame.
        for ws in &mut app.workspaces {
            for pane in ws.panes.values_mut() {
                let _ = pane.try_flush_startup();
            }
        }

        let phase_started_at = app::frame_diagnostics::phase_started();
        app.flush_pending_codex_peer_messages();
        app::frame_diagnostics::finish_phase(
            app::frame_diagnostics::PHASE_CODEX_FLUSH,
            phase_started_at,
        );

        // After paste, wait a few frames for PTY echo to settle
        if app.paste_cooldown > 0 {
            app.paste_cooldown -= 1;
            if app.paste_cooldown == 0 {
                app.dirty = true;
            }
        }

        // After a layout change (split/close/sidebar/terminal resize),
        // wait a few frames so child PTYs can respond to SIGWINCH with
        // a fresh redraw. Prevents the "old buffer at new size" flash.
        if app.resize_cooldown > 0 {
            app.resize_cooldown -= 1;
            if app.resize_cooldown == 0 {
                app.dirty = true;
            }
        }

        // Phase 2 (#37) catch-up: when freeze+catch-up is enabled,
        // periodically force a single repaint so body content stays
        // visible through an open overlay. No-op otherwise.
        app.maybe_tick_overlay_catchup();

        // First-launch macOS tip: hide the banner if it's been up for
        // more than the auto-dismiss budget (~20 s). Persists the
        // marker file via the same path as a key-press dismissal.
        // Cheap no-op when the banner isn't showing.
        app.check_macos_tip_timeout();

        // Only render when something changed (and no cooldown is active)
        if app.dirty && app.paste_cooldown == 0 && app.resize_cooldown == 0 {
            let phase_started_at = app::frame_diagnostics::phase_started();
            app.dirty = false;
            // Defense-in-depth for the Windows conpty caret-leak
            // originally reported in #25 / fixed in #36: while any pane
            // diff paints, ratatui-crossterm emits MoveTo+Print without
            // hiding the hardware cursor, and conpty leaks each MoveTo
            // to Windows Terminal's caret. Windows Terminal anchors IME
            // pre-edit to that host caret, so an intermediate MoveTo on
            // Claude's spinner row can pull native IME composition away
            // from Claude's input row.
            //
            // Force-hide the cursor for the whole draw transaction;
            // ratatui re-shows it only after the frame's final
            // `set_cursor_position` has been applied.
            //
            // Scoped to Windows because conpty is the observed
            // culprit; macOS / Linux terminals don't exhibit the
            // leak, and the gate avoids any unintended side effect.
            //
            // The same conpty path is in play under WSL: there the renga
            // binary is Linux (so `cfg(windows)` is false at compile time)
            // but the outer terminal is still Windows Terminal via conpty,
            // which leaks intermediate MoveTo the same way. Native Linux /
            // macOS terminals don't, so gate the Linux side on a runtime WSL
            // check to avoid changing their behavior.
            if hide_cursor_during_draw() {
                let _ = execute!(terminal.backend_mut(), crossterm::cursor::Hide);
            }
            terminal
                .draw(|frame| {
                    let render_draw_started_at = app::frame_diagnostics::phase_started();
                    ui::render(app, frame);
                    app::frame_diagnostics::finish_phase(
                        app::frame_diagnostics::PHASE_RENDER_DRAW,
                        render_draw_started_at,
                    );
                })
                .map_err(|error| EventLoopFailure::new(EventLoopFailureKind::Draw, error))?;
            // Apply the caret AFTER the draw, while the cursor is still hidden
            // from the pre-draw `Hide`. On conpty, `ui::render` deferred the
            // caret here instead of calling `frame.set_cursor_position`, so
            // ratatui left the frame cursor hidden at frame end rather than
            // re-showing it at its stale post-paint position (the one-frame
            // flicker onto Claude's spinner row, #260). MoveTo first, then
            // Show, so the cursor only becomes visible at its final position.
            // Gated identically to the pre-draw Hide; non-conpty targets keep
            // the original in-frame `set_cursor_position` path (#253).
            if hide_cursor_during_draw() {
                if let Some((x, y)) = app.deferred_caret.take() {
                    let _ = execute!(
                        terminal.backend_mut(),
                        crossterm::cursor::MoveTo(x, y),
                        crossterm::cursor::Show
                    );
                }
            }
            app::frame_diagnostics::finish_phase(
                app::frame_diagnostics::PHASE_RENDER,
                phase_started_at,
            );
        }

        if app.should_quit {
            app::frame_diagnostics::finish_frame(|| {
                app.workspaces[app.active_tab].layout.collect_pane_ids()
            });
            log_heartbeat_if_due(app, &mut heartbeat, Instant::now());
            break;
        }

        // Poll for crossterm events at the configured idle rate.
        if event::poll(event_poll_timeout)
            .map_err(|error| EventLoopFailure::new(EventLoopFailureKind::EventRead, error))?
        {
            match event::read()
                .map_err(|error| EventLoopFailure::new(EventLoopFailureKind::EventRead, error))?
            {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let consumed = app.handle_key_event(key).map_err(|error| {
                        EventLoopFailure::new(EventLoopFailureKind::Other, error)
                    })?;
                    if !consumed {
                        // Collect rapid key events as potential paste
                        if let Some(bytes) = crate::app::key_event_to_bytes_pub(&key) {
                            paste_buffer.extend_from_slice(&bytes);
                            // Drain all immediately available key events (paste burst)
                            while event::poll(Duration::from_millis(1)).map_err(|error| {
                                EventLoopFailure::new(EventLoopFailureKind::EventRead, error)
                            })? {
                                if let Event::Key(k) = event::read().map_err(|error| {
                                    EventLoopFailure::new(EventLoopFailureKind::EventRead, error)
                                })? {
                                    if k.kind == KeyEventKind::Press {
                                        if app.handle_key_event(k).map_err(|error| {
                                            EventLoopFailure::new(
                                                EventLoopFailureKind::Other,
                                                error,
                                            )
                                        })? {
                                            // Shortcut consumed — flush buffer first
                                            if !paste_buffer.is_empty() {
                                                flush_paste_buffer(app, &mut paste_buffer)
                                                    .map_err(|error| {
                                                        EventLoopFailure::new(
                                                            EventLoopFailureKind::Other,
                                                            error,
                                                        )
                                                    })?;
                                            }
                                            break;
                                        }
                                        if let Some(b) = crate::app::key_event_to_bytes_pub(&k) {
                                            paste_buffer.extend_from_slice(&b);
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                            flush_paste_buffer(app, &mut paste_buffer).map_err(|error| {
                                EventLoopFailure::new(EventLoopFailureKind::Other, error)
                            })?;
                        }
                    }
                    app.dirty = true;
                }
                Event::Key(_) => {}
                Event::Paste(text) => {
                    let routed_to_overlay = app.handle_paste(&text).map_err(|error| {
                        EventLoopFailure::new(EventLoopFailureKind::Other, error)
                    })?;
                    if !routed_to_overlay {
                        app.paste_cooldown = 5;
                    }
                    app.dirty = true;
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse_event(mouse);
                    app.dirty = true;
                }
                Event::Resize(cols, rows) => {
                    // Propagate the new terminal size to App so every
                    // pane's PTY gets a prompt SIGWINCH, and hold the
                    // paint for a few frames while the children redraw.
                    app.on_terminal_resize(cols, rows);
                }
                _ => {}
            }
        }

        app::frame_diagnostics::finish_frame(|| {
            app.workspaces[app.active_tab].layout.collect_pane_ids()
        });
        log_heartbeat_if_due(app, &mut heartbeat, Instant::now());
    }

    Ok(())
}

/// Flush accumulated key buffer to PTY. If multiple characters were collected
/// (indicating a paste), wrap in bracketed paste sequences only when the PTY
/// application has enabled the mode. Unconditional wrapping causes shells that
/// haven't opted in to display the escape sequences as literal text (issue #2).
fn flush_paste_buffer(app: &mut app::App, buffer: &mut Vec<u8>) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }

    let focused_id = app.ws().focused_pane_id;
    if let Some(pane) = app.ws_mut().panes.get_mut(&focused_id) {
        pane.scroll_reset();
        pane.clear_codex_transcript_overlay_hint();
        if buffer.len() > 6 {
            if pane.is_bracketed_paste_enabled() {
                let mut data = Vec::with_capacity(buffer.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(buffer);
                data.extend_from_slice(b"\x1b[201~");
                pane.write_input(&data)?;
            } else {
                pane.write_input(buffer)?;
            }
            app.paste_cooldown = 5;
        } else {
            // Normal typing — send directly
            pane.write_input(buffer)?;
        }
    }
    buffer.clear();
    Ok(())
}

/// Whether the hardware cursor must be force-hidden for the whole draw
/// transaction to avoid conpty leaking intermediate `MoveTo` to the host
/// caret (see the call site for the full rationale). Always true on native
/// Windows; on other targets only under WSL, where the outer terminal is
/// still Windows Terminal via conpty.
pub(crate) fn hide_cursor_during_draw() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(windows))]
    {
        is_wsl()
    }
}

/// Detect WSL once and cache the result. WSL exports `WSL_DISTRO_NAME` /
/// `WSL_INTEROP`, and its kernel release string contains "microsoft".
#[cfg(not(windows))]
fn is_wsl() -> bool {
    use std::sync::OnceLock;
    static WSL: OnceLock<bool> = OnceLock::new();
    *WSL.get_or_init(|| {
        if std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
        {
            return true;
        }
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| {
                let s = s.to_ascii_lowercase();
                s.contains("microsoft") || s.contains("wsl")
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        classify_exit_error, flush_paste_buffer, log_heartbeat_if_due, log_process_exit,
        log_process_start, panic_record, write_panic_record_then, EventLoopFailure,
        EventLoopFailureKind, HeartbeatTracker,
    };
    use crate::app::App;
    use std::time::{Duration, Instant};

    struct EnvVarRestore {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn set(value: &std::ffi::OsStr) -> Self {
            let previous = std::env::var_os("RENGA_DEBUG_CODEX_PEER_LOG");
            std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", value);
            Self { previous }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("RENGA_DEBUG_CODEX_PEER_LOG", value),
                None => std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG"),
            }
            crate::app::set_codex_peer_debug_log_path_test_override(Some(None));
        }
    }

    fn lifecycle_test_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "renga-{label}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn heartbeat_clock_only_emits_after_interval_and_resets_frame_count() {
        let started = Instant::now();
        let mut tracker = HeartbeatTracker::new(started);
        tracker.record_frame();
        tracker.record_frame();
        assert_eq!(tracker.take_due(started), None);
        assert_eq!(
            tracker.take_due(started + super::PROCESS_HEARTBEAT_INTERVAL),
            Some(2)
        );
        tracker.record_frame();
        assert_eq!(
            tracker.take_due(started + super::PROCESS_HEARTBEAT_INTERVAL * 2),
            Some(1)
        );
    }

    #[test]
    fn process_exit_quit_record_contains_reason_frames_and_uptime() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = lifecycle_test_path("process-exit");
        let _env = EnvVarRestore::set(path.as_os_str());
        crate::app::set_codex_peer_debug_log_path_test_override(None);
        log_process_exit("quit_key", None, 42, Duration::from_millis(1234));

        let line = std::fs::read_to_string(&path).expect("process exit trace");
        let record: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["action"], "process_exit");
        assert_eq!(record["component"], "tui");
        assert_eq!(record["reason"], "quit_key");
        assert_eq!(record["frames_total"], 42);
        assert_eq!(record["uptime_ms"], 1234);
        std::fs::remove_file(&path).unwrap();
        std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG");
        log_process_exit("quit_key", None, 99, Duration::from_millis(9999));
        assert!(!path.exists());
    }

    #[test]
    fn process_start_production_path_obeys_debug_env_absence() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = lifecycle_test_path("process-start-env");
        let _env = EnvVarRestore::set(path.as_os_str());
        crate::app::set_codex_peer_debug_log_path_test_override(None);
        log_process_start();
        let enabled = std::fs::read_to_string(&path).expect("enabled process start trace");
        assert_eq!(enabled.lines().count(), 1);
        let record: serde_json::Value = serde_json::from_str(enabled.trim()).unwrap();
        assert_eq!(record["action"], "process_start");
        assert_eq!(record["component"], "tui");
        std::fs::remove_file(&path).unwrap();

        std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG");
        log_process_start();
        assert!(!path.exists());
    }

    #[test]
    fn heartbeat_production_path_records_state_and_obeys_env_absence() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = lifecycle_test_path("heartbeat-env");
        let _env = EnvVarRestore::set(path.as_os_str());
        let mut app = App::new(40, 80).expect("App::new");
        let expected_tab = app.workspaces[app.active_tab].name.clone();
        crate::app::set_codex_peer_debug_log_path_test_override(None);
        let started = Instant::now();
        let mut tracker = HeartbeatTracker::new(started);
        tracker.record_frame();
        tracker.record_frame();
        log_heartbeat_if_due(
            &app,
            &mut tracker,
            started + super::PROCESS_HEARTBEAT_INTERVAL,
        );

        let line = std::fs::read_to_string(&path).expect("heartbeat trace");
        assert_eq!(line.lines().count(), 1);
        let record: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["action"], "heartbeat");
        assert_eq!(record["frames_since_last"], 2);
        assert_eq!(record["pane_count"], 1);
        assert_eq!(record["visible_tab"], expected_tab);
        assert_eq!(record["trace_write_failures_since_last"], 0);
        std::fs::remove_file(&path).unwrap();

        std::env::remove_var("RENGA_DEBUG_CODEX_PEER_LOG");
        let mut disabled = HeartbeatTracker::new(started);
        disabled.record_frame();
        log_heartbeat_if_due(
            &app,
            &mut disabled,
            started + super::PROCESS_HEARTBEAT_INTERVAL,
        );
        assert!(!path.exists());
        crate::app::set_codex_peer_debug_log_path_test_override(Some(None));
        app.shutdown();
    }

    #[test]
    fn panic_trace_failure_still_reaches_default_hook_stage() {
        let _guard = crate::DEBUG_CODEX_PEER_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::app::set_codex_peer_debug_log_path_test_override(Some(Some(
            std::env::temp_dir().as_os_str().to_owned(),
        )));
        let reached = std::cell::Cell::new(false);
        write_panic_record_then(
            panic_record("boom".to_owned(), Some("worker".to_owned()), None),
            || reached.set(true),
        );
        crate::app::set_codex_peer_debug_log_path_test_override(Some(None));
        assert!(reached.get());
        let _ = crate::app::take_codex_peer_debug_write_failures();
    }

    #[test]
    fn exit_error_classification_uses_internal_kind_without_rewriting_error() {
        for (kind, expected) in [
            (EventLoopFailureKind::EventRead, "event_read_error"),
            (EventLoopFailureKind::Draw, "draw_error"),
            (EventLoopFailureKind::Other, "error"),
        ] {
            let failure = EventLoopFailure::new(kind, anyhow::anyhow!("original text"));
            assert_eq!(classify_exit_error(&failure), expected);
            assert_eq!(failure.error.to_string(), "original text");
        }
    }

    #[test]
    fn panic_record_formats_message_thread_and_location() {
        let record = panic_record(
            "boom".to_owned(),
            Some("worker".to_owned()),
            Some(("src/example.rs", 12, 34)),
        );
        assert_eq!(record["action"], "panic");
        assert_eq!(record["message"], "boom");
        assert_eq!(record["thread_name"], "worker");
        assert_eq!(record["location"]["file"], "src/example.rs");
        assert_eq!(record["location"]["line"], 12);
        assert_eq!(record["location"]["column"], 34);
    }

    #[test]
    fn flush_paste_buffer_clears_codex_transcript_overlay_hint() {
        let mut app = App::new(40, 80).expect("App::new");
        let focused_id = app.ws().focused_pane_id;
        let pane = app
            .ws_mut()
            .panes
            .get_mut(&focused_id)
            .expect("focused pane exists");
        pane.set_codex_transcript_overlay_hint_for_test(true);

        let mut buffer = b"x".to_vec();
        flush_paste_buffer(&mut app, &mut buffer).expect("flush succeeds");

        let pane = app
            .ws()
            .panes
            .get(&focused_id)
            .expect("focused pane exists");
        assert!(
            !pane.codex_transcript_overlay_hint_for_test(),
            "typing/paste flush must clear transcript fallback state"
        );
        assert!(buffer.is_empty(), "flush should consume the buffered input");
        app.shutdown();
    }
}
