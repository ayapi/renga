//! renga-78w diagnostic harness (Windows-only; no-op stub elsewhere).
//!
//! Reproduces a renga child pane's ConPTY environment without launching the
//! TUI. By default it runs tools/console_color_probe.exe three times inside
//! one ConPTY session:
//!   1. read  — expect the dark Campbell defaults (bug environment)
//!   2. set   — seed the screen buffer ColorTable with host-light colors
//!   3. read  — expect the codex-like fallback to now report the seeded colors
//!
//! Overrides: argv[1] = inner cmd.exe script ({probe} expands to the probe
//! exe path), argv[2] = capture seconds (child killed at the deadline),
//! argv[3] = optional sidecar cmd.exe script spawned into the same session
//! before the main script (e.g. "renga.exe __conpty-color-seed fafafa 0c0c0c").
//!
//! Build the probe first:
//!   rustc tools/console_color_probe.rs -o target/debug/console_color_probe.exe

#[cfg(not(windows))]
fn main() {
    eprintln!("conpty_color_poc is a Windows-only diagnostic harness");
}

#[cfg(windows)]
fn main() {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");

    let probe = std::env::current_dir()
        .expect("cwd")
        .join("target")
        .join("debug")
        .join("console_color_probe.exe");
    let probe = probe.to_str().expect("probe path utf8").to_string();

    let args: Vec<String> = std::env::args().collect();
    let script = match args.get(1) {
        Some(custom) => custom.replace("{probe}", &probe),
        None => {
            assert!(
                std::path::Path::new(&probe).exists(),
                "build console_color_probe.exe first"
            );
            format!(
                "echo ===BEFORE=== && {probe} && echo ===SEED=== && {probe} set fafafa 0c0c0c && echo ===AFTER=== && {probe}"
            )
        }
    };
    let capture_secs: u64 = args
        .get(2)
        .map(|s| s.parse().expect("capture seconds"))
        .unwrap_or(30);

    let sidecar_child = args.get(3).map(|sidecar| {
        let script = sidecar.replace("{probe}", &probe);
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", &script]);
        pair.slave.spawn_command(cmd).expect("sidecar spawn failed")
    });

    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.args(["/c", &script]);

    let mut child = pair.slave.spawn_command(cmd).expect("spawn failed");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(capture_secs);
    let mut out: Vec<u8> = Vec::new();
    loop {
        if child.try_wait().ok().flatten().is_some() {
            while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(300)) {
                out.extend(chunk);
            }
            break;
        }
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            out.extend(chunk);
        }
        if Instant::now() > deadline {
            eprintln!("timeout, killing child");
            let _ = child.kill();
            break;
        }
    }
    if let Some(mut sidecar) = sidecar_child {
        let _ = sidecar.kill();
    }

    println!("{}", String::from_utf8_lossy(&out));
}
