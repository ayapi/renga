//! Repaint-cost probe for renga-pgd: measures how much CPU a renga
//! instance burns while a pane spams output (a) on the active tab and
//! (b) on a background tab after `Alt+T`.
//!
//! With the tab-visibility dirty gate, phase B should cost close to
//! nothing; without it (e.g. the released 1.3.2 binary) phase B stays
//! as expensive as phase A because every PtyOutput repaints the frame.
//!
//! Usage (Windows):
//! ```text
//! cargo run --example repaint_probe -- target\debug\renga.exe
//! ```
//! Prints per-phase CPU percentages for the renga process itself.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

#[cfg(windows)]
mod cpu {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    impl FileTime {
        fn as_u64(self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }

    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
        fn GetProcessTimes(
            h: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    /// Total CPU (kernel + user) consumed by `pid`, in 100ns units.
    pub fn process_cpu_100ns(pid: u32) -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut c = FileTime::default();
            let mut e = FileTime::default();
            let mut k = FileTime::default();
            let mut u = FileTime::default();
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
            CloseHandle(h);
            (ok != 0).then(|| k.as_u64() + u.as_u64())
        }
    }
}

#[cfg(not(windows))]
mod cpu {
    pub fn process_cpu_100ns(_pid: u32) -> Option<u64> {
        // The probe targets the Windows repaint path; extend if the
        // gate ever needs measuring elsewhere.
        None
    }
}

fn measure_phase(label: &str, pid: u32, secs: u64, frame_bytes: &AtomicU64) {
    let before = cpu::process_cpu_100ns(pid);
    let bytes_before = frame_bytes.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(secs));
    let after = cpu::process_cpu_100ns(pid);
    let bytes = frame_bytes.load(Ordering::Relaxed) - bytes_before;
    match (before, after) {
        (Some(b), Some(a)) => {
            let pct = (a - b) as f64 / (secs as f64 * 10_000_000.0) * 100.0;
            let kbps = bytes as f64 / secs as f64 / 1024.0;
            println!("{label}: {pct:.1}% CPU, {kbps:.0} KiB/s frame output over {secs}s");
        }
        _ => println!("{label}: process not measurable"),
    }
}

fn main() {
    let exe = std::env::args()
        .nth(1)
        .expect("usage: repaint_probe <path-to-renga-exe>");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // Synthetic key input does not reach renga in this harness
    // (crossterm's win32-input-mode handshake), so the scenario is
    // driven entirely from inside the pane: `--exec` starts the
    // spammer in the background, and after 14s the pane itself calls
    // `renga new-tab` over IPC (it inherits RENGA_SOCKET/RENGA_TOKEN),
    // which switches focus to a fresh tab and turns the spammer into
    // background-tab output.
    // Absolute path: the pane's cwd is the temp dir, so a relative
    // exe path would not resolve for the in-pane `renga new-tab`.
    let exe_fwd = std::fs::canonicalize(&exe)
        .expect("canonicalize exe")
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_string();
    // Claude-spinner-shaped load: tiny chunks at ~20 Hz. Each chunk
    // triggers a repaint, but vt100 parsing cost stays negligible —
    // isolating the render cost this probe exists to measure. (A
    // full-speed `while true; do echo; done` loop buries the repaint
    // delta under megabytes/s of parser work that both the gated and
    // ungated binaries pay identically on the reader thread.)
    let script = format!(
        "(while true; do echo spin-$RANDOM; sleep 0.05; done &); sleep 14; '{exe_fwd}' new-tab"
    );
    let mut cmd = CommandBuilder::new(&exe);
    cmd.arg("--exec");
    cmd.arg(script);
    cmd.cwd(std::env::temp_dir());
    // The probe may itself be run from inside a renga pane; the child
    // must not refuse to start because of the inherited marker.
    cmd.env_remove("RENGA");

    let mut child = pty.slave.spawn_command(cmd).expect("spawn renga");
    drop(pty.slave);
    let pid = child.process_id().expect("renga pid");
    println!("renga pid: {pid} ({exe})");

    // Drain renga's frame output so a full pipe can't block painting
    // and flatten the very cost we're measuring.
    let mut reader = pty.master.try_clone_reader().expect("clone reader");
    let frame_bytes = Arc::new(AtomicU64::new(0));
    let frame_bytes_reader = Arc::clone(&frame_bytes);
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    frame_bytes_reader.fetch_add(n as u64, Ordering::Relaxed);
                }
                _ => break,
            }
        }
    });
    // Timeline (t=0 spawn): the pane shell prompts and the --exec
    // command flushes within ~2-4s; the spammer runs on the active
    // tab until the in-pane `sleep 14` fires `renga new-tab` at
    // t≈16-18. Phase A must finish before that; phase B starts after.
    std::thread::sleep(Duration::from_secs(6));
    match child.try_wait() {
        Ok(Some(status)) => println!("!! renga exited early: {status:?}"),
        Ok(None) => println!("renga still running after 6s"),
        Err(e) => println!("try_wait error: {e}"),
    }

    // Phase A: spam on the ACTIVE tab — repaints are expected.
    measure_phase("phase A (spam on active tab)", pid, 6, &frame_bytes);

    // Phase B: after the pane's `renga new-tab`, the spammer keeps
    // running on the now-hidden tab 0.
    std::thread::sleep(Duration::from_secs(9));
    measure_phase("phase B (spam on background tab)", pid, 6, &frame_bytes);

    let _ = child.kill();
    let _ = child.wait();
    println!("done");
}
