//! Host-terminal default-color capture and ConPTY color seeding.
//!
//! Problem (Issue: codex input field renders black-on-black in renga panes on
//! Windows Terminal): TUIs like Codex decide light-vs-dark on Windows by
//! querying OSC 10/11 and, when no reply arrives, falling back to
//! `GetConsoleScreenBufferInfoEx`. Inside a pane spawned through the public
//! `kernel32!CreatePseudoConsole` (portable-pty), the OSC query is consumed by
//! ConPTY and never reaches renga, and the console screen buffer defaults are
//! the dark Campbell palette regardless of the host terminal's theme — so on
//! a light host terminal Codex still picks its dark palette and its input
//! field becomes near-invisible. (Windows Terminal itself avoids this only
//! because it bundles a private conhost that answers OSC 10/11.)
//!
//! Fix: before entering raw mode, renga asks the *host* terminal for its real
//! default colors via OSC 10/11 and stores the payloads in
//! `RENGA_OSC10_RESPONSE_PAYLOAD` / `RENGA_OSC11_RESPONSE_PAYLOAD`. For every
//! pane, a short-lived sidecar process (`renga __conpty-color-seed <bg> <fg>`)
//! is spawned into the same ConPTY session; from inside the session it can
//! open `CONOUT$` and rewrite the screen buffer's default ColorTable entries,
//! so the `GetConsoleScreenBufferInfoEx` fallback reports the host's actual
//! colors to any process that probes them later.
//!
//! Knobs (all optional):
//! - `RENGA_FORCE_HOST_OSC_FG` / `RENGA_FORCE_HOST_OSC_BG` — skip the query
//!   and use these payloads (`rrggbb`-style X11 color spec, `rgb:` prefix
//!   allowed). Useful when the host terminal doesn't answer OSC 10/11.
//! - `RENGA_DISABLE_HOST_OSC_COLOR_QUERY=1` — never query the host terminal.
//! - `RENGA_DISABLE_CONPTY_COLOR_SEED=1` — don't spawn the seeding sidecar.
//! - `RENGA_DEBUG_HOST_OSC_COLOR_LOG=path` — write the query outcome.
//! - `RENGA_DEBUG_CONPTY_COLOR_LOG=path` — append sidecar seed diagnostics.

#[cfg(windows)]
use portable_pty::CommandBuilder;
use portable_pty::SlavePty;

/// argv[1] sentinel selecting the sidecar seeding mode. Dispatched at the top
/// of `main()` before clap parsing; not part of the public CLI surface.
pub const SEED_MODE_ARG: &str = "__conpty-color-seed";

pub const OSC10_PAYLOAD_ENV: &str = "RENGA_OSC10_RESPONSE_PAYLOAD";
pub const OSC11_PAYLOAD_ENV: &str = "RENGA_OSC11_RESPONSE_PAYLOAD";

/// 8-bit RGB triple.
type Rgb = (u8, u8, u8);

// ---------------------------------------------------------------------------
// Sidecar entry point
// ---------------------------------------------------------------------------

/// Runs the seeding sidecar and exits the process when invoked as
/// `renga __conpty-color-seed <bg_rrggbb> <fg_rrggbb>`. Call first in
/// `main()`: the sidecar runs inside a pane's ConPTY and must never fall
/// through to TUI startup.
pub fn run_seed_mode_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(SEED_MODE_ARG) {
        return;
    }
    let bg = args.get(2).and_then(|s| parse_rrggbb(s));
    let fg = args.get(3).and_then(|s| parse_rrggbb(s));
    let code = match (bg, fg) {
        (Some(bg), Some(fg)) => seed_console_colors(bg, fg),
        _ => {
            log_seed_result("bad_args");
            2
        }
    };
    std::process::exit(code);
}

fn log_seed_result(line: &str) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("RENGA_DEBUG_CONPTY_COLOR_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "conpty_color_seed,{line}");
        }
    }
}

// ---------------------------------------------------------------------------
// Pane-side: spawn the sidecar into a pane's ConPTY
// ---------------------------------------------------------------------------

/// Spawns the seeding sidecar into the same ConPTY session as the pane's
/// shell. Best-effort: any failure (no captured host colors, current_exe
/// unavailable, spawn error) silently skips seeding — panes keep working with
/// the dark ConPTY defaults, which is the pre-fix behavior. Call after the
/// shell has been spawned so the session always has a long-lived client.
#[cfg(windows)]
pub fn spawn_seed_sidecar(slave: &dyn SlavePty) {
    if std::env::var_os("RENGA_DISABLE_CONPTY_COLOR_SEED").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        return;
    }
    let Some((bg, fg)) = captured_host_colors() else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = CommandBuilder::new(exe);
    cmd.args([
        SEED_MODE_ARG,
        &format!("{:02x}{:02x}{:02x}", bg.0, bg.1, bg.2),
        &format!("{:02x}{:02x}{:02x}", fg.0, fg.1, fg.2),
    ]);
    match slave.spawn_command(cmd) {
        Ok(mut child) => {
            // The sidecar exits within milliseconds; reap it off-thread so
            // pane creation never blocks on it.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(_) => log_seed_result("spawn_failed"),
    }
}

#[cfg(not(windows))]
pub fn spawn_seed_sidecar(_slave: &dyn SlavePty) {}

/// Host default colors captured at startup, as (bg, fg) RGB tuples.
#[cfg_attr(not(windows), allow(dead_code))]
fn captured_host_colors() -> Option<(Rgb, Rgb)> {
    let bg = parse_osc_color_payload(&std::env::var(OSC11_PAYLOAD_ENV).ok()?)?;
    let fg = parse_osc_color_payload(&std::env::var(OSC10_PAYLOAD_ENV).ok()?)?;
    Some((bg, fg))
}

// ---------------------------------------------------------------------------
// Startup: capture the host terminal's default colors
// ---------------------------------------------------------------------------

/// Queries the host terminal for its OSC 10/11 default colors and publishes
/// the payloads via env vars. Must run before raw mode / the alternate
/// screen, while the query reply still arrives on our own stdin.
pub fn capture_host_default_colors() {
    if let Some((fg, bg)) = forced_payloads() {
        std::env::set_var(OSC10_PAYLOAD_ENV, &fg);
        std::env::set_var(OSC11_PAYLOAD_ENV, &bg);
        log_host_capture("forced", Some((fg, bg)));
        return;
    }

    if std::env::var_os("RENGA_DISABLE_HOST_OSC_COLOR_QUERY").is_some() {
        log_host_capture("disabled", None);
        return;
    }

    let result = query_host_osc_colors();
    if let Some((fg, bg)) = result.as_ref() {
        std::env::set_var(OSC10_PAYLOAD_ENV, fg);
        std::env::set_var(OSC11_PAYLOAD_ENV, bg);
    }
    // When the query fails, any payloads inherited from an outer renga are
    // left in place — for a nested renga the outer host colors are still the
    // best available answer.
    log_host_capture("queried", result);
}

fn forced_payloads() -> Option<(String, String)> {
    let fg = forced_payload("RENGA_FORCE_HOST_OSC_FG");
    let bg = forced_payload("RENGA_FORCE_HOST_OSC_BG");
    if fg.is_none() && bg.is_none() {
        return None;
    }
    // When only one side is forced, default the other to near-white so the
    // pair stays parseable; forcing only BG is the common manual override.
    Some((
        fg.unwrap_or_else(|| "0c0c/0c0c/0c0c".to_string()),
        bg.unwrap_or_else(|| "fafa/fafa/fafa".to_string()),
    ))
}

fn forced_payload(name: &str) -> Option<String> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    Some(trimmed.strip_prefix("rgb:").unwrap_or(trimmed).to_string())
}

fn log_host_capture(source: &str, result: Option<(String, String)>) {
    if let Some(path) = std::env::var_os("RENGA_DEBUG_HOST_OSC_COLOR_LOG") {
        let line = match result {
            Some((fg, bg)) => format!("host_osc_color,source={source},fg={fg},bg={bg}\n"),
            None => format!("host_osc_color,source={source},unavailable\n"),
        };
        let _ = std::fs::write(path, line);
    }
}

// ---------------------------------------------------------------------------
// OSC color payload parsing (shared by capture and sidecar)
// ---------------------------------------------------------------------------

/// Parses an X11 color spec as returned by OSC 10/11 (`rgb:RR/GG/BB` with
/// 1–4 hex digits per component; `rgb:` prefix optional because the capture
/// step may already have stripped it) into an 8-bit RGB tuple.
fn parse_osc_color_payload(payload: &str) -> Option<Rgb> {
    let spec = payload.trim();
    let spec = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))
        .unwrap_or(spec);
    let mut parts = spec.split('/');
    let r = parse_color_component(parts.next()?)?;
    let g = parse_color_component(parts.next()?)?;
    let b = parse_color_component(parts.next()?)?;
    Some((r, g, b))
}

/// Scales a 1–4 hex digit X11 color component to 8 bits.
fn parse_color_component(component: &str) -> Option<u8> {
    let len = component.len();
    if !(1..=4).contains(&len) {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (4 * len)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

/// Parses a plain `rrggbb` hex triplet (the sidecar argv format).
fn parse_rrggbb(text: &str) -> Option<Rgb> {
    let hex = text.trim();
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Extracts the payload of an OSC `slot` reply (terminated by BEL or ST) from
/// a raw response buffer.
#[cfg_attr(not(windows), allow(dead_code))]
fn extract_osc_color_payload(bytes: &[u8], slot: u8) -> Option<String> {
    let prefix = match slot {
        10 => b"\x1b]10;".as_slice(),
        11 => b"\x1b]11;".as_slice(),
        _ => return None,
    };
    let start = bytes
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    let payload_start = start + prefix.len();
    let mut i = payload_start;
    while i < bytes.len() {
        if bytes[i] == b'\x07' || (bytes[i] == b'\x1b' && bytes.get(i + 1) == Some(&b'\\')) {
            let text = std::str::from_utf8(&bytes[payload_start..i]).ok()?;
            if text == "?" || text.is_empty() || text.contains('\0') {
                return None;
            }
            return Some(text.to_string());
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Windows implementations
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
fn query_host_osc_colors() -> Option<(String, String)> {
    None
}

#[cfg(not(windows))]
fn seed_console_colors(_bg: Rgb, _fg: Rgb) -> i32 {
    0
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub type Handle = *mut c_void;

    pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    pub const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    pub const ENABLE_LINE_INPUT: u32 = 0x0002;
    pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
    pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    pub const WAIT_OBJECT_0: u32 = 0x00000000;
    pub const WAIT_TIMEOUT: u32 = 0x00000102;
    pub const GENERIC_READ: u32 = 0x8000_0000;
    pub const GENERIC_WRITE: u32 = 0x4000_0000;
    pub const FILE_SHARE_READ: u32 = 0x0001;
    pub const FILE_SHARE_WRITE: u32 = 0x0002;
    pub const OPEN_EXISTING: u32 = 3;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Coord {
        pub x: i16,
        pub y: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct SmallRect {
        pub left: i16,
        pub top: i16,
        pub right: i16,
        pub bottom: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ConsoleScreenBufferInfoEx {
        pub cb_size: u32,
        pub dw_size: Coord,
        pub dw_cursor_position: Coord,
        pub w_attributes: u16,
        pub sr_window: SmallRect,
        pub dw_maximum_window_size: Coord,
        pub w_popup_attributes: u16,
        pub b_fullscreen_supported: i32,
        pub color_table: [u32; 16],
    }

    impl Default for ConsoleScreenBufferInfoEx {
        fn default() -> Self {
            Self {
                cb_size: std::mem::size_of::<Self>() as u32,
                dw_size: Coord::default(),
                dw_cursor_position: Coord::default(),
                w_attributes: 0,
                sr_window: SmallRect::default(),
                dw_maximum_window_size: Coord::default(),
                w_popup_attributes: 0,
                b_fullscreen_supported: 0,
                color_table: [0; 16],
            }
        }
    }

    extern "system" {
        pub fn GetStdHandle(n_std_handle: u32) -> Handle;
        pub fn GetConsoleMode(h_console_handle: Handle, lp_mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h_console_handle: Handle, dw_mode: u32) -> i32;
        pub fn WaitForSingleObject(h_handle: Handle, dw_milliseconds: u32) -> u32;
        pub fn WriteFile(
            h_file: Handle,
            lp_buffer: *const c_void,
            n_number_of_bytes_to_write: u32,
            lp_number_of_bytes_written: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> i32;
        pub fn ReadFile(
            h_file: Handle,
            lp_buffer: *mut c_void,
            n_number_of_bytes_to_read: u32,
            lp_number_of_bytes_read: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> i32;
        pub fn GetConsoleScreenBufferInfoEx(
            h_console_output: Handle,
            lp_info: *mut ConsoleScreenBufferInfoEx,
        ) -> i32;
        pub fn SetConsoleScreenBufferInfoEx(
            h_console_output: Handle,
            lp_info: *const ConsoleScreenBufferInfoEx,
        ) -> i32;
        pub fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: Handle,
        ) -> Handle;
    }
}

/// Sends OSC 10/11 queries to the host terminal and waits briefly for the
/// replies on stdin. Returns `(fg_payload, bg_payload)` with the `rgb:`
/// prefix preserved as sent by the terminal.
#[cfg(windows)]
fn query_host_osc_colors() -> Option<(String, String)> {
    use std::ptr;
    use std::time::{Duration, Instant};
    use win::*;

    unsafe fn get_mode(handle: Handle) -> Option<u32> {
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            None
        } else {
            Some(mode)
        }
    }

    unsafe fn write_all(handle: Handle, data: &[u8]) -> bool {
        let mut written_total = 0usize;
        while written_total < data.len() {
            let mut written = 0u32;
            let ok = WriteFile(
                handle,
                data[written_total..].as_ptr() as *const std::ffi::c_void,
                (data.len() - written_total) as u32,
                &mut written,
                ptr::null_mut(),
            );
            if ok == 0 || written == 0 {
                return false;
            }
            written_total += written as usize;
        }
        true
    }

    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let old_input_mode = unsafe { get_mode(input)? };
    let old_output_mode = unsafe { get_mode(output)? };

    // Read the reply in raw mode: with ENABLE_LINE_INPUT set, a console
    // ReadFile blocks until CR, and an OSC reply contains none — cooked mode
    // could stall startup until the user presses Enter (and ECHO_INPUT would
    // print the reply). Same flag handling as crossterm's enable_raw_mode.
    let raw_input_mode = (old_input_mode
        & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT;
    let _ = unsafe { SetConsoleMode(input, raw_input_mode) };
    let _ = unsafe { SetConsoleMode(output, old_output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };

    let query = b"\x1b]10;?\x07\x1b]11;?\x07";
    if !unsafe { write_all(output, query) } {
        let _ = unsafe { SetConsoleMode(input, old_input_mode) };
        let _ = unsafe { SetConsoleMode(output, old_output_mode) };
        return None;
    }

    let started = Instant::now();
    let timeout = Duration::from_millis(150);
    let mut bytes = Vec::with_capacity(256);
    let mut fg = None;
    let mut bg = None;

    while started.elapsed() < timeout && (fg.is_none() || bg.is_none()) {
        let remaining = timeout.saturating_sub(started.elapsed()).as_millis() as u32;
        let wait = unsafe { WaitForSingleObject(input, remaining.min(25)) };
        if wait == WAIT_TIMEOUT {
            continue;
        }
        if wait != WAIT_OBJECT_0 {
            break;
        }

        let mut buf = [0u8; 256];
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                input,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read as usize]);
        if fg.is_none() {
            fg = extract_osc_color_payload(&bytes, 10);
        }
        if bg.is_none() {
            bg = extract_osc_color_payload(&bytes, 11);
        }
    }

    let _ = unsafe { SetConsoleMode(input, old_input_mode) };
    let _ = unsafe { SetConsoleMode(output, old_output_mode) };

    Some((fg?, bg?))
}

/// Rewrites the ConPTY session's console screen buffer defaults so that the
/// `GetConsoleScreenBufferInfoEx` fallback used by color-probing TUIs reports
/// the host terminal's colors. Runs inside the pane's ConPTY (sidecar mode).
#[cfg(windows)]
fn seed_console_colors(bg: Rgb, fg: Rgb) -> i32 {
    use win::*;

    fn color_ref((r, g, b): Rgb) -> u32 {
        (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
    }

    let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
    let conout = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if conout.is_null() || conout == (-1isize as win::Handle) {
        log_seed_result("open_conout_failed");
        return 1;
    }

    let mut info = ConsoleScreenBufferInfoEx::default();
    if unsafe { GetConsoleScreenBufferInfoEx(conout, &mut info) } == 0 {
        log_seed_result("get_failed");
        return 1;
    }

    let fg_index = (info.w_attributes & 0x0f) as usize;
    let bg_index = ((info.w_attributes >> 4) & 0x0f) as usize;
    // Seed the entries the current attributes point at, plus the classic
    // defaults 0 (black bg) and 7 (gray fg) that an SGR-reset state uses, so
    // a probe still decodes host colors if attributes change before it runs.
    info.color_table[bg_index] = color_ref(bg);
    info.color_table[fg_index] = color_ref(fg);
    info.color_table[0] = color_ref(bg);
    info.color_table[7] = color_ref(fg);

    // SetConsoleScreenBufferInfoEx treats srWindow right/bottom as exclusive,
    // so writing back the values read would shrink the window by one cell in
    // each direction. Compensate before writing.
    info.sr_window.right += 1;
    info.sr_window.bottom += 1;

    if unsafe { SetConsoleScreenBufferInfoEx(conout, &info) } == 0 {
        log_seed_result("set_failed");
        return 1;
    }
    log_seed_result("set_ok");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_four_digit_xparse_payload() {
        assert_eq!(
            parse_osc_color_payload("rgb:fafa/fafa/fafa"),
            Some((250, 250, 250))
        );
        assert_eq!(
            parse_osc_color_payload("0c0c/0c0c/0c0c"),
            Some((12, 12, 12))
        );
    }

    #[test]
    fn parses_short_component_payloads() {
        assert_eq!(parse_osc_color_payload("rgb:ff/80/00"), Some((255, 128, 0)));
        assert_eq!(parse_osc_color_payload("rgb:f/8/0"), Some((255, 136, 0)));
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert_eq!(parse_osc_color_payload(""), None);
        assert_eq!(parse_osc_color_payload("?"), None);
        assert_eq!(parse_osc_color_payload("rgb:zz/00/00"), None);
        assert_eq!(parse_osc_color_payload("rgb:00/00"), None);
        assert_eq!(parse_osc_color_payload("rgb:00000/00/00"), None);
    }

    #[test]
    fn parses_sidecar_argv_hex() {
        assert_eq!(parse_rrggbb("fafafa"), Some((250, 250, 250)));
        assert_eq!(parse_rrggbb("0C0C0C"), Some((12, 12, 12)));
        assert_eq!(parse_rrggbb("fafafa0"), None);
        assert_eq!(parse_rrggbb("gggggg"), None);
    }

    #[test]
    fn extracts_osc_reply_payloads() {
        let bel = b"\x1b]11;rgb:fafa/fafa/fafa\x07";
        assert_eq!(
            extract_osc_color_payload(bel, 11),
            Some("rgb:fafa/fafa/fafa".to_string())
        );
        let st = b"\x1b]10;rgb:0c0c/0c0c/0c0c\x1b\\";
        assert_eq!(
            extract_osc_color_payload(st, 10),
            Some("rgb:0c0c/0c0c/0c0c".to_string())
        );
        // A query echo ("?") is not a reply.
        assert_eq!(extract_osc_color_payload(b"\x1b]11;?\x07", 11), None);
        // Unterminated reply is ignored.
        assert_eq!(extract_osc_color_payload(b"\x1b]11;rgb:aa/bb/cc", 11), None);
    }

    #[test]
    fn captured_colors_roundtrip_through_payload_format() {
        let (bg, fg) = ((250u8, 250u8, 250u8), (12u8, 12u8, 12u8));
        let bg_arg = format!("{:02x}{:02x}{:02x}", bg.0, bg.1, bg.2);
        let fg_arg = format!("{:02x}{:02x}{:02x}", fg.0, fg.1, fg.2);
        assert_eq!(parse_rrggbb(&bg_arg), Some(bg));
        assert_eq!(parse_rrggbb(&fg_arg), Some(fg));
    }
}
