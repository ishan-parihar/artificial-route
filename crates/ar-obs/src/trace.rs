//! JSON tracing: a 128k lossy channel, a dedicated writer thread, 7d rotation.
//!
//! `docs/04`: "`tracing json + crossbeam 128k lossy + dedicated writer`, 7d
//! rotation, `RUST_LOG=warn`". Three decisions worth stating:
//!
//! * **Lossy is the feature.** The emitting side uses `try_send`, never `send`.
//!   A blocking send couples a logging hiccup to request latency; a full buffer
//!   drops a batch instead. An unbounded channel would trade the drop for
//!   unbounded memory, against the "minimal-RAM" budget in `docs/00`.
//! * **Rotation is by day index, not a formatted date.** The filename is
//!   `ar-trace-<days-since-epoch>.jsonl`, so retention is an integer compare and
//!   needs no date-formatting crate and no timezone.
//! * **The writer thread never calls `tracing`.** A write failure inside a
//!   subscriber would re-enter that subscriber and recurse until the stack ran
//!   out, so failures go to stderr.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::error::ObsError;

/// Batches buffered before `emit` starts dropping.
pub const CHANNEL_CAP: usize = 128 * 1024;

/// Days of trace files kept. `docs/04` says 7d.
pub const RETENTION_DAYS: u64 = 7;

const PREFIX: &str = "ar-trace-";
const SUFFIX: &str = ".jsonl";
const SECS_PER_DAY: u64 = 86_400;

/// How often the writer checks for a shutdown it was told about.
///
/// Exiting on channel-disconnect alone is not enough: an installed subscriber
/// holds a `Sender` clone for the life of the process, so `rx` never sees
/// `Disconnected` and a join in `Drop` would hang. Five idle wakeups a second is
/// cheaper than that hang.
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// Days since the Unix epoch. Zero before it rather than an error: a clock set to
/// 1970 must still produce a writable file, not a panic on a request.
fn day_index() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / SECS_PER_DAY)
}

/// Opens `ar-trace-<day>.jsonl` for append.
fn open_day(dir: &Path, day: u64) -> io::Result<File> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{PREFIX}{day}{SUFFIX}")))
}

/// Deletes every trace file past [`RETENTION_DAYS`].
///
/// A name this does not understand is left alone: this is a janitor, and a
/// janitor that deletes what it cannot parse is how a rotation eats something it
/// should not.
fn prune(dir: &Path, today: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = today.saturating_sub(RETENTION_DAYS);
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(digits) = name
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(SUFFIX))
        else {
            continue;
        };
        if let Ok(day) = digits.parse::<u64>()
            && day < cutoff
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Sends formatted event batches to the writer thread, dropping on pressure.
#[derive(Clone, Debug)]
struct TrySendWriter {
    tx: Sender<Vec<u8>>,
    buf: Vec<u8>,
}

impl TrySendWriter {
    /// Hands the batch over, or drops it if the channel is full. `Err` here is
    /// the lossy path working, and neither condition is worth blocking or
    /// panicking a request over.
    fn send(&mut self) {
        if !self.buf.is_empty() {
            let _ = self.tx.try_send(std::mem::take(&mut self.buf));
        }
    }
}

impl Write for TrySendWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send();
        Ok(())
    }
}

impl Drop for TrySendWriter {
    fn drop(&mut self) {
        // The fmt layer flushes after an event, but one dropped mid-event (a
        // panic unwinding through it) would lose the batch. This is the
        // reliable finaliser.
        self.send();
    }
}

impl<'a> MakeWriter<'a> for TrySendWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        Self {
            tx: self.tx.clone(),
            buf: Vec::with_capacity(512),
        }
    }
}

/// A running trace pipeline: the channel, plus the writer thread behind it.
#[derive(Debug)]
pub struct TraceWriter {
    tx: Sender<Vec<u8>>,
    stopped: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl TraceWriter {
    /// Starts the writer thread against `dir`, pruning anything past retention.
    pub fn start(dir: &Path) -> Result<Self, ObsError> {
        fs::create_dir_all(dir)?;
        let today = day_index();
        prune(dir, today);
        let (tx, rx) = bounded::<Vec<u8>>(CHANNEL_CAP);
        let stopped = Arc::new(AtomicBool::new(false));
        let (dir, flag) = (dir.to_path_buf(), Arc::clone(&stopped));
        let join = std::thread::Builder::new()
            .name("ar-trace-writer".to_string())
            .spawn(move || {
                let mut day = today;
                // A day-file that will not open must not kill the thread: there
                // is nowhere to put the batches, but the channel still has to
                // drain or `emit` starts dropping everything.
                let mut file = open_day(&dir, day).ok();
                loop {
                    let chunk = match rx.recv_timeout(SHUTDOWN_POLL) {
                        Ok(chunk) => chunk,
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if flag.load(Ordering::Relaxed) {
                                break;
                            }
                            continue;
                        }
                    };
                    let now = day_index();
                    if now != day {
                        day = now;
                        prune(&dir, day);
                        // Reopening is also the recovery path: a file that failed
                        // to open, or was dropped after a write error, gets
                        // another chance on the next rotation.
                        file = open_day(&dir, day).ok();
                    }
                    // `map_or` ends the borrow of `file` before the error arm
                    // reassigns it.
                    let written = file.as_mut().map_or(Ok(()), |f| f.write_all(&chunk));
                    if let Err(e) = written {
                        eprintln!("ar-obs: trace write failed: {e}; dropping batch");
                        file = None;
                    }
                }
            })?;
        Ok(Self {
            tx,
            stopped,
            join: Some(join),
        })
    }

    /// Installs a JSON subscriber on this writer's channel.
    ///
    /// `RUST_LOG` when set, `warn` otherwise -- the `docs/04` default. On a proxy,
    /// `info` per request is a cost nobody asked for.
    pub fn install(&self) -> Result<(), ObsError> {
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(TrySendWriter {
                        tx: self.tx.clone(),
                        buf: Vec::new(),
                    }),
            )
            .try_init()
            .map_err(|_| ObsError::Subscriber)
    }

    /// Queues one pre-formatted line, dropping it if the buffer is full.
    pub fn emit(&self, line: &str) {
        let _ = self.tx.try_send(line.as_bytes().to_vec());
    }

    /// Batches this writer buffers before dropping.
    #[must_use]
    pub fn channel_cap(&self) -> usize {
        CHANNEL_CAP
    }
}

impl Drop for TraceWriter {
    fn drop(&mut self) {
        // Flag first, then join. Relying on channel-disconnect alone hangs: this
        // struct's own `tx` is alive during `drop`, and an installed subscriber
        // holds a clone for the life of the process.
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
