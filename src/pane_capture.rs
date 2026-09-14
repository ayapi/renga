//! Opt-in raw PTY capture. Producers enqueue typed records; only the writer
//! thread serializes JSON or touches files. No capture resolver runs on readers.
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[cfg(test)]
pub(crate) mod replay;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static FAILURES: AtomicU64 = AtomicU64::new(0);
// Retain shutdown senders even when a pane was dropped while its writer was
// draining. The shutdown guard flushes every writer, including closed panes.
static REGISTRY: Mutex<Vec<Arc<Capture>>> = Mutex::new(Vec::new());
#[cfg(not(test))]
static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) directory: PathBuf,
    pub(crate) origin: Instant,
    pub(crate) origin_unix_ms: u128,
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: std::cell::RefCell<Option<Config>> = const {
        std::cell::RefCell::new(None)
    };
}

fn resolve_config(get_env: impl FnOnce() -> Option<std::ffi::OsString>) -> Option<Config> {
    let directory = PathBuf::from(get_env()?);
    let time = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    Some(Config {
        directory: directory.join(format!(
            "session-{}-{}",
            std::process::id(),
            time.as_nanos()
        )),
        origin: Instant::now(),
        origin_unix_ms: time.as_millis(),
    })
}

/// Called once when entering the TUI, before creating any panes.
pub(crate) fn startup() -> Shutdown {
    #[cfg(not(test))]
    CONFIG.get_or_init(|| resolve_config(|| std::env::var_os("RENGA_DEBUG_PANE_CAPTURE")));
    Shutdown
}

pub(crate) fn enabled() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn failure_count() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

pub(crate) struct Shutdown;
impl Drop for Shutdown {
    fn drop(&mut self) {
        let captures = std::mem::take(&mut *REGISTRY.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = Instant::now() + Duration::from_secs(1);
        for capture in captures {
            capture.flush_until(deadline);
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct DeferredState {
    pub(crate) kind: &'static str,
    pub(crate) opened_at_elapsed_us: Option<u64>,
    pub(crate) buffered_len: usize,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Data {
    Metadata {
        version: u8,
        rows: u16,
        cols: u16,
        origin_unix_ms: u128,
    },
    Read {
        bin_offset: u64,
        read_len: usize,
        deferred: DeferredState,
    },
    Transition {
        action: &'static str,
        kind: &'static str,
        marker: Option<&'static str>,
        bin_offset: u64,
        reason: Option<&'static str>,
    },
    ParserApply {
        bin_offset: u64,
        byte_len: usize,
        applied_offset: u64,
    },
    AppTickRelease {
        released_len: usize,
        deferred: DeferredState,
    },
    AppDraw {
        drawn: bool,
        applied_offset: u64,
        scrollback: usize,
        deferred: DeferredState,
    },
    Resize {
        rows: u16,
        cols: u16,
        clear: bool,
        applied_offset: u64,
    },
    ReaderExit {
        deferred: DeferredState,
    },
}

#[derive(Serialize)]
struct Record {
    sequence: u64,
    timestamp_unix_ms: u128,
    elapsed_us: u64,
    pane_id: usize,
    process_id: u32,
    child_process_id: Option<u32>,
    #[serde(flatten)]
    data: Data,
}

enum Command {
    Record(Record, Option<Vec<u8>>, bool),
    Flush(mpsc::Sender<()>),
    #[cfg(test)]
    Stop(mpsc::Sender<()>),
}

struct State {
    sender: mpsc::Sender<Command>,
    sequence: u64,
    applied_offset: u64,
    deferred: DeferredState,
    flush_pending: bool,
}

pub(crate) struct Capture {
    config: Config,
    pane_id: usize,
    child_process_id: Option<u32>,
    disabled: Arc<AtomicBool>,
    state: Mutex<State>,
    drawn: AtomicBool,
}

impl Capture {
    pub(crate) fn for_pane(
        pane_id: usize,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
    ) -> Option<Arc<Self>> {
        #[cfg(test)]
        let config = TEST_CONFIG.with(|value| value.borrow().clone());
        #[cfg(not(test))]
        let config = CONFIG.get().cloned().flatten();
        config.and_then(|config| Self::create(config, pane_id, child_pid, rows, cols))
    }

    pub(crate) fn create(
        config: Config,
        pane_id: usize,
        child_pid: Option<u32>,
        rows: u16,
        cols: u16,
    ) -> Option<Arc<Self>> {
        let (sender, receiver) = mpsc::channel();
        let disabled = Arc::new(AtomicBool::new(false));
        let worker_disabled = disabled.clone();
        let directory = config.directory.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("capture-{pane_id}"))
            .spawn(move || {
                if write_capture(directory, pane_id, &receiver).is_err() {
                    worker_disabled.store(true, Ordering::Release);
                    FAILURES.fetch_add(1, Ordering::Relaxed);
                }
            });
        if spawn.is_err() {
            FAILURES.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let capture = Arc::new(Self {
            config,
            pane_id,
            child_process_id: child_pid,
            disabled,
            state: Mutex::new(State {
                sender,
                sequence: 0,
                applied_offset: 0,
                deferred: DeferredState {
                    kind: "none",
                    ..Default::default()
                },
                flush_pending: false,
            }),
            drawn: AtomicBool::new(false),
        });
        capture.event(
            capture.config.origin,
            Data::Metadata {
                version: 1,
                rows,
                cols,
                origin_unix_ms: capture.config.origin_unix_ms,
            },
            false,
        );
        REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(capture.clone());
        ACTIVE.store(true, Ordering::Relaxed);
        Some(capture)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !self.disabled.load(Ordering::Acquire)
    }

    pub(crate) fn elapsed_us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.config.origin)
            .as_micros()
            .min(u64::MAX as u128) as u64
    }

    fn send(
        &self,
        state: &mut State,
        at: Instant,
        data: Data,
        bytes: Option<Vec<u8>>,
        flush: bool,
    ) {
        let elapsed_us = self.elapsed_us(at);
        let record = Record {
            sequence: state.sequence,
            timestamp_unix_ms: self.config.origin_unix_ms + u128::from(elapsed_us / 1000),
            elapsed_us,
            pane_id: self.pane_id,
            process_id: std::process::id(),
            child_process_id: self.child_process_id,
            data,
        };
        state.sequence += 1;
        if state
            .sender
            .send(Command::Record(record, bytes, flush))
            .is_err()
        {
            self.disabled.store(true, Ordering::Release);
        }
    }

    pub(crate) fn event(&self, at: Instant, data: Data, flush: bool) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.flush_pending |= flush;
        self.send(&mut state, at, data, None, flush);
    }

    pub(crate) fn read(&self, at: Instant, offset: u64, bytes: &[u8], deferred: DeferredState) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.deferred = deferred.clone();
        self.send(
            &mut state,
            at,
            Data::Read {
                bin_offset: offset,
                read_len: bytes.len(),
                deferred,
            },
            Some(bytes.to_vec()),
            false,
        );
    }

    pub(crate) fn set_deferred(&self, deferred: DeferredState) {
        if self.is_enabled() {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .deferred = deferred;
        }
    }

    /// Caller holds the parser lock across mutation and this record.
    pub(crate) fn applied(&self, byte_len: usize) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let bin_offset = state.applied_offset;
        state.applied_offset += byte_len as u64;
        let applied_offset = state.applied_offset;
        let flush = state.flush_pending;
        self.send(
            &mut state,
            Instant::now(),
            Data::ParserApply {
                bin_offset,
                byte_len,
                applied_offset,
            },
            None,
            flush,
        );
    }

    /// Caller holds the parser lock. Injected resize bytes do not count as PTY bytes.
    pub(crate) fn resize(&self, rows: u16, cols: u16) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let applied_offset = state.applied_offset;
        self.send(
            &mut state,
            Instant::now(),
            Data::Resize {
                rows,
                cols,
                clear: true,
                applied_offset,
            },
            None,
            false,
        );
    }

    pub(crate) fn finish_update(&self) {
        if self.is_enabled() {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .flush_pending = false;
        }
    }

    /// True records are made under the existing render parser lock.
    pub(crate) fn draw(&self, drawn: bool, scrollback: usize) {
        if !self.is_enabled() {
            return;
        }
        if drawn {
            self.drawn.store(true, Ordering::Relaxed);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let data = Data::AppDraw {
            drawn,
            scrollback,
            applied_offset: state.applied_offset,
            deferred: state.deferred.clone(),
        };
        self.send(&mut state, Instant::now(), data, None, false);
    }

    pub(crate) fn finish_draw(&self) {
        if !self.drawn.swap(false, Ordering::Relaxed) {
            self.draw(false, 0);
        }
    }

    fn flush_until(&self, deadline: Instant) {
        if !self.is_enabled() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let _ = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sender
            .send(Command::Flush(sender));
        let _ = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        self.flush_until(Instant::now() + Duration::from_secs(1));
    }
}

fn write_capture(
    directory: PathBuf,
    pane_id: usize,
    receiver: &mpsc::Receiver<Command>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(&directory)?;
    let open = |extension| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("pane-{pane_id}.{extension}")))
    };
    let mut binary = BufWriter::new(open("bin")?);
    let mut jsonl = BufWriter::new(open("jsonl")?);
    let interval = Duration::from_millis(100);
    let mut deadline = Instant::now() + interval;
    loop {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Record(record, bytes, flush)) => {
                if let Some(bytes) = bytes {
                    binary.write_all(&bytes)?;
                }
                serde_json::to_writer(&mut jsonl, &record)?;
                jsonl.write_all(b"\n")?;
                if flush {
                    binary.flush()?;
                    jsonl.flush()?;
                }
            }
            Ok(Command::Flush(done)) => {
                binary.flush()?;
                jsonl.flush()?;
                let _ = done.send(());
            }
            #[cfg(test)]
            Ok(Command::Stop(done)) => {
                binary.flush()?;
                jsonl.flush()?;
                drop(binary);
                drop(jsonl);
                let _ = done.send(());
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            binary.flush()?;
            jsonl.flush()?;
            deadline = Instant::now() + interval;
        }
    }
    binary.flush()?;
    jsonl.flush()
}

#[cfg(test)]
pub(crate) fn with_test_config<T>(config: Option<Config>, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Config>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_CONFIG.with(|value| *value.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TEST_CONFIG.with(|value| value.replace(config)));
    action()
}

#[cfg(test)]
pub(crate) fn test_config(name: &str, origin: Instant) -> Config {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    Config {
        directory: std::env::temp_dir().join(format!(
            "renga-capture-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        origin,
        origin_unix_ms: 1000,
    }
}

#[cfg(test)]
pub(crate) struct TestCaptureCleanup(PathBuf);

#[cfg(test)]
impl TestCaptureCleanup {
    pub(crate) fn new(config: &Config) -> Self {
        assert_eq!(
            config.directory.parent(),
            Some(std::env::temp_dir().as_path())
        );
        assert!(config
            .directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("renga-capture-"));
        Self(config.directory.clone())
    }
}

#[cfg(test)]
impl Drop for TestCaptureCleanup {
    fn drop(&mut self) {
        // Stop only this test's writers before deleting their known scratch path.
        let captures: Vec<_> = {
            let mut registry = REGISTRY.lock().unwrap_or_else(|error| error.into_inner());
            let captures = registry
                .iter()
                .filter(|capture| capture.config.directory == self.0)
                .cloned()
                .collect();
            registry.retain(|capture| capture.config.directory != self.0);
            captures
        };
        for capture in captures {
            capture.disabled.store(true, Ordering::Release);
            let (done, receive) = mpsc::channel();
            let _ = capture
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .sender
                .send(Command::Stop(done));
            let _ = receive.recv_timeout(Duration::from_secs(1));
        }
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_capture_opens_nothing_with_positive_leak_control() {
        let config = test_config("absent", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        assert!(resolve_config(|| None).is_none());
        with_test_config(None, || assert!(Capture::for_pane(1, None, 3, 8).is_none()));
        assert!(!config.directory.exists());
        let capture =
            with_test_config(Some(config.clone()), || Capture::for_pane(1, None, 3, 8)).unwrap();
        capture.flush();
        assert!(config.directory.join("pane-1.bin").exists());
        assert!(
            std::fs::metadata(config.directory.join("pane-1.jsonl"))
                .unwrap()
                .len()
                > 0
        );
    }

    #[test]
    fn draw_records_are_one_per_pane_per_render_including_skips() {
        let config = test_config("draws", Instant::now());
        let _cleanup = TestCaptureCleanup::new(&config);
        let capture = Capture::create(config.clone(), 7, Some(123), 3, 8).unwrap();
        capture.draw(true, 2);
        capture.finish_draw();
        capture.finish_draw();
        capture.flush();
        let lines: Vec<serde_json::Value> =
            std::fs::read_to_string(config.directory.join("pane-7.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1]["drawn"], true);
        assert_eq!(lines[1]["scrollback"], 2);
        assert_eq!(lines[2]["drawn"], false);
        assert_eq!(lines[2]["child_process_id"], 123);
    }
}
