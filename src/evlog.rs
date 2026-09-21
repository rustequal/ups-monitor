//! The utility's operational log.
//!
//! This is the working record, not a debug trace. It answers "what did the UPS
//! and the utility do, and when" — start and stop, power events, notifications,
//! device faults, connection changes, settings changes — with the metric values
//! that mattered at that moment. It is meant to be opened and read by a person.
//!
//! Three rules follow from that, and they are what keep it from turning back
//! into a debug log:
//!
//! * **One event, one line.** No continuation lines, no multi-line dumps. A
//!   line is a complete statement, and the file can be scanned by eye or by
//!   `findstr` without either breaking.
//! * **Only events.** Nothing is written on a timer or per poll. A UPS sitting
//!   on mains for a week produces no lines at all, so the file does not grow
//!   in the absence of anything to report.
//! * **Local wall-clock time.** The user correlating a balloon they saw at
//!   14:32 against this file is reading the clock on their wall, not UTC.
//!
//! The file is named after the executable — `ups-monitor.exe` writes
//! `ups-monitor.log` — and lives beside it. The utility's portability rules
//! apply in full: nothing is written anywhere else, and a directory that
//! cannot be written to disables the log rather than failing the utility.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
/// Cap on the log file. Reached, the file is truncated and restarted rather
/// than rotated: a second file is a second thing to clean up, and this log is
/// read for recent events, not archived. Since only real events are written,
/// reaching this at all takes a very long time.
///
/// A cap, so it is compared with `>=`. The strict comparison this replaces let
/// the file settle one line *past* the number the constant names — small, but
/// a constant called a cap that is exceeded by design is a constant that
/// cannot be trusted at the next reading.
const MAX_BYTES: u64 = 1024 * 1024;

/// How many failed writes in a row turn the log off.
///
/// Not one. A write can fail for reasons that are over by the next event: a
/// backup or an anti-virus scanner holding the file open for a moment, a
/// network drive between reconnects, a disk that is briefly full. Disabling on
/// the first failure meant a single such moment cost every line for the rest of
/// the session — the utility went on running and quietly stopped keeping the
/// record it exists to keep. Five consecutive failures is no longer a moment;
/// each attempt in between reopens the file, so a handle invalidated by a
/// removable drive coming back is recovered from rather than counted against.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Everything the log is: the file, what is known about it, whether it is
/// still being written, whether the debug level is on, and what has already
/// been reported.
///
/// One value with one owner. It was four independent statics — two atomics and
/// two mutexes — that were nonetheless one thing: every one of them is a fact
/// about *this log*, and a second log could not have existed even in a test.
/// That was the real cost. The disable switch is process-wide, so a test that
/// drove a run of write failures turned the log off for every test that ran
/// after it, and the whole file had to be serialised behind a lock to stop the
/// tests clobbering each other.
///
/// The instance below is still a singleton, and that is deliberate rather than
/// residual: a diagnostic line is raised from the poll thread, from the HID
/// layer, and from a window procedure the OS calls with no context of ours,
/// and handing every one of them a reference to a logger would put a parameter
/// on `opt_value`, `draw_text` and `wnd_proc` for the benefit of a line that is
/// usually not even written. What the type buys is that the singleton is now
/// one binding rather than four, and that the logic can be exercised against a
/// log that is not it.
pub(crate) struct Log {
    /// Where lines go. `None` until first use, so a `Log` can be built in a
    /// const context and a test can point one at a directory of its own.
    path: OnceLock<PathBuf>,
    /// Whether the log is still being written. Cleared for the rest of the
    /// session after a run of failed writes.
    enabled: AtomicBool,
    /// Whether `debug` lines are written. Off unless the user selects the
    /// Debug level in Settings.
    debug: AtomicBool,
    /// Diagnostics already reported, so a standing fault is stated without
    /// being repeated on every poll. See [`Repeat`].
    ///
    /// Keyed by the field the line is about, not by the whole message. A single
    /// "last line written" slot was tried first and was wrong in a way that
    /// defeated the entire level: successful transfers are logged too, so the
    /// slot was overwritten between one failure and the next and the filter
    /// compared against something unrelated. Worse, when it did match, a
    /// permanently broken field was reported once at startup and then silenced
    /// for the life of the process — the exact opposite of what someone turns
    /// this level on to see.
    ///
    /// The value is the number of consecutive occurrences the condition has
    /// held. It is reported on the first occurrence and then on a widening
    /// schedule, so a fault that persists for hours leaves a trail proving it
    /// is still happening without writing 1200 identical lines an hour.
    ///
    /// *Occurrences*, not polls, and the distinction is load-bearing rather
    /// than pedantic. Almost every key here is fed from the poll loop, so the
    /// two coincide and the line used to say "polls" — but [`Log::transfer`]
    /// is also reached from the self-test's vendor channel, where a key counts
    /// two control transfers milliseconds apart inside one button press. This
    /// counter cannot tell those apart, and neither can the layer under it:
    /// `RawDevice::get_feature` is the same primitive on both paths and would
    /// have to be handed a parameter that exists only to word a log line. So
    /// the count states what it actually knows.
    seen: Mutex<BTreeMap<String, Repeat>>,
    /// The file and everything the writer knows about it. See [`Sink`].
    sink: Mutex<Sink>,
}

impl Log {
    /// A log that has written nothing yet, at the default path.
    const fn new() -> Self {
        Self {
            path: OnceLock::new(),
            enabled: AtomicBool::new(true),
            debug: AtomicBool::new(false),
            seen: Mutex::new(BTreeMap::new()),
            sink: Mutex::new(Sink::new()),
        }
    }

    /// A log that writes to `path`, for tests that want to read back what was
    /// written without touching the file this process logs to.
    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        let log = Self::new();
        let _ = log.path.set(path);
        log
    }

    /// Where this log writes.
    ///
    /// `ups-monitor.exe` -> `ups-monitor.log`, alongside the binary. The naming
    /// rule itself lives in `config`, with the INI's — one rule for every file
    /// the utility owns, so a renamed copy keeps them all together and a
    /// `current_exe()` that cannot be read falls back to one spelling rather
    /// than to two that agree by hand.
    fn path(&self) -> &Path {
        self.path.get_or_init(|| crate::config::sibling_file("log"))
    }

    /// Applies the configured log level.
    fn set_debug(&self, on: bool) {
        self.debug.store(on, Ordering::Relaxed);
    }

    /// Whether the debug level is on.
    fn debug_enabled(&self) -> bool {
        self.debug.load(Ordering::Relaxed)
    }

    /// Whether the log has turned itself off after a run of failed writes.
    fn disabled(&self) -> bool {
        !self.enabled.load(Ordering::Relaxed)
    }
}

/// The log this process writes.
static LOG: Log = Log::new();

/// The log file and everything the writer knows about it, as [`Log`] holds it.
///
/// One value under one lock, rather than a handle beside three atomics. The
/// decision a write makes — "the file is at the cap, so empty it and start the
/// count again" — reads three of those and writes two, and three threads write
/// here: the poll thread, the UI thread and the self-test reader. Separate
/// atomics made that sequence interleavable, so two threads could both decide
/// to truncate, and a count could be reset against a file that had not been.
/// Nothing about the file is knowable without the handle anyway, so the fields
/// belong beside it, where the lock that serialises the write serialises them
/// too and the question of memory ordering does not arise.
struct Sink {
    /// The append handle, or `None` before the first line and after a failure.
    file: Option<std::fs::File>,
    /// Size of the file, in bytes: what it held when it was opened, plus
    /// everything written since.
    written: u64,
    /// Size at which the file is next truncated.
    ///
    /// Normally `MAX_BYTES`. Raised by `MAX_BYTES` whenever a truncation is
    /// refused — a file another process holds open cannot be truncated, and
    /// retrying on the very next line achieves nothing but another failed
    /// `open`. Backing the threshold off keeps `written` an honest file size
    /// (setting it to zero after a truncation that did not happen would have
    /// been a lie the cap then acted on) while retrying occasionally rather
    /// than continuously.
    truncate_at: u64,
    /// Consecutive failed writes. Reset by any successful one.
    failures: u32,
}

impl Sink {
    /// The state before the first line is written: no handle, nothing counted.
    const fn new() -> Self {
        Self {
            file: None,
            written: 0,
            truncate_at: MAX_BYTES,
            failures: 0,
        }
    }

    /// Records one successful write, which is what makes the failure count a
    /// count of *consecutive* failures: a line that got through says the
    /// condition the earlier ones reported is over.
    fn note_success(&mut self) {
        self.failures = 0;
    }

    /// Records one failed write: drops the handle so the next line reopens the
    /// file, and turns the log off once the failures have run consecutively for
    /// long enough to be a condition rather than a moment.
    ///
    /// Dropping the handle is half the recovery: a `File` whose device has gone
    /// will keep failing forever, while a fresh `open` on the same path
    /// succeeds the moment the path is writable again.
    fn note_failure(&mut self, enabled: &AtomicBool) {
        self.file = None;
        self.failures += 1;
        if self.failures >= MAX_CONSECUTIVE_FAILURES {
            enabled.store(false, Ordering::Relaxed);
        }
    }
}

/// Applies the configured log level. Called at startup and whenever settings
/// are applied.
pub(crate) fn set_debug(on: bool) {
    LOG.set_debug(on);
}

pub(crate) fn debug_enabled() -> bool {
    LOG.debug_enabled()
}

/// Local wall-clock timestamp, in the time zone Windows is configured for.
///
/// `GetLocalTime` is the whole reason this is not computed from the Unix
/// epoch: converting UTC to local time correctly needs the current DST rule
/// for the active zone, which is exactly what the system call already knows
/// and what a hand-rolled calendar cannot.
fn stamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: `GetLocalTime` takes no arguments and fills a `SYSTEMTIME` it
    // returns by value; there is no pointer and no handle involved.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// `ups-monitor.exe` -> `ups-monitor.log`, alongside the binary.
///
/// The naming rule itself lives in `config`, with the INI's — one rule for
/// every file the utility owns, so a renamed copy keeps them all together and
/// a `current_exe()` that cannot be read falls back to one spelling rather than
/// to two that agree by hand.
///
/// Public because the standing warning names the file: the usual causes of a
/// log that cannot be written — a read-only directory, an installation under
/// Program Files, a removed drive — are only diagnosable if the user can see
/// which file is meant, the same reasoning the settings-file warning follows.
pub(crate) fn path() -> PathBuf {
    LOG.path().to_owned()
}

/// Appends one line.
///
/// A failure drops the file handle so the next line reopens, and counts against
/// [`MAX_CONSECUTIVE_FAILURES`]; reaching that turns the log off for the rest of
/// the session and raises the standing warning the panel shows. The utility
/// itself never stops: a monitoring tool that quits monitoring because it cannot
/// write a log is worse than one that stops writing the log.
impl Log {
    fn write_line(&self, line: &str) {
        if self.disabled() {
            return;
        }
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        if sink.file.is_none() {
            let p = self.path();
            sink.written = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            match OpenOptions::new().create(true).append(true).open(p) {
                Ok(f) => sink.file = Some(f),
                Err(_) => return sink.note_failure(&self.enabled),
            }
        }

        if sink.written >= sink.truncate_at {
            // The file is emptied through a separate, short-lived handle, and the
            // log keeps writing through the append handle it already has.
            //
            // Windows cannot express "append and truncate" on one open — the two
            // access modes are mutually exclusive, and `OpenOptions` rejects the
            // combination — so the previous code resolved it by keeping the
            // *truncating* descriptor: after one truncation the log was written
            // without append semantics, and "the log is only ever appended to"
            // held by convention among the callers rather than by the handle.
            // Emptying the file separately leaves that invariant with the
            // descriptor, where it belongs, and the existing handle then appends
            // from the new end of file.
            match std::fs::File::create(self.path()) {
                Ok(_) => {
                    sink.written = 0;
                    sink.truncate_at = MAX_BYTES;
                }
                // Not a failed write — the line below is still appended to a file
                // that is merely larger than intended, and turning the log off over
                // a cap would throw away the very records it caps for. Only the
                // next attempt is deferred.
                Err(_) => {
                    sink.truncate_at = sink.truncate_at.saturating_add(MAX_BYTES);
                }
            }
        }

        let Some(f) = sink.file.as_mut() else {
            return;
        };
        if f.write_all(line.as_bytes()).is_err() {
            return sink.note_failure(&self.enabled);
        }
        let _ = f.flush();
        sink.written = sink.written.saturating_add(line.len() as u64);
        sink.note_success();
    }
}

/// Whether the log has turned itself off after repeated write failures.
///
/// Polled rather than pushed: this module is called from the poll thread, from
/// the HID layer and from the UI thread, and none of them holds anything it
/// could notify. The main loop asks once per pass and raises the standing
/// warning, which is where a condition that is still true belongs.
pub(crate) fn disabled() -> bool {
    LOG.disabled()
}

/// Event categories. Fixed width in the output so the columns line up and the
/// file can be read down a column rather than parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cat {
    /// Utility started or stopped.
    Session,
    /// UPS connected, disconnected, identified.
    Device,
    /// Mains lost or restored, battery low, fault, overload.
    Power,
    /// A balloon was shown to the user, or suppressed by settings.
    Notify,
    /// The user changed a setting, or the UPS beeper was switched.
    Config,
    /// The utility could not do something it was asked to do.
    Error,
    /// Why a value is missing, written only at the Debug level.
    ///
    /// Its own category rather than `ERROR`: a field that failed to read is
    /// not something the utility failed at — the panel handles it correctly by
    /// showing a dash — and mixing the two would make `findstr ERROR` return
    /// the ordinary noise of a device that drops a field now and then.
    Debug,
}

impl Cat {
    fn tag(self) -> &'static str {
        match self {
            Cat::Session => "SESSION",
            Cat::Device => "DEVICE",
            Cat::Power => "POWER",
            Cat::Notify => "NOTIFY",
            Cat::Config => "CONFIG",
            Cat::Error => "ERROR",
            Cat::Debug => "DEBUG",
        }
    }
}

/// Writes one event. `text` must be a single line; newlines are replaced so a
/// stray one in a translated string cannot break the one-event-one-line rule.
pub(crate) fn event(cat: Cat, text: &str) {
    LOG.event(cat, text);
}

impl Log {
    fn event(&self, cat: Cat, text: &str) {
        let text = if text.contains(['\n', '\r']) {
            text.replace(['\n', '\r'], " ")
        } else {
            text.to_owned()
        };
        // CRLF, matching the INI writer. The log is opened in Notepad and grepped
        // with `findstr` on the machines this runs on, and both expect Windows
        // line endings; a bare LF makes Notepad render the whole file as one line.
        self.write_line(&format!("{} {:<7} {}\r\n", stamp(), cat.tag(), text));
    }
}

/// What has been reported so far about one throttle key.
///
/// The button set used to live in a second map keyed identically to this one.
/// Two stores with one lifetime is two clearing rules that can disagree, and
/// they did: the recovery path removed the count and left the set behind, so a
/// flag that failed, recovered, and failed again with the same usages was
/// judged unchanged and throttled as a standing condition instead of being
/// written as the fresh event it was. Held together, a key is forgotten in one
/// operation and cannot be half-forgotten.
#[derive(Default)]
struct Repeat {
    /// Consecutive occurrences this condition has held, counted from 1.
    count: u64,
    /// Last button set reported for this key, for the keys that carry one.
    ///
    /// `debug_buttons` throttles on a stable key (one entry per flag) but must
    /// still re-report when the *set* of usages in the report changes — that
    /// shift is the diagnostic event. Remembering the last set makes the
    /// comparison possible with a bounded map, instead of folding the whole set
    /// into the throttle key, which grew this map without limit over a long
    /// session with a flapping report.
    last_set: Option<String>,
}

/// Whether a repetition of `key` should be written this time.
///
/// Reported on occurrences 1, 2, 5, 10, then every 20th. The early ones matter
/// because the difference between "failed once" and "failing every poll" is
/// the difference between a glitch and a defect, and it has to be visible
/// within seconds of turning the level on rather than after a hundred polls.
fn should_report(count: u64) -> bool {
    matches!(count, 1 | 2 | 5 | 10) || count % 20 == 0
}

/// Writes a diagnostic line, throttling repetitions of the same condition.
///
/// `key` names what the line is about — a field, a report, a usage — and is
/// what repetition is judged on. `text` is the line itself and may carry
/// varying detail (timings, byte counts) without defeating the throttle.
///
/// Suppression is never silent about itself: the count is printed, so a line
/// saying a field has now failed 200 times running is visibly a standing
/// condition rather than a fresh event.
///
/// Both arguments are [`fmt::Arguments`], which is what makes the level free
/// when it is off. `format_args!` captures the pieces by reference and defers
/// the join; the caller therefore pays nothing until this function decides the
/// line will actually be written. That is a type-level guarantee rather than a
/// convention: a `String` built by `format!` does not coerce to
/// `fmt::Arguments`, so a call site cannot allocate eagerly and still compile.
/// It had to be, because these are the per-poll diagnostics — twenty-one
/// successful field reads a cycle each built two strings and dropped them
/// unread on every machine whose owner never opened Settings.
pub(crate) fn debug_repeating(key: fmt::Arguments<'_>, text: fmt::Arguments<'_>) {
    LOG.repeating(key, text);
}

impl Log {
    fn repeating(&self, key: fmt::Arguments<'_>, text: fmt::Arguments<'_>) {
        if !self.debug_enabled() {
            return;
        }
        let count = {
            let Ok(mut map) = self.seen.lock() else {
                return;
            };
            let seen = map.entry(key.to_string()).or_default();
            seen.count += 1;
            seen.count
        };
        if !should_report(count) {
            return;
        }
        if count == 1 {
            self.event(Cat::Debug, &text.to_string());
        } else {
            self.event(Cat::Debug, &format!("{text} [x{count} in a row]"));
        }
    }
}

/// Records that `key` is working again, and reports the recovery if it had
/// been failing.
///
/// This is the half the single-slot filter could never provide. A fault that
/// stops is as diagnostic as one that starts: it says the condition was
/// intermittent rather than permanent, which decides whether to suspect the
/// cable or the descriptor. Without it the file just stops mentioning the
/// field, which reads identically to the utility having given up on it.
/// Lazy in both arguments for the same reason as [`debug_repeating`]: this one
/// is called on *every successful* field read, which is the hot path itself.
pub(crate) fn debug_recovered(key: fmt::Arguments<'_>, text: fmt::Arguments<'_>) {
    LOG.recovered(key, text);
}

impl Log {
    fn recovered(&self, key: fmt::Arguments<'_>, text: fmt::Arguments<'_>) {
        if !self.debug_enabled() {
            return;
        }
        let previous = {
            let Ok(mut map) = self.seen.lock() else {
                return;
            };
            map.remove(&key.to_string())
        };
        if let Some(seen) = previous {
            let count = seen.count;
            self.event(
                Cat::Debug,
                &format!("{text} after {count} failed attempt(s)"),
            );
        }
    }
}

/// Which side a failed transfer implicates, named in the log line.
///
/// The question "is this the device or is this us" is the first one asked of
/// any of these lines, and answering it needs a table lookup the reader
/// should not have to do. `ERROR_GEN_FAILURE` from a HID feature transfer is
/// the device refusing or not answering; nothing about the request is
/// malformed, and Windows says so by using the generic code rather than a
/// parameter error. `ERROR_INVALID_PARAMETER` and
/// `ERROR_INSUFFICIENT_BUFFER` are the opposite: the call was built wrongly
/// here. `ERROR_DEVICE_NOT_CONNECTED` and `ERROR_FILE_NOT_FOUND` mean the
/// handle no longer refers to anything, which is a cable or a removal.
fn blame(code: u32) -> &'static str {
    // Win32 codes arrive HRESULT-wrapped as 0x8007xxxx; the low 16 bits are
    // the original error.
    match code & 0xFFFF {
        // ERROR_GEN_FAILURE
        0x001F => "device did not answer",
        // ERROR_DEVICE_NOT_CONNECTED / ERROR_FILE_NOT_FOUND / ERROR_NO_SUCH_DEVICE
        0x048F | 0x0002 | 0x0433 => "device is gone",
        // ERROR_INVALID_PARAMETER / ERROR_INSUFFICIENT_BUFFER / ERROR_BAD_LENGTH
        0x0057 | 0x007A | 0x0018 => "BAD REQUEST FROM THIS UTILITY",
        // ERROR_SEM_TIMEOUT / ERROR_TIMEOUT
        0x0079 | 0x05B4 => "device timed out",
        // ERROR_ACCESS_DENIED / ERROR_SHARING_VIOLATION
        0x0005 | 0x0020 => "access refused, another process may hold the device",
        // ERROR_BUSY
        0x00AA => "device busy",
        _ => "cause not classified",
    }
}

/// Records one feature transfer: report id, size, elapsed time, and the bytes.
///
/// This is the line that answers "why is there a dash" at the level below the
/// interpretation. `debug` says the field could not be resolved; this says
/// what the device actually put on the wire, so the two can be compared. A
/// report that came back full of plausible bytes while the field failed to
/// resolve is a fault in this code's map of the descriptor; a report that came
/// back all zeroes, or short, or not at all, is the device.
///
/// Successful transfers are logged too, and that is deliberate: the question
/// asked of this level is where the poller's time goes and whether the format
/// is being read correctly, and neither can be answered from failures alone.
/// A poll writes ten of these lines, so the level is only usable in bursts —
/// which is what it is for. The size cap keeps one line one line.
pub(crate) fn debug_transfer(
    rid: u8,
    len: usize,
    elapsed: Option<std::time::Duration>,
    outcome: std::result::Result<&[u8], (u32, String)>,
) {
    LOG.transfer(rid, len, elapsed, outcome);
}

impl Log {
    fn transfer(
        &self,
        rid: u8,
        len: usize,
        elapsed: Option<std::time::Duration>,
        outcome: std::result::Result<&[u8], (u32, String)>,
    ) {
        if !self.debug_enabled() {
            return;
        }
        // An untimed transfer is one that began while the level was off, so there
        // is nothing to report about it: the caller does not read the clock unless
        // the line is going to be written. The two guards are about two different
        // moments, and only a settings change landing inside a single transfer can
        // tell them apart — which is exactly the case that must not produce a line
        // claiming `0 us`.
        let Some(elapsed) = elapsed else {
            return;
        };
        let micros = elapsed.as_micros();
        match outcome {
            Ok(buf) => {
                // A report that failed and is now reading again: say so, and
                // clear the failure counter. This is the line that tells the
                // reader the fault was intermittent — without it the file simply
                // stops mentioning the report, which reads the same as the
                // utility having given up on it.
                debug_recovered(
                    format_args!("fail:{rid}"),
                    format_args!("get_feature rid {rid}: reading again"),
                );
                // Successes are throttled per report id. Ten reports are read
                // every poll and almost all of them succeed, so logging each one
                // unconditionally would bury the failures this level exists to
                // show — 12 000 lines an hour at a 3 s interval, and the 1 MB cap
                // reached in a few hours with the interesting lines already
                // rotated away.
                //
                // Throttled rather than dropped, because the timings are the
                // evidence for the other question this level answers: where the
                // poller's time goes. A periodic sample of each report's latency
                // shows a device slowing down under polling long before it starts
                // refusing, and refusals alone never show that.
                debug_repeating(
                    format_args!("ok:{rid}"),
                    format_args!(
                        "get_feature rid {rid}: ok, {len} bytes in {micros} us, data {}",
                        hex(buf)
                    ),
                );
            }
            // The buffer contents on failure are not reported: HidD_GetFeature
            // leaves it undefined, and printing whatever happens to be in it
            // would put invented data in a file kept for diagnosis.
            Err((code, message)) => {
                // The success counter is dropped without announcement: "stopped
                // succeeding" is what the failure line below already says, and
                // saying it twice per failure would double the noise.
                if let Ok(mut map) = LOG.seen.lock() {
                    map.remove(&format!("ok:{rid}"));
                }
                debug_repeating(
                    format_args!("fail:{rid}"),
                    format_args!(
                        "get_feature rid {rid}: FAILED after {micros} us, \
                     requested {len} bytes, {message} ({code:#010x}) \
                     — {}",
                        blame(code)
                    ),
                );
            }
        }
    }
}

/// Records a feature *write*, so the log accounts for both directions of the
/// feature channel rather than only reads.
///
/// Unlike `debug_transfer`, this is never throttled: a feature write is a rare,
/// deliberate act — the beeper toggle and the self-test gate are the only
/// callers — so every one is worth a line, and there is no per-poll volume to
/// suppress. The written bytes are always shown because, unlike a failed read's
/// undefined buffer, the payload of a write is known and is exactly what a
/// reader diagnosing a refused command needs to see.
pub(crate) fn debug_write(rid: u8, buf: &[u8], outcome: std::result::Result<(), (u32, String)>) {
    LOG.write(rid, buf, outcome);
}

impl Log {
    fn write(&self, rid: u8, buf: &[u8], outcome: std::result::Result<(), (u32, String)>) {
        if !self.debug_enabled() {
            return;
        }
        match outcome {
            Ok(()) => event(
                Cat::Debug,
                &format!(
                    "set_feature rid {rid}: ok, {} bytes, data {}",
                    buf.len(),
                    hex(buf)
                ),
            ),
            Err((code, message)) => event(
                Cat::Debug,
                &format!(
                "set_feature rid {rid}: FAILED, {} bytes, data {}, {message} ({code:#010x}) — {}",
                buf.len(),
                hex(buf),
                blame(code)
            ),
            ),
        }
    }
}

/// Records what one flag read: whether the usage was set, and which usages the
/// report actually carried.
///
/// Called on **every** read, not only on the ones that come back false, and
/// that is the point of the signature. The line below is written only when the
/// flag is false — a flag reading true is the ordinary case and explains
/// nothing — but the *count* behind that line has to know about both, because
/// it says how many readings in a row the flag has been false and a reading in
/// which it was true breaks the row.
///
/// It used to take only the false case, and the caller was trusted to know
/// that the true case was of no interest. It is: to the line. To the counter it
/// is the only thing that ends a run, and the caller had no way to say it. The
/// result, in a real log: four flags of one report, read in one poll and all
/// false since the same instant, reporting 60, 10, 10 and 10 readings in a row
/// — the three agreeing with the clock being the correct ones. The run had
/// survived twelve seconds in which the flag was true, because nothing was
/// told.
///
/// The throttle's own reset — the usage set having moved — cannot stand in for
/// this. The set that follows an episode is very often the one that preceded
/// it: a UPS reads `FullyCharged, ACPresent` on mains, spends a self-test
/// reading `Discharging, ACPresent`, and returns to the first, so the flag that
/// was set throughout is exactly the one whose key sees no change at all.
///
/// Why the line is worth writing when the flag is false: the flag being false
/// is usually the truth and needs no explanation, but the same result appears
/// when the report being parsed is not the one the flag lives in — and from the
/// value alone the two are identical. Listing the usages that *were* set makes
/// the difference visible: a `PresentStatus` report with its normal complement
/// of flags and this one absent is the device saying no, while a report
/// carrying usages from an unrelated collection is a map that resolved the
/// wrong report.
///
/// Nothing is written for a flag that reads true, and it is not needed: every
/// other flag queried on the same report prints the usage list, and the one
/// that went true appears in it. A line of its own would restate what the
/// neighbouring lines already carry, in a file that is read by eye.
pub(crate) fn debug_buttons(page: u16, usage: u16, rid: u8, present: bool, set: &[u16]) {
    LOG.buttons(page, usage, rid, present, set);
}

/// The throttle key one flag's not-set run is counted under.
///
/// Written once because the counting half and the clearing half need the same
/// string. Built separately, they would be one edit apart from counting under
/// one key and clearing another, and the symptom of that is a counter that
/// never resets — the fault this pair exists to fix.
fn button_key(page: u16, usage: u16, rid: u8) -> String {
    format!("buttons:{page:#04x}:{usage:#04x}:{rid}")
}

impl Log {
    fn buttons(&self, page: u16, usage: u16, rid: u8, present: bool, set: &[u16]) {
        if !self.debug_enabled() {
            return;
        }
        let key = button_key(page, usage, rid);
        if present {
            // The run of readings in which this flag was false has ended. The
            // whole entry goes, not just the count: `last_set` is the other
            // half of the state, and a run that restarted while remembering
            // the set it saw before the flag went true would suppress the
            // first line of the new run — the set matches, so nothing resets,
            // and occurrence 1 is never reported as one.
            if let Ok(mut map) = self.seen.lock() {
                map.remove(&key);
            }
            return;
        }
        let list = if set.is_empty() {
            "none".to_owned()
        } else {
            set.iter()
                .map(|u| format!("{u:#04x}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        // The throttle key is stable — `page:usage:rid` — so the map holds one
        // entry per flag rather than one per distinct button set. Folding the set
        // into the key (as before) meant a report whose usages flapped created a
        // fresh key every time and grew the throttle map without bound over a long
        // session.
        //
        // The set is still watched: when it differs from the last one reported for
        // this key, the throttle is reset so the change is written immediately as a
        // fresh occurrence. That shift — a report whose button set moves under a
        // flag that stays false — is a map resolving the wrong report, and it is the
        // event worth catching. A stable set is throttled as one standing condition.
        {
            let Ok(mut map) = LOG.seen.lock() else {
                return;
            };
            let seen = map.entry(key.clone()).or_default();
            if seen.last_set.as_deref() != Some(list.as_str()) {
                seen.last_set = Some(list.clone());
                // The set moved, so the throttle starts over and `debug_repeating`
                // below reports this as occurrence 1. Both halves of that decision
                // happen under one lock: the set and the count are one entry now,
                // and a reader between them cannot see a new set still carrying the
                // old count.
                seen.count = 0;
            }
        }
        debug_repeating(
            format_args!("{key}"),
            format_args!(
                "flag {usage:#04x} on page {page:#04x} not set in report {rid}; \
             usages present: {list}"
            ),
        );
    }
}

/// Bytes as lowercase hex, capped so one event stays one line.
///
/// The cap is generous relative to this device's reports (the longest is 62
/// bytes) and exists for the vendor message pipe, which would otherwise put a
/// wall of hex in a file meant to be read.
fn hex(buf: &[u8]) -> String {
    const MAX: usize = 64;
    let shown = buf.len().min(MAX);
    let mut s = String::with_capacity(shown * 3);
    for (i, b) in buf.iter().take(MAX).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    if buf.len() > MAX {
        let _ = write!(s, " ... (+{} bytes)", buf.len() - MAX);
    }
    s
}

/// Forgets every throttle key, so the next diagnostic for every field is
/// written as a first occurrence.
///
/// Called when the device is reconnected: a new connection re-asks every
/// question, and an answer carried over from the previous one would let a
/// throttle suppress the first report about hardware that may not even be the
/// same unit. One statement clears the counts and the button sets together,
/// because they are one entry: the two-map version had to remember to clear
/// both here, and the recovery path — which had the same duty — forgot.
pub(crate) fn debug_reset() {
    LOG.forget_throttles();
}

impl Log {
    /// Forgets every throttle key. See [`debug_reset`].
    fn forget_throttles(&self) {
        if let Ok(mut map) = self.seen.lock() {
            map.clear();
        }
    }
}

pub(crate) fn session_start(version: &str) {
    event(Cat::Session, &format!("UPS Monitor {version} started"));
}

pub(crate) fn session_end() {
    event(Cat::Session, "stopped");
}

/// The metric snapshot appended to power and notification events.
///
/// This is what makes a line evidence rather than an assertion: "switched to
/// battery" next to the charge, load and runtime at that instant. Absent
/// fields are omitted rather than printed as zero — the device genuinely does
/// not report some of them, and a zero would be read as a measurement.
pub(crate) fn metrics(r: &crate::hid::Reading) -> String {
    let mut s = String::new();
    let mut sep = "";
    let mut add = |part: String| {
        let _ = write!(s, "{sep}{part}");
        sep = ", ";
    };

    if let Some(v) = r.charge_percent {
        add(format!("charge {v}%"));
    }
    if let Some(v) = r.runtime_seconds {
        // Minutes, matching the panel and the tooltip. A log that renders the
        // same measurement in a different unit than the screen forces whoever
        // is comparing the two to convert in their head.
        add(format!("runtime {} min", v / 60));
    }
    if let Some(v) = r.load_percent {
        match r.load_watts {
            Some(w) => add(format!("load {v}% ({w}W)")),
            None => add(format!("load {v}%")),
        }
    } else if let Some(w) = r.load_watts {
        add(format!("load {w}W"));
    }
    if let Some(v) = r.input_voltage {
        add(format!("input {v:.0}V"));
    }
    if let Some(v) = r.output_voltage {
        add(format!("output {v:.0}V"));
    }
    if let Some(v) = r.battery_voltage {
        add(format!("battery {v:.1}V"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that drive the process-wide log.
    ///
    /// The singleton is deliberate (see [`Log`]), so the tests that go through
    /// the free functions — the ones a caller actually uses — share its debug
    /// switch and its throttle map and cannot run concurrently. The tests that
    /// are about the *logic* no longer need this: they build a `Log` of their
    /// own and leave this one alone.
    ///
    /// The lock itself lives in `testsupport`: `hid::selftest` writes its
    /// trace through this same singleton, and a second lock of its own would
    /// serialise a disjoint set of tests against the same state.
    use crate::testsupport::serial_log as serial;

    /// How many consecutive occurrences the process-wide log has recorded for
    /// `key`, or `None` when it holds nothing about it.
    fn throttle_count(key: &str) -> Option<u64> {
        LOG.seen.lock().expect("lock").get(key).map(|r| r.count)
    }

    /// A moment of failure is not a dead log; a run of them is.
    ///
    /// Disabling on the first failed write meant one anti-virus scanner holding
    /// the file open, or one moment on a full disk, cost every line for the rest
    /// of the session — the utility went on running and quietly stopped keeping
    /// the record it exists to keep, with no way to notice.
    ///
    /// Run against a `Log` of its own, so turning one off says nothing about
    /// the log this process writes. That is what the type bought: the switch
    /// used to be a process-wide atomic, and this test had to put it back
    /// afterwards and hold a lock meanwhile so no other test observed it off.
    #[test]
    fn a_run_of_failures_disables_the_log_and_a_success_does_not() {
        let log = Log::new();
        let mut sink = Sink::new();
        for i in 1..MAX_CONSECUTIVE_FAILURES {
            sink.note_failure(&log.enabled);
            assert!(!log.disabled(), "{i} failure(s) must not be fatal");
        }
        sink.note_failure(&log.enabled);
        assert!(
            log.disabled(),
            "{MAX_CONSECUTIVE_FAILURES} consecutive failures is a condition, not a moment"
        );

        // A success in between clears the count, so the failures have to be
        // consecutive to add up. Without that, a log that fails once a week
        // would eventually turn itself off over five unrelated moments spread
        // across a month.
        let log = Log::new();
        let mut sink = Sink::new();
        for _ in 1..MAX_CONSECUTIVE_FAILURES {
            sink.note_failure(&log.enabled);
        }
        sink.note_success();
        sink.note_failure(&log.enabled);
        assert!(
            !log.disabled(),
            "a success in between must have cleared the run"
        );
    }

    /// A failed write drops the handle, so the next line reopens the file.
    ///
    /// This is the recovery: a `File` whose device has gone will keep failing
    /// for as long as it is held, while a fresh `open` on the same path succeeds
    /// the moment the path is writable again.
    #[test]
    fn a_failure_drops_the_handle_so_the_next_line_reopens() {
        let log = Log::new();
        let mut sink = Sink::new();
        sink.file = std::fs::File::open(std::env::current_exe().expect("test exe path")).ok();
        assert!(sink.file.is_some(), "the test needs a handle to drop");
        sink.note_failure(&log.enabled);
        assert!(sink.file.is_none(), "the handle was kept across a failure");
    }

    /// A line written is a line in the file, and the file is the one the log
    /// was pointed at.
    ///
    /// The first check that goes through the writer end to end — open, append,
    /// flush — rather than stopping at the decision to write. It was not
    /// possible before: there was one log, at one path, and a test that wrote
    /// to it wrote to the file this process keeps its own record in.
    #[test]
    fn a_line_reaches_the_file_the_log_was_pointed_at() {
        let dir = crate::testsupport::TempDir::new("evlog-write");
        let file = dir.path().join("ups-monitor.log");
        let log = Log::at(file.clone());

        log.event(Cat::Session, "started");
        log.event(Cat::Device, "connected");

        let written = std::fs::read_to_string(&file).expect("the log file must exist");
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 2, "one event is one line");
        assert!(
            written.contains("\r\n"),
            "the file is read in Notepad, which needs CRLF"
        );
        assert!(lines[0].ends_with("started") && lines[1].ends_with("connected"));
        assert!(
            lines[0].contains(Cat::Session.tag()) && lines[1].contains(Cat::Device.tag()),
            "each line carries its own category"
        );
    }

    /// A log that has turned itself off writes nothing more.
    ///
    /// The condition is not cosmetic: the panel raises a standing warning from
    /// it, and the utility goes on monitoring. What must not happen is a
    /// half-off log that keeps appending after it has told the user it stopped.
    #[test]
    fn a_disabled_log_writes_nothing() {
        let dir = crate::testsupport::TempDir::new("evlog-disabled");
        let file = dir.path().join("ups-monitor.log");
        let log = Log::at(file.clone());

        log.enabled.store(false, Ordering::Relaxed);
        log.event(Cat::Session, "started");

        assert!(
            !file.exists(),
            "a disabled log must not even create the file"
        );
    }

    /// The formatting contract the whole file depends on: one event is one
    /// line. A translated string containing a newline must not split it.
    #[test]
    fn newlines_never_split_an_event() {
        let text = "line one\nline two\rline three";
        let cleaned = if text.contains(['\n', '\r']) {
            text.replace(['\n', '\r'], " ")
        } else {
            text.to_owned()
        };
        assert!(!cleaned.contains('\n'));
        assert!(!cleaned.contains('\r'));
    }

    #[test]
    fn metrics_omit_absent_fields_rather_than_printing_zero() {
        let r = crate::hid::Reading {
            charge_percent: Some(100),
            load_percent: Some(11),
            load_watts: Some(97),
            ..Default::default()
        };
        let m = metrics(&r);
        assert!(m.contains("charge 100%"));
        assert!(m.contains("load 11% (97W)"));
        // The device reports no frequency and this unit reported no voltages
        // in this reading; absent must mean absent, not "0V".
        assert!(!m.contains("input"), "absent fields must be omitted: {m}");
        assert!(
            !m.contains("0V"),
            "absent fields must not render as zero: {m}"
        );
    }

    /// Runtime is reported in whole minutes, the same unit the panel and the
    /// tooltip use. Not a composite, and never seconds.
    #[test]
    fn runtime_is_reported_in_minutes() {
        let r = crate::hid::Reading {
            runtime_seconds: Some(1_305),
            ..Default::default()
        };
        let m = metrics(&r);
        assert!(m.contains("runtime 21 min"), "{m}");
        assert!(!m.contains('h'), "hours must never appear: {m}");
        assert!(!m.contains("45s"), "seconds must never appear: {m}");
    }

    /// With the level off, a diagnostic writes nothing and formats nothing.
    ///
    /// Two properties in one check, because they fail together and for the same
    /// reason. Nothing reaches the file — asserted against a `Log` of this
    /// test's own, so "nothing was written" is a fact about a file rather than
    /// about a global nobody can observe. And nothing is *formatted*: the two
    /// entry points that take `fmt::Arguments` are handed a value whose
    /// `Display` panics, so a line built before the level is tested fails the
    /// test where it happens rather than costing an allocation nobody notices.
    ///
    /// This replaces a test that read the source of these five functions and
    /// checked that `debug_enabled()` appeared before every `format!`. That
    /// test was the best available while the level lived in a process-wide
    /// atomic — and it broke, correctly, the moment the bodies moved into
    /// methods, because it was pinned to the shape of the text rather than to
    /// the behaviour. The behaviour is now observable, so it is asserted
    /// instead.
    ///
    /// It is the hot path itself: twenty-one field reads a poll, on every
    /// machine whose owner never opened Settings.
    #[test]
    fn a_diagnostic_costs_nothing_while_the_level_is_off() {
        /// A value that cannot be formatted without failing the test.
        struct Explodes;

        impl fmt::Display for Explodes {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("the level is off; nothing should have been formatted");
            }
        }

        let dir = crate::testsupport::TempDir::new("evlog-level-off");
        let file = dir.path().join("ups-monitor.log");
        let log = Log::at(file.clone());
        assert!(!log.debug_enabled(), "the level is off by default");

        log.repeating(format_args!("{Explodes}"), format_args!("{Explodes}"));
        log.recovered(format_args!("{Explodes}"), format_args!("{Explodes}"));
        log.transfer(
            19,
            8,
            Some(std::time::Duration::from_micros(120)),
            Ok(&[1, 2]),
        );
        log.write(41, b"T\r", Ok(()));
        log.buttons(0x84, 0x65, 23, false, &[0x63]);
        log.buttons(0x84, 0x65, 23, true, &[0x65, 0x63]);

        assert!(
            !file.exists(),
            "a diagnostic written while the level is off would have created the file"
        );

        // And with the level on, the same calls do reach it — otherwise the
        // assertion above would pass just as well on a log that writes nothing
        // at all.
        log.set_debug(true);
        log.repeating(format_args!("key"), format_args!("a diagnostic"));
        let written = std::fs::read_to_string(&file).expect("the level is on now");
        assert!(written.contains("a diagnostic"));
    }

    /// A standing fault keeps being reported, and says how long it has stood.
    ///
    /// This is the defect the previous filter had, stated as a test. It held
    /// a single "last line written" slot, so two things went wrong at once:
    /// successful transfers are logged too and overwrote the slot between one
    /// failure and the next, and when the slot *did* match, a permanently
    /// broken field was reported once and then silenced for the life of the
    /// process. The symptom was a Debug log in which errors stopped appearing
    /// — the exact opposite of what the level is turned on to see.
    ///
    /// The schedule is deliberately front-loaded. The difference between
    /// "failed once" and "failing every poll" is the difference between a
    /// glitch and a defect, and at a three-second interval it has to be
    /// visible within the first minute rather than after a hundred polls.
    #[test]
    fn a_standing_fault_is_reported_repeatedly_not_once() {
        // Reported at 1, 2, 5, 10, then every 20th.
        assert!(should_report(1), "the first occurrence must be reported");
        assert!(should_report(2), "a repeat must confirm it is not a glitch");
        assert!(should_report(5));
        assert!(should_report(10));
        assert!(should_report(20));
        assert!(
            should_report(200),
            "a fault standing for hours must still show"
        );
        assert!(should_report(2000));

        // And it is a throttle, not a firehose: the great majority of polls
        // are silent, or the file fills with one repeated line.
        let reported = (1..=600).filter(|c| should_report(*c)).count();
        assert!(
            reported < 40,
            "{reported} lines out of 600 polls is not a throttle"
        );

        // Every window has at least one report, so no stretch of the log is
        // ever wholly silent while a fault is standing.
        for window_start in (1..=500).step_by(20) {
            let any = (window_start..window_start + 20).any(should_report);
            assert!(any, "no report between poll {window_start} and the next 20");
        }
    }

    /// Distinct conditions are counted separately.
    ///
    /// Sharing one counter across fields is how the old filter lost failures:
    /// a report succeeding on one id displaced the record of another id
    /// failing, and the failure then re-reported as if new, or not at all.
    #[test]
    fn conditions_are_throttled_independently() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        // Two different keys, interleaved the way a poll interleaves them.
        for _ in 0..3 {
            debug_repeating(format_args!("fail:19"), format_args!("report 19 failed"));
            debug_repeating(format_args!("ok:15"), format_args!("report 15 fine"));
        }
        let counts = (throttle_count("fail:19"), throttle_count("ok:15"));
        assert_eq!(
            counts,
            (Some(3), Some(3)),
            "each condition must keep its own count"
        );

        debug_reset();
        set_debug(before);
    }

    /// The failure line says which side is at fault.
    ///
    /// The first question asked of any of these lines is "is this the device
    /// or is this us", and the reader should not have to look up a Win32
    /// code to answer it. The distinction is real: `ERROR_GEN_FAILURE` from a
    /// HID feature transfer is the device declining to answer a well-formed
    /// request, while `ERROR_INVALID_PARAMETER` means this code built the
    /// call wrongly — one is a hardware or cabling problem and the other is a
    /// bug to fix here.
    #[test]
    fn failures_name_the_side_at_fault() {
        // The code seen on a real CP1350EPFCLCD when the USB link drops.
        assert_eq!(blame(0x8007_001F), "device did not answer");
        assert_eq!(blame(0x0000_001F), "device did not answer", "bare code too");

        assert_eq!(blame(0x8007_048F), "device is gone");
        assert_eq!(blame(0x8007_0079), "device timed out");

        // Ours, and shouted, because it is the only class that means there is
        // something to fix in this program.
        let mine = blame(0x8007_0057);
        assert!(mine.contains("THIS UTILITY"), "{mine}");
        assert_eq!(blame(0x8007_007A), mine, "buffer too small is ours as well");

        // A device another process holds, and one that is merely busy, are
        // two answers a reader acts on differently: the first is resolved by
        // finding the other process, the second by waiting. Collapsed into
        // "cause not classified" both become a shrug.
        let refused = blame(0x8007_0005);
        assert!(refused.contains("another process"), "{refused}");
        assert_eq!(
            blame(0x8007_0020),
            refused,
            "a sharing violation is the same cause as access denied"
        );
        assert_eq!(blame(0x8007_00AA), "device busy");

        // An unrecognised code must not be silently attributed to either
        // side: guessing wrong sends the reader after the wrong component.
        assert_eq!(blame(0x8007_0999), "cause not classified");
    }

    /// Recovery forgets the button set with the count, so a flag that fails
    /// again with the same usages is a fresh event rather than a continuation.
    ///
    /// The set used to live in a second map that the recovery path did not
    /// touch. The count went, the set stayed, and the next failure compared
    /// equal to a set reported before the recovery — so the throttle was not
    /// reset and the new episode was written as the twentieth-odd occurrence of
    /// a condition that had in fact stopped and started again.
    #[test]
    fn recovery_forgets_the_button_set_too() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        let key = "buttons:0x84:0x65:23";
        debug_buttons(0x84, 0x65, 23, false, &[0x63, 0x6e]);
        debug_recovered(format_args!("{key}"), format_args!("flag readable again"));
        debug_buttons(0x84, 0x65, 23, false, &[0x63, 0x6e]);

        let count = throttle_count(key);
        assert_eq!(
            count,
            Some(1),
            "the same set after a recovery must report as a first occurrence"
        );

        debug_reset();
        set_debug(before);
    }

    /// Recovery clears the counter and says how long the fault ran.
    ///
    /// Without the reset an intermittent fault would drift up the throttle
    /// schedule and eventually be reported only every twentieth occurrence,
    /// even though each episode is a fresh event.
    ///
    /// The line itself is asserted because it carries the count, and nothing
    /// read it before: the test took the map entry as the whole outcome and
    /// was blind to what the reader is actually shown. That the wording of
    /// this line could be changed with the suite staying green is how it came
    /// to claim the fault had lasted so many *polls* — a unit this counter
    /// cannot know, since the same keys are fed by the self-test's vendor
    /// channel, where the transfers are not polls.
    #[test]
    fn recovery_resets_the_count_and_reports_how_long_it_ran() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        debug_repeating(
            format_args!("value:0x84:0x35"),
            format_args!("load unreadable"),
        );
        debug_repeating(
            format_args!("value:0x84:0x35"),
            format_args!("load unreadable"),
        );
        let recovery = appended_by(|| {
            debug_recovered(
                format_args!("value:0x84:0x35"),
                format_args!("load readable again"),
            );
        });

        let after = throttle_count("value:0x84:0x35");
        assert_eq!(after, None, "recovery must forget the condition");
        assert!(
            recovery.contains("load readable again after 2 failed attempt(s)"),
            "recovery must say how long the fault ran: {recovery}"
        );
        assert!(
            !recovery.contains("poll"),
            "the count is occurrences, and this line cannot know they were polls: {recovery}"
        );

        debug_reset();
        set_debug(before);
    }

    /// Nothing is recorded while the level is off.
    ///
    /// The counters are per-condition and unbounded in principle, so they
    /// must not accumulate for users who never enable diagnostics.
    #[test]
    fn the_throttle_records_nothing_while_disabled() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(false);
        debug_reset();

        debug_repeating(
            format_args!("value:0x84:0x35"),
            format_args!("load unreadable"),
        );
        let recorded = LOG.seen.lock().expect("lock").len();
        assert_eq!(
            recorded, 0,
            "disabled diagnostics must not accumulate state"
        );

        set_debug(before);
    }

    /// Hex is the raw bytes, unchanged, so a report can be compared against a
    /// `ups-dump` listing byte for byte.
    #[test]
    fn hex_renders_bytes_verbatim() {
        assert_eq!(hex(&[0x18, 0x2a, 0x03, 0x46, 0x05]), "18 2a 03 46 05");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0xff]), "00 ff");
    }

    /// A long buffer is capped, and says so rather than truncating silently.
    ///
    /// The vendor message pipe uses 62-byte reports and nothing stops a
    /// future descriptor declaring more. A line that quietly dropped the tail
    /// would be worse than one that is obviously incomplete: the reader would
    /// take what they see for the whole report.
    #[test]
    fn hex_caps_long_buffers_and_admits_it() {
        let long = vec![0xabu8; 100];
        let s = hex(&long);
        assert!(s.contains("(+36 bytes)"), "the cap must be disclosed: {s}");
        assert!(!s.contains('\n'), "one event is one line");

        // Exactly the cap is not over it. A buffer that fits announcing
        // "(+0 bytes)" tells the reader a report was truncated when the whole
        // of it is on the line in front of them — and this device's longest
        // report is the one most likely to land on the boundary.
        let exact = vec![0xabu8; 64];
        let s = hex(&exact);
        assert!(!s.contains('('), "a buffer that fits is not truncated: {s}");
        assert_eq!(s.split(' ').count(), 64, "every byte is shown: {s}");
    }

    /// Debug output is off unless it was turned on.
    ///
    /// The default matters: the level is read from the INI at startup, and a
    /// build that defaulted to writing diagnostics would fill the log of
    /// every user who never opened Settings.
    #[test]
    fn diagnostics_are_off_until_enabled() {
        let _guard = serial();
        // The static is process-wide, so this restores what it found rather
        // than assuming a starting value: another test may have set it.
        let before = debug_enabled();
        set_debug(false);
        assert!(!debug_enabled());
        set_debug(true);
        assert!(debug_enabled());
        set_debug(before);
    }

    /// The DEBUG tag obeys the column rule the rest of the file follows.
    #[test]
    fn the_debug_tag_lines_up_with_the_others() {
        assert!(Cat::Debug.tag().len() <= 7);
        assert!(Cat::Debug.tag().chars().all(|c| c.is_ascii_uppercase()));
        // Distinct from ERROR on purpose: a field that failed to read is not
        // something the utility failed at, and mixing the two would make
        // `findstr ERROR` return the ordinary noise of a flaky device.
        assert_ne!(Cat::Debug.tag(), Cat::Error.tag());
    }

    /// The text the process-wide log gained while `body` ran.
    ///
    /// Read by byte offset rather than by clearing the file: the log holds an
    /// open append handle and its own byte counter, so emptying it underneath
    /// would leave the two disagreeing for the rest of the session. Callers
    /// assert with `contains`, which is what makes this safe against a line
    /// another test appended in between.
    fn appended_by(body: impl FnOnce()) -> String {
        let file = path();
        let before = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        body();
        let bytes = std::fs::read(&file).unwrap_or_default();
        let from = usize::try_from(before).unwrap_or(usize::MAX);
        String::from_utf8_lossy(bytes.get(from..).unwrap_or_default()).into_owned()
    }

    /// The stamp is fixed-width, local, and sorts as text.
    ///
    /// Every line of the file begins with it, and the file is read by eye and
    /// by `findstr`. A stamp of the wrong width breaks the column alignment
    /// the rest of the format is built on; a stamp in the wrong order stops
    /// the file sorting chronologically as text, which is how a reader
    /// correlates it with a Windows event log. Neither failure announces
    /// itself — the line is still written, and still looks like a line.
    #[test]
    fn a_stamp_is_fixed_width_and_sorts_as_text() {
        let s = stamp();
        let (date, time) = s
            .split_once(' ')
            .expect("a stamp is a date and a time, in that order");

        let fields = |field: &str, sep: char| -> Vec<usize> {
            assert!(
                field
                    .split(sep)
                    .all(|p| p.chars().all(|c| c.is_ascii_digit())),
                "every field is decimal digits: {s:?}"
            );
            field.split(sep).map(str::len).collect()
        };
        // Most significant first and zero-padded, or the text order is not the
        // chronological one.
        assert_eq!(fields(date, '-'), vec![4, 2, 2], "{s:?}");
        assert_eq!(fields(time, ':'), vec![2, 2, 2], "{s:?}");

        let year: u32 = date
            .split('-')
            .next()
            .expect("split always yields one field")
            .parse()
            .expect("the year is digits");
        assert!(
            year >= 2024,
            "the clock is the system's, not an epoch: {s:?}"
        );
    }

    /// The log names the file it writes, beside the executable.
    ///
    /// The standing warning shown when the log dies quotes this path, and a
    /// warning naming an empty or relative path tells the user nothing about
    /// which file to make writable. The stem follows the executable's for the
    /// same reason the INI's does: a renamed copy keeps its files together.
    #[test]
    fn the_log_names_the_file_it_writes() {
        let exe = std::env::current_exe().expect("a running test has a path");
        let file = path();
        assert!(
            file.is_absolute(),
            "a relative log path follows the working directory: {file:?}"
        );
        assert_eq!(file.parent(), exe.parent(), "the log lives beside the exe");
        assert_eq!(file.file_stem(), exe.file_stem(), "and is named after it");
        assert_eq!(
            file.extension().and_then(std::ffi::OsStr::to_str),
            Some("log")
        );
    }

    /// The standing warning follows the log's switch rather than a constant.
    ///
    /// Both answers have to be observed. A reporter stuck on "alive" leaves
    /// the panel silent about a log that stopped recording; one stuck on
    /// "dead" raises a warning about a log that is writing perfectly, and the
    /// user is invited to fix a working file.
    #[test]
    fn the_standing_warning_follows_the_log_switch() {
        let _guard = serial();
        assert!(!disabled(), "the process log starts alive");

        LOG.enabled.store(false, Ordering::Relaxed);
        let while_off = disabled();
        // Restored before the assertion, so a failure here does not leave the
        // log off for every test that follows.
        LOG.enabled.store(true, Ordering::Relaxed);
        assert!(
            while_off,
            "a log that turned itself off must be reported so"
        );
        assert!(!disabled());
    }

    /// A cap measured in megabytes, not in lines.
    ///
    /// The cap exists so a log left running for months does not fill the
    /// disk; it must not be so small that an ordinary session rotates away the
    /// records the file is kept for. A few kilobytes of ordinary events is
    /// nothing, and the first line of the session must still be there.
    #[test]
    fn the_cap_does_not_rotate_away_an_ordinary_session() {
        let dir = crate::testsupport::TempDir::new("evlog-cap");
        let file = dir.path().join("ups-monitor.log");
        let log = Log::at(file.clone());

        log.event(Cat::Session, "started");
        let filler = "x".repeat(100);
        for i in 0..40 {
            log.event(Cat::Device, &format!("line {i} {filler}"));
        }

        let text = std::fs::read_to_string(&file).expect("the log file must exist");
        assert!(
            text.len() > 4096,
            "the fixture must exceed any plausible small cap: {} bytes",
            text.len()
        );
        assert!(
            text.contains("started"),
            "a few kilobytes must not rotate the session away"
        );
    }

    /// A feature transfer is recorded, and a failure retires the success
    /// counter for that report.
    ///
    /// The throttle map is the observable half: `transfer` reports successes
    /// and failures under separate keys, so a report that stops answering
    /// starts a failure run rather than continuing a success one. Without the
    /// retirement the next success would be announced as the two-hundredth
    /// consecutive one, across a gap in which the device was not answering at
    /// all.
    #[test]
    fn a_feature_transfer_is_recorded_under_its_own_report_id() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        let elapsed = Some(std::time::Duration::from_micros(120));
        debug_transfer(7, 8, elapsed, Ok(&[0x01, 0x02]));
        let after_success = throttle_count("ok:7");
        debug_transfer(7, 8, elapsed, Err((0x8007_001F, "gen failure".to_owned())));
        let failures = throttle_count("fail:7");
        let successes = throttle_count("ok:7");

        debug_reset();
        set_debug(before);

        assert_eq!(after_success, Some(1), "a read that worked is recorded");
        assert_eq!(failures, Some(1), "and so is one that did not");
        assert_eq!(
            successes, None,
            "a failure retires the run of successes rather than extending it"
        );
    }

    /// A first occurrence is a first occurrence, not a run of one.
    ///
    /// The suffix is what tells a reader that a line is a standing condition
    /// rather than something that just happened. Printed on the first
    /// occurrence it says the opposite of the truth, and it says it in the
    /// file kept precisely to establish when a fault began.
    #[test]
    fn a_first_occurrence_is_not_labelled_as_a_repeat() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        let first = appended_by(|| {
            debug_repeating(
                format_args!("first-occurrence:1"),
                format_args!("the field did not read"),
            );
        });
        // The second reported occurrence: `should_report` passes counts 1 and
        // 2, so one more call is enough to reach the other arm.
        let second = appended_by(|| {
            debug_repeating(
                format_args!("first-occurrence:1"),
                format_args!("the field did not read"),
            );
        });

        debug_reset();
        set_debug(before);

        assert!(first.contains("the field did not read"), "{first}");
        assert!(
            !first.contains("[x"),
            "the first report of a condition is not a repeat: {first}"
        );
        assert!(
            second.contains("[x2 in a row]"),
            "a repetition must say how many: {second}"
        );
    }

    /// A button set that moves restarts the throttle; one that stays put does
    /// not.
    ///
    /// A flag reading false while the usages under it change is a map that
    /// resolved the wrong report — the event this level exists to catch — and
    /// it is caught by the change, not by the flag. Answered the other way
    /// round, a stable condition would be re-announced every poll and the
    /// interesting shift would be throttled away.
    #[test]
    fn a_button_set_that_moves_restarts_the_throttle() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        let key = "buttons:0x84:0x66:24";
        debug_buttons(0x84, 0x66, 24, false, &[0x63]);
        let first = throttle_count(key);
        debug_buttons(0x84, 0x66, 24, false, &[0x63]);
        let unchanged = throttle_count(key);
        debug_buttons(0x84, 0x66, 24, false, &[0x63, 0x6e]);
        let moved = throttle_count(key);

        debug_reset();
        set_debug(before);

        assert_eq!(first, Some(1));
        assert_eq!(unchanged, Some(2), "a standing condition is one condition");
        assert_eq!(moved, Some(1), "a set that moved is a fresh occurrence");
    }

    /// A flag that reads true ends the run of readings in which it did not.
    ///
    /// The set moving is not enough, and this is the case that proves it: the
    /// set here is the same before and after, which is the ordinary shape of
    /// an episode rather than a contrived one. A UPS reads `FullyCharged,
    /// ACPresent` on mains, spends a self-test reading `Discharging,
    /// ACPresent`, and comes back to the first — so `Discharging`'s key sees
    /// the same set on both sides of the twelve seconds it was true, and
    /// nothing in the set comparison can notice.
    ///
    /// Taken from a real log, where four flags of one report, read in one poll
    /// and all false since the same instant, reported 60, 10, 10 and 10
    /// readings in a row. The three agreeing with the clock were right.
    #[test]
    fn a_flag_reading_true_ends_the_run_of_readings_it_was_false() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);
        debug_reset();

        let key = "buttons:0x85:0x45:11";
        // On mains: false, twice, under one set.
        debug_buttons(0x85, 0x45, 11, false, &[0x46, 0xd0]);
        debug_buttons(0x85, 0x45, 11, false, &[0x46, 0xd0]);
        let before_episode = throttle_count(key);

        // The self-test: the flag itself is among the usages now.
        debug_buttons(0x85, 0x45, 11, true, &[0x45, 0xd0]);
        let during = throttle_count(key);

        // Back on mains, with the set it had before the episode.
        debug_buttons(0x85, 0x45, 11, false, &[0x46, 0xd0]);
        let after = throttle_count(key);

        debug_reset();
        set_debug(before);

        assert_eq!(before_episode, Some(2));
        assert_eq!(during, None, "the flag being set must end the run");
        assert_eq!(
            after,
            Some(1),
            "the run after the episode is a new one, not the old one resumed"
        );
    }

    /// A feature write reaches the file, in both outcomes, with its payload.
    ///
    /// The bytes are shown because a write's payload is known — unlike a
    /// failed read's undefined buffer — and it is exactly what a reader
    /// diagnosing a refused command needs. A refusal also names the cause, so
    /// the line answers "why" and not only "no".
    #[test]
    fn a_feature_write_reaches_the_file_with_its_payload() {
        let _guard = serial();
        let before = debug_enabled();
        set_debug(true);

        let written = appended_by(|| {
            debug_write(0x0b, &[0xde, 0xad], Ok(()));
            debug_write(0x0b, &[0xde, 0xad], Err((0x8007_0005, "denied".to_owned())));
        });

        set_debug(before);

        assert!(written.contains("set_feature rid 11"), "{written}");
        assert!(written.contains("de ad"), "the payload is shown: {written}");
        assert!(
            written.contains("another process"),
            "a refused write names the cause: {written}"
        );
    }

    /// The closing line is written, and is a session line.
    ///
    /// It is the only thing that distinguishes a clean shutdown from a process
    /// that was killed or that stopped when the machine lost power — which is
    /// the event this utility exists to be present for.
    #[test]
    fn the_session_closes_with_a_line() {
        let _guard = serial();
        let closing = appended_by(session_end);
        assert!(closing.contains("stopped"), "{closing}");
        assert!(closing.contains(Cat::Session.tag()), "{closing}");
    }

    #[test]
    fn categories_are_fixed_width_tags() {
        for c in [
            Cat::Session,
            Cat::Device,
            Cat::Power,
            Cat::Notify,
            Cat::Config,
            Cat::Error,
            Cat::Debug,
        ] {
            assert!(c.tag().len() <= 7, "{c:?} tag breaks column alignment");
            assert!(c.tag().chars().all(|ch| ch.is_ascii_uppercase()));
        }
    }
}
