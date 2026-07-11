#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::mem;
use std::time::{Duration, Instant};

type Handle = *mut c_void;

const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const FILE_TYPE_UNKNOWN: u32 = 0x0000;
const FILE_TYPE_DISK: u32 = 0x0001;
const FILE_TYPE_CHAR: u32 = 0x0002;
const FILE_TYPE_PIPE: u32 = 0x0003;
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_WINDOW_INPUT: u32 = 0x0008;
const ENABLE_MOUSE_INPUT: u32 = 0x0010;
const ENABLE_INSERT_MODE: u32 = 0x0020;
const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
const ENABLE_AUTO_POSITION: u32 = 0x0100;
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
const ENABLE_LVB_GRID_WORLDWIDE: u32 = 0x0010;
const WAIT_OBJECT_0: u32 = 0x00000000;
const WAIT_TIMEOUT: u32 = 0x00000102;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0001;
const FILE_SHARE_WRITE: u32 = 0x0002;
const OPEN_EXISTING: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct ConsoleScreenBufferInfoEx {
    cb_size: u32,
    dw_size: Coord,
    dw_cursor_position: Coord,
    w_attributes: u16,
    sr_window: SmallRect,
    dw_maximum_window_size: Coord,
    w_popup_attributes: u16,
    b_fullscreen_supported: i32,
    color_table: [u32; 16],
}

impl Default for ConsoleScreenBufferInfoEx {
    fn default() -> Self {
        Self {
            cb_size: mem::size_of::<Self>() as u32,
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
    fn GetStdHandle(n_std_handle: u32) -> Handle;
    fn GetFileType(h_file: Handle) -> u32;
    fn GetConsoleMode(h_console_handle: Handle, lp_mode: *mut u32) -> i32;
    fn SetConsoleMode(h_console_handle: Handle, mode: u32) -> i32;
    fn WaitForSingleObject(h_handle: Handle, milliseconds: u32) -> u32;
    fn WriteFile(
        h_file: Handle,
        buffer: *const c_void,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn ReadFile(
        h_file: Handle,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetConsoleScreenBufferInfoEx(
        h_console_output: Handle,
        lp_console_screen_buffer_info_ex: *mut ConsoleScreenBufferInfoEx,
    ) -> i32;
    fn SetConsoleScreenBufferInfoEx(
        h_console_output: Handle,
        lp_console_screen_buffer_info_ex: *const ConsoleScreenBufferInfoEx,
    ) -> i32;
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *mut c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: Handle,
    ) -> Handle;
}

#[link(name = "user32")]
extern "system" {
    fn GetConsoleWindow() -> Handle;
}

fn rgb(color_ref: u32) -> (u8, u8, u8) {
    (
        (color_ref & 0xff) as u8,
        ((color_ref >> 8) & 0xff) as u8,
        ((color_ref >> 16) & 0xff) as u8,
    )
}

fn print_console_mode(label: &str, handle: Handle) {
    let mut mode = 0u32;
    let ok = unsafe { GetConsoleMode(handle, &mut mode) };
    if ok == 0 {
        println!("{label}_mode=ERROR {}", std::io::Error::last_os_error());
    } else {
        println!("{label}_mode=0x{mode:08x}");
        print_mode_bits(label, mode);
    }
}

fn print_mode_bits(label: &str, mode: u32) {
    let bits: &[(&str, u32)] = if label == "stdin" {
        &[
            ("ENABLE_PROCESSED_INPUT", ENABLE_PROCESSED_INPUT),
            ("ENABLE_LINE_INPUT", ENABLE_LINE_INPUT),
            ("ENABLE_ECHO_INPUT", ENABLE_ECHO_INPUT),
            ("ENABLE_WINDOW_INPUT", ENABLE_WINDOW_INPUT),
            ("ENABLE_MOUSE_INPUT", ENABLE_MOUSE_INPUT),
            ("ENABLE_INSERT_MODE", ENABLE_INSERT_MODE),
            ("ENABLE_QUICK_EDIT_MODE", ENABLE_QUICK_EDIT_MODE),
            ("ENABLE_EXTENDED_FLAGS", ENABLE_EXTENDED_FLAGS),
            ("ENABLE_AUTO_POSITION", ENABLE_AUTO_POSITION),
            (
                "ENABLE_VIRTUAL_TERMINAL_INPUT",
                ENABLE_VIRTUAL_TERMINAL_INPUT,
            ),
        ][..]
    } else {
        &[
            ("ENABLE_PROCESSED_OUTPUT", ENABLE_PROCESSED_OUTPUT),
            ("ENABLE_WRAP_AT_EOL_OUTPUT", ENABLE_WRAP_AT_EOL_OUTPUT),
            (
                "ENABLE_VIRTUAL_TERMINAL_PROCESSING",
                ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            ),
            ("DISABLE_NEWLINE_AUTO_RETURN", DISABLE_NEWLINE_AUTO_RETURN),
            ("ENABLE_LVB_GRID_WORLDWIDE", ENABLE_LVB_GRID_WORLDWIDE),
        ][..]
    };

    let enabled = bits
        .iter()
        .filter_map(|(name, bit)| (mode & bit != 0).then_some(*name))
        .collect::<Vec<_>>();
    println!("{label}_mode_bits={}", enabled.join("|"));
}

fn file_type_name(handle: Handle) -> &'static str {
    match unsafe { GetFileType(handle) } {
        FILE_TYPE_UNKNOWN => "unknown",
        FILE_TYPE_DISK => "disk",
        FILE_TYPE_CHAR => "char",
        FILE_TYPE_PIPE => "pipe",
        _ => "other",
    }
}

fn print_env(name: &str) {
    println!(
        "{name}={}",
        std::env::var(name).unwrap_or_else(|_| "<unset>".to_string())
    );
}

fn write_all(handle: Handle, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr().cast(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn read_once(handle: Handle, buffer: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0_u8; 256];
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            handle,
            chunk.as_mut_ptr().cast(),
            chunk.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.extend_from_slice(&chunk[..read as usize]);
    Ok(())
}

fn parse_osc_color(buffer: &[u8], slot: u8) -> Option<(u8, u8, u8)> {
    let prefix = format!("\x1B]{slot};");
    let start = buffer
        .windows(prefix.len())
        .position(|window| window == prefix.as_bytes())?;
    let rest = &buffer[start + prefix.len()..];
    let end = rest
        .iter()
        .position(|b| *b == 0x07)
        .or_else(|| rest.windows(2).position(|window| window == b"\x1B\\"))?;
    parse_osc_rgb(std::str::from_utf8(&rest[..end]).ok()?)
}

fn parse_osc_rgb(payload: &str) -> Option<(u8, u8, u8)> {
    let (prefix, values) = payload.trim().split_once(':')?;
    if !prefix.eq_ignore_ascii_case("rgb") && !prefix.eq_ignore_ascii_case("rgba") {
        return None;
    }
    let mut parts = values.split('/');
    let r = parse_osc_component(parts.next()?)?;
    let g = parse_osc_component(parts.next()?)?;
    let b = parse_osc_component(parts.next()?)?;
    Some((r, g, b))
}

fn parse_osc_component(component: &str) -> Option<u8> {
    match component.len() {
        2 => u8::from_str_radix(component, 16).ok(),
        4 => u16::from_str_radix(component, 16)
            .ok()
            .map(|value| (value / 257) as u8),
        _ => None,
    }
}

fn escaped(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| match *b {
            0x1b => "\\e".to_string(),
            0x07 => "\\a".to_string(),
            b'\r' => "\\r".to_string(),
            b'\n' => "\\n".to_string(),
            b if b.is_ascii_graphic() || b == b' ' => (b as char).to_string(),
            b => format!("\\x{b:02x}"),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn codex_like_default_color_probe(input: Handle, output: Handle) {
    let mut original_mode = 0u32;
    let ok = unsafe { GetConsoleMode(input, &mut original_mode) };
    if ok == 0 {
        println!(
            "codex_probe_vt_input=GetConsoleMode ERROR {}",
            io::Error::last_os_error()
        );
        return;
    }

    let requested_mode = original_mode | ENABLE_VIRTUAL_TERMINAL_INPUT;
    let ok = unsafe { SetConsoleMode(input, requested_mode) };
    if ok == 0 {
        println!(
            "codex_probe_vt_input=SetConsoleMode ERROR requested=0x{requested_mode:08x} {}",
            io::Error::last_os_error()
        );
        return;
    }
    println!(
        "codex_probe_vt_input=ok original=0x{original_mode:08x} requested=0x{requested_mode:08x}"
    );

    let mut buffer = Vec::new();
    let write_result = write_all(output, b"\x1B]10;?\x1B\\\x1B]11;?\x1B\\");
    println!("codex_probe_write_osc={write_result:?}");

    if write_result.is_ok() {
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let now = Instant::now();
            if now >= deadline {
                println!("codex_probe_wait=timeout");
                break;
            }
            let timeout_ms = deadline.saturating_duration_since(now).as_millis() as u32;
            match unsafe { WaitForSingleObject(input, timeout_ms) } {
                WAIT_OBJECT_0 => {
                    let before = buffer.len();
                    let read_result = read_once(input, &mut buffer);
                    println!(
                        "codex_probe_read={read_result:?} bytes_added={}",
                        buffer.len() - before
                    );
                    if read_result.is_err() {
                        break;
                    }
                    let fg = parse_osc_color(&buffer, 10);
                    let bg = parse_osc_color(&buffer, 11);
                    if let (Some(fg), Some(bg)) = (fg, bg) {
                        println!(
                            "codex_probe_osc_colors=fg:{},{},{} bg:{},{},{}",
                            fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
                        );
                        break;
                    }
                }
                WAIT_TIMEOUT => {
                    println!("codex_probe_wait=timeout");
                    break;
                }
                other => {
                    println!(
                        "codex_probe_wait=unexpected 0x{other:08x} {}",
                        io::Error::last_os_error()
                    );
                    break;
                }
            }
        }
    }

    if !buffer.is_empty() {
        println!("codex_probe_buffer={}", escaped(&buffer));
    }

    let restore_ok = unsafe { SetConsoleMode(input, original_mode) };
    if restore_ok == 0 {
        println!("codex_probe_restore=ERROR {}", io::Error::last_os_error());
    } else {
        println!("codex_probe_restore=ok");
    }
}

fn color_ref_from_rgb((r, g, b): (u8, u8, u8)) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn parse_hex_rgb(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn open_conout() -> io::Result<Handle> {
    let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
    let handle = unsafe {
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
    if handle.is_null() || handle == (-1isize as Handle) {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

fn print_buffer_summary(label: &str, info: &ConsoleScreenBufferInfoEx) {
    let fg_index = (info.w_attributes & 0x0f) as usize;
    let bg_index = ((info.w_attributes >> 4) & 0x0f) as usize;
    let fg = rgb(info.color_table[fg_index]);
    let bg = rgb(info.color_table[bg_index]);
    println!(
        "{label}_attributes=0x{:04x} fg_index={fg_index} fg_rgb={},{},{} bg_index={bg_index} bg_rgb={},{},{}",
        info.w_attributes, fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
    );
    println!(
        "{label}_window=({},{})->({},{}) size={}x{}",
        info.sr_window.left,
        info.sr_window.top,
        info.sr_window.right,
        info.sr_window.bottom,
        info.dw_size.x,
        info.dw_size.y
    );
}

/// PoC for renga-78w: seed the ConPTY console screen buffer defaults with the
/// host terminal's colors so codex's GetConsoleScreenBufferInfoEx fallback
/// (terminal_probe.rs::query_console_default_colors) decodes them instead of
/// the dark Campbell defaults.
fn run_set_mode(bg_arg: Option<&str>, fg_arg: Option<&str>) {
    let bg = match bg_arg {
        Some(text) => match parse_hex_rgb(text) {
            Some(rgb) => rgb,
            None => {
                eprintln!("set_error=invalid bg color '{text}' (expected rrggbb)");
                std::process::exit(2);
            }
        },
        None => (0xfa, 0xfa, 0xfa),
    };
    let fg = match fg_arg {
        Some(text) => match parse_hex_rgb(text) {
            Some(rgb) => rgb,
            None => {
                eprintln!("set_error=invalid fg color '{text}' (expected rrggbb)");
                std::process::exit(2);
            }
        },
        None => (0x0c, 0x0c, 0x0c),
    };
    println!(
        "set_target fg={},{},{} bg={},{},{}",
        fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
    );

    let conout = match open_conout() {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("set_open_conout=ERROR {err}");
            std::process::exit(1);
        }
    };
    println!("set_open_conout=ok handle={conout:p}");

    let mut info = ConsoleScreenBufferInfoEx::default();
    if unsafe { GetConsoleScreenBufferInfoEx(conout, &mut info) } == 0 {
        eprintln!("set_get=ERROR {}", io::Error::last_os_error());
        std::process::exit(1);
    }
    print_buffer_summary("set_before", &info);

    let fg_index = (info.w_attributes & 0x0f) as usize;
    let bg_index = ((info.w_attributes >> 4) & 0x0f) as usize;
    // Seed the entries the current attributes point at, plus the classic
    // defaults 0 (black bg) and 7 (gray fg) that a fresh SGR-reset state uses,
    // so codex still decodes host colors if attributes change before launch.
    info.color_table[bg_index] = color_ref_from_rgb(bg);
    info.color_table[fg_index] = color_ref_from_rgb(fg);
    info.color_table[0] = color_ref_from_rgb(bg);
    info.color_table[7] = color_ref_from_rgb(fg);

    // SetConsoleScreenBufferInfoEx treats srWindow right/bottom as exclusive,
    // so re-applying the values read back would shrink the window by one cell
    // each call. Compensate before writing.
    info.sr_window.right += 1;
    info.sr_window.bottom += 1;

    if unsafe { SetConsoleScreenBufferInfoEx(conout, &info) } == 0 {
        eprintln!("set_set=ERROR {}", io::Error::last_os_error());
        std::process::exit(1);
    }
    println!("set_set=ok");

    let mut after = ConsoleScreenBufferInfoEx::default();
    if unsafe { GetConsoleScreenBufferInfoEx(conout, &mut after) } == 0 {
        eprintln!("set_verify_get=ERROR {}", io::Error::last_os_error());
        std::process::exit(1);
    }
    print_buffer_summary("set_after", &after);
    for idx in [0usize, 7, fg_index, bg_index] {
        let (r, g, b) = rgb(after.color_table[idx]);
        println!("set_after_color_table[{idx}]={r},{g},{b}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set") {
        run_set_mode(
            args.get(2).map(String::as_str),
            args.get(3).map(String::as_str),
        );
        return;
    }

    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let window = unsafe { GetConsoleWindow() };

    println!("stdin_handle={input:p} type={}", file_type_name(input));
    println!("stdout_handle={output:p} type={}", file_type_name(output));
    println!("console_window={window:p}");
    for name in [
        "WT_SESSION",
        "WT_PROFILE_ID",
        "TERM_PROGRAM",
        "TERM",
        "COLORTERM",
        "FORCE_COLOR",
    ] {
        print_env(name);
    }

    print_console_mode("stdin", input);
    print_console_mode("stdout", output);
    codex_like_default_color_probe(input, output);

    let mut info = ConsoleScreenBufferInfoEx::default();
    let ok = unsafe { GetConsoleScreenBufferInfoEx(output, &mut info) };
    if ok == 0 {
        eprintln!(
            "GetConsoleScreenBufferInfoEx=ERROR {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }

    let fg_index = (info.w_attributes & 0x0f) as usize;
    let bg_index = ((info.w_attributes >> 4) & 0x0f) as usize;
    let fg = rgb(info.color_table[fg_index]);
    let bg = rgb(info.color_table[bg_index]);

    println!("attributes=0x{:04x}", info.w_attributes);
    println!(
        "size={}x{} cursor={},{} window=({},{})->({},{}) max={}x{}",
        info.dw_size.x,
        info.dw_size.y,
        info.dw_cursor_position.x,
        info.dw_cursor_position.y,
        info.sr_window.left,
        info.sr_window.top,
        info.sr_window.right,
        info.sr_window.bottom,
        info.dw_maximum_window_size.x,
        info.dw_maximum_window_size.y
    );
    println!(
        "default_fg_index={} default_fg_rgb={},{},{}",
        fg_index, fg.0, fg.1, fg.2
    );
    println!(
        "default_bg_index={} default_bg_rgb={},{},{}",
        bg_index, bg.0, bg.1, bg.2
    );

    for (idx, color_ref) in info.color_table.iter().enumerate() {
        let (r, g, b) = rgb(*color_ref);
        println!("color_table[{idx}]={r},{g},{b}");
    }
}
