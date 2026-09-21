//! Vendor self-test channel.
//!
//! The CP1350EPFCLCD starts its self-test not through the HID `Test` feature
//! report — writing that was observed to either do nothing or begin an
//! unbounded discharge — but through a vendor ASCII channel layered on top of
//! HID: the command `T\r` is delivered in output report 41 by `WriteFile`, and
//! the acknowledgement `#0` comes back in input report 40 read by `ReadFile`.
//!
//! Two facts, both established from USBPcap captures of the stock PowerPanel
//! software and confirmed on the device, shape this module:
//!
//! * **The channel is gated.** Feature report 37 (usage `0xff01:0x20`) is an
//!   enable flag, and it opens the channel on a **rising edge** 0 -> 1, not on
//!   level: writing 1 over an already-1 flag does nothing. So the edge is
//!   forced — drive it low, pause, drive it high — immediately before the
//!   command. The gate lapses on its own about twelve seconds after the last
//!   traffic, which is why the edge goes right before `T` and no keep-alive is
//!   needed: the command lands in a freshly opened channel with seconds to
//!   spare.
//! * **The gate must be closed again.** PowerPanel lowers it at the end of its
//!   session; leaving it latched open is untidy and changes the state a later
//!   run starts from. Closing it is therefore an invariant of the session type
//!   rather than a step in a sequence: [`SelfTestSession`] lowers the gate in
//!   its `Drop`, so every exit path — success, refusal, timeout, an early
//!   `?` — closes the channel behind it.
//!
//! The command handle is separate from the polling handle on purpose. The
//! monitor opens the device with a zero access mask (the trick that lets it
//! read feature reports while the system HID stack holds the battery), and a
//! zero-mask handle cannot `WriteFile`. This module opens its own short-lived
//! `GENERIC_READ | GENERIC_WRITE` handle for the duration of the test and
//! closes it after; the monitor's own handle is never touched, so the trick it
//! depends on is never at risk. Every mask this needs was confirmed openable on
//! the device.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Devices::HumanInterfaceDevice::HidD_SetNumInputBuffers;
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_EVENT, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::descriptor::{Descriptor, FlagField, Scope, PAGE_VENDOR};
use super::device::RawDevice;
use crate::error::{Error, Result};
/// Usage of the channel gate flag on the vendor page (feature report 37).
const U_CHANNEL_GATE: u16 = 0x20;

/// The self-test command: the ASCII letter T terminated by a carriage return.
const CMD_SELF_TEST: &[u8] = b"T\r";

/// Bytes a vendor command frame spends before its payload: the report id and
/// the one-byte payload length.
///
/// Named because two separate limits follow from it — the payload must fit the
/// output report (`output_len >= FRAME_HEADER + payload.len()`) and its length
/// must fit one byte — and both were previously implicit in a slice index and a
/// cast.
const FRAME_HEADER: usize = 2;

/// `Test` feature value meaning a test is running right now.
const TEST_IN_PROGRESS: u32 = 5;

/// Minimum battery charge, in percent, before a test may start.
///
/// A test transfers the load to the inverter and drains the battery for its
/// duration, so the pack must be full enough to carry it; starting below this
/// risks a low-battery condition mid-test. Public because the panel greys the
/// self-test button on the same threshold — the check must not drift between
/// the button and the session.
pub(crate) const MIN_CHARGE_PERCENT: u32 = 90;

/// Pause between driving the gate low and high, long enough for the two feature
/// writes to be distinct transactions the firmware sees as an edge.
const GATE_EDGE_PAUSE: Duration = Duration::from_millis(150);

/// How long to wait for the `#0` acknowledgement on input report 40. Captures
/// show it in 40-48 ms; a second is generous enough that a timeout means "no
/// reply", not "read too soon".
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Depth of the driver input queue. The device streams status reports every
/// 64 ms, so the default of 32 can drop the acknowledgement if it arrives
/// during a pause.
const INPUT_BUFFERS: u32 = 128;

/// Logs one self-test phase line to the DEBUG category, only when debug logging
/// is on. English like the rest of the log, and gated so the ordinary run
/// leaves a single DEVICE line while a debug run leaves a trace of every phase —
/// which is exactly where a silent failure hides: the gate, the write, the ack,
/// or the poll.
fn debug(text: &str) {
    if crate::evlog::debug_enabled() {
        crate::evlog::event(crate::evlog::Cat::Debug, text);
    }
}

/// Test progress is polled once a second. Captures measure about 15 s from
/// `Test = 5` to the final code; 30 s leaves room for a slower run.
///
/// The window is measured from the acknowledgement, not from the command:
/// waiting for `#0` may take up to `REPLY_TIMEOUT`, and folding that wait into
/// the same budget silently shortened the observation window by however long
/// the firmware took to answer.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the reply queue between the reader thread and the session.
///
/// The device pushes a status report about every 64 ms whether or not anyone
/// asked, and the only reply this module wants is the `#0` that follows a
/// command. On an unbounded queue the status stream simply accumulated: a 30 s
/// test left some 470 buffers of `input_len` bytes alive until the session
/// dropped. Bounding it turns that stream into what it is — noise that is
/// discarded once the queue is full.
///
/// The bound cannot cost an acknowledgement. Sizing it against the status
/// cadence: the queue is drained immediately before the command is written,
/// and `acknowledged` starts consuming within microseconds, so at most one or
/// two status reports can be in front of the `#0`. Eight leaves that margin
/// several times over.
const REPLY_QUEUE: usize = 8;

/// Outcome of a self-test attempt.
///
/// The values map onto the NUT `Test` codes for the states the test itself
/// reaches (`Passed` .. `Aborted`); the rest describe why a test never got that
/// far, which the raw code cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfTestOutcome {
    /// `Test` reached 1.
    Passed,
    /// `Test` reached 2.
    PassedWithWarning,
    /// `Test` reached 3.
    Failed,
    /// `Test` reached 4.
    Aborted,
    /// The command was sent but `#0` never came back: the channel did not
    /// accept `T` as a start command.
    NotAcknowledged,
    /// A pre-test safety check failed; nothing was sent.
    NotSafe,
    /// `Test` did not leave state 5 within the polling window. Not treated as a
    /// test failure: the run may simply be slower than expected.
    Timeout,
    /// The application asked to shut down while the test was still running, so
    /// the watch was abandoned. The gate is still closed by `Drop`; the test
    /// itself may continue on the device, but the utility is exiting and stops
    /// following it.
    Cancelled,
    /// The gate, the write or the read failed at the transport level.
    ChannelError,
}

impl SelfTestOutcome {
    /// Why no test ran, when none did.
    ///
    /// The line between the two halves of this enum, and it decides where each
    /// outcome is seen. The four verdicts are the device's own `Test` codes:
    /// the register says the same thing, and goes on saying it, so the panel
    /// reads them from there and this returns `None`. The rest say why no test
    /// happened — a refused command, a channel error, conditions that had
    /// lapsed by the time the thread reached the request — and the register has
    /// no code for any of them, because from the device's side nothing
    /// occurred.
    ///
    /// Both halves used to be kept as "the last test" and shown on that line,
    /// which froze it: once this session had run a test the register was
    /// shadowed for good, and a test started from the front panel of the UPS
    /// changed nothing on screen.
    pub(crate) fn not_run(self) -> Option<SelfTestNotRun> {
        match self {
            Self::Passed | Self::PassedWithWarning | Self::Failed | Self::Aborted => None,
            Self::NotAcknowledged => Some(SelfTestNotRun::NotAcknowledged),
            Self::NotSafe => Some(SelfTestNotRun::NotSafe),
            Self::Timeout => Some(SelfTestNotRun::Timeout),
            Self::Cancelled => Some(SelfTestNotRun::Cancelled),
            Self::ChannelError => Some(SelfTestNotRun::ChannelError),
        }
    }
}

/// Why a self-test the utility asked for never ran.
///
/// The half of [`SelfTestOutcome`] the device's `Test` register cannot express,
/// as its own type rather than as a rule about which variants a function may be
/// given. The warning that carries these can then hold nothing else, which is
/// the difference between a state that cannot occur and one that is asserted
/// not to.
///
/// The other half has no type here and needs none: a verdict is read off the
/// register, on the line that reports it, where it stays current instead of
/// being frozen at whatever this session last saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfTestNotRun {
    /// The command was sent but `#0` never came back.
    NotAcknowledged,
    /// A pre-test safety check failed; nothing was sent.
    NotSafe,
    /// `Test` did not leave state 5 within the polling window.
    Timeout,
    /// The application asked to shut down while the test was still running.
    Cancelled,
    /// The gate, the write or the read failed at the transport level.
    ChannelError,
}

impl SelfTestNotRun {
    /// Localisation key for the warning line naming the reason.
    pub(crate) fn lang_key(self) -> crate::strings::Key {
        use crate::strings::Key;
        match self {
            Self::NotAcknowledged => Key::SelftestNotAcknowledged,
            Self::NotSafe => Key::SelftestNotSafe,
            Self::Timeout => Key::SelftestTimeout,
            Self::Cancelled => Key::SelftestCancelled,
            Self::ChannelError => Key::SelftestChannelError,
        }
    }
}

/// Result of waiting for the vendor acknowledgement. Distinct from a plain
/// `bool` so a shutdown mid-wait is reported as its own outcome rather than
/// collapsed into "no acknowledgement".
///
/// `Copy` because it is three fieldless cases and callers pass it around by
/// value; a scripted answer held for a whole run would otherwise have to be
/// cloned to be given twice.
#[derive(Clone, Copy)]
enum Ack {
    /// `#0` arrived: the command was taken as a start.
    Ok,
    /// No `#0` within the timeout, or a different reply: the command was not
    /// accepted.
    None,
    /// The application asked to shut down while waiting.
    Cancelled,
}

/// The pre-test conditions, read fresh from the device the instant before the
/// command. They are re-read here rather than taken from the last poll because
/// a poll can be seconds old, and the mains can fail or the battery discharge
/// in that gap; the check has to describe the device now.
pub(crate) struct Safety {
    pub charge_percent: Option<u32>,
    /// `None` when the mains flag could not be read. An unknown mains state
    /// blocks the test for the same reason an unknown charge does: a value that
    /// could not be read is not evidence the condition is met.
    pub ac_present: Option<bool>,
    pub discharging: Option<bool>,
    /// The raw `Test` value standing immediately before the command, or `None`
    /// when the read failed.
    ///
    /// The value itself, not a `== InProgress` verdict derived from it. Two
    /// questions are asked of this field and they need different answers: "is a
    /// test already running" (the pre-test check) and "what did `Test` say
    /// *before* this one started" (the baseline `watch` compares against). The
    /// second was thrown away, so the watcher had no way to tell a fresh result
    /// from the code left over by the previous test, and fell back on timing —
    /// which reported the *previous* test's outcome as this one's whenever the
    /// firmware took longer than a second to enter state 5.
    pub test_before: Option<u32>,
}

impl Safety {
    /// The first unmet pre-test condition, in English for the log, or `None`
    /// when it is safe to start. Named so the debug log can say *why* a test was
    /// refused rather than only that it was — the reason is the diagnostic part,
    /// and `NotSafe` alone forces the reader to guess which check tripped.
    ///
    /// Every condition treats an unread flag as unmet, not as clear: a test
    /// drains the battery onto the inverter, so it proceeds only on positively
    /// confirmed-safe state. Reading `discharging` as `false` when it could not
    /// be read was the unsafe direction — it let a test start on a pack that
    /// might already be discharging.
    fn refusal_reason(&self) -> Option<String> {
        match self.charge_percent {
            None => return Some("battery charge is unknown".into()),
            Some(c) if c < MIN_CHARGE_PERCENT => {
                return Some(format!(
                    "battery charge {c}% is below the {MIN_CHARGE_PERCENT}% floor"
                ));
            }
            _ => {}
        }
        match self.ac_present {
            None => return Some("mains state is unknown".into()),
            Some(false) => return Some("no mains power".into()),
            Some(true) => {}
        }
        match self.discharging {
            None => return Some("discharge state is unknown".into()),
            Some(true) => return Some("already discharging".into()),
            Some(false) => {}
        }
        if self.test_before == Some(TEST_IN_PROGRESS) {
            return Some("a test is already running".into());
        }
        None
    }
}

/// Report ids and lengths the vendor channel needs, resolved once from the
/// descriptor. Absent when the firmware does not expose the channel.
struct Channel {
    /// The gate flag, kept whole (not just its report id) so the gate write is
    /// placed by usage through `write_flag` rather than at a guessed byte.
    gate: FlagField,
    out_report_id: u8,
    in_report_id: u8,
    feature_len: usize,
    output_len: usize,
    input_len: usize,
}

impl Channel {
    fn resolve(desc: &Descriptor) -> Option<Self> {
        let gate = desc.find_flag(PAGE_VENDOR, U_CHANNEL_GATE, Scope::Unscoped)?;
        Some(Self {
            gate,
            out_report_id: desc.vendor_out_report_id?,
            in_report_id: desc.vendor_in_report_id?,
            feature_len: desc.feature_len.max(2),
            output_len: desc.output_len,
            input_len: desc.input_len,
        })
    }
}

/// A self-test session over the vendor channel.
///
/// Owns the read/write command handle and the reader thread, and guarantees the
/// gate is closed when it drops. Construct with [`SelfTestSession::open`], run
/// with [`SelfTestSession::run`].
pub(crate) struct SelfTestSession<'a> {
    /// The monitor's device, used for its zero-mask feature reads (the gate
    /// write goes through it) and its device path.
    dev: &'a RawDevice,
    channel: Channel,
    /// The command handle, opened `GENERIC_READ | GENERIC_WRITE` with
    /// `FILE_FLAG_OVERLAPPED` so the reader's blocking read can be cancelled
    /// deterministically at shutdown.
    handle: HANDLE,
    /// Vendor replies, forwarded from the reader thread with the report id in
    /// byte 0 so foreign reports can be filtered out.
    replies: Receiver<Vec<u8>>,
    /// Signalled by `Drop` to tell the reader to abort its in-flight read and
    /// exit. Owned here, borrowed by the reader; the join in `Drop` happens
    /// before this event drops, so the borrow never dangles.
    cancel_event: Event,
    /// The reader thread. Joined in `Drop` after `cancel_event` is signalled, so
    /// the thread never outlives the session and the handle is never closed
    /// while a read is still in flight on it.
    reader: Option<JoinHandle<()>>,
    /// True once the gate has been driven open, so `Drop` only closes a gate it
    /// actually opened.
    gate_open: bool,
}

impl<'a> SelfTestSession<'a> {
    /// Opens the command handle and starts the reply reader.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestUnavailable`] when the descriptor exposes no vendor
    /// channel, or exposes one whose reports are too short to carry a frame.
    /// That is a firmware fact rather than a failure: this device cannot run a
    /// vendor self-test, and the panel hides the control instead of offering
    /// one that will always refuse.
    ///
    /// [`Error::SelfTestChannel`] when the channel is present but the handle
    /// will not open — another process holding the interface, or the path
    /// going away between enumeration and open.
    pub(crate) fn open(dev: &'a RawDevice, desc: &Descriptor) -> Result<Self> {
        let channel = Channel::resolve(desc).ok_or(Error::SelfTestUnavailable)?;
        if channel.output_len < 4 || channel.input_len < 2 {
            return Err(Error::SelfTestUnavailable);
        }

        let handle = open_rw(&dev.path)?;
        // Deepen the input queue before the first read so a reply arriving
        // between status reports is not dropped.
        //
        // The failure is reported rather than discarded because this call is
        // the whole reason the acknowledgement is seen at all: at the default
        // depth of 32 the `#0` reply is lost behind the status reports the
        // device sends unprompted. A run that could not deepen the queue will
        // very likely end in `NotAcknowledged`, and without this line the log
        // would show that verdict with no trace of what caused it.
        // SAFETY: `handle` is the live handle just opened; the depth is passed
        // by value.
        if !unsafe { HidD_SetNumInputBuffers(handle, INPUT_BUFFERS) } {
            debug("self-test: input queue could not be deepened; an ack may be missed");
        }
        // Two fallible steps follow — creating the cancel event (manual-reset,
        // so a request to cancel stays latched for every later check) and
        // starting the reader. Until the session value exists, nothing owns the
        // handle opened above, so either failure must close it. They are
        // chained into one expression so there is a single place that does,
        // rather than a `CloseHandle` copied into each error arm — which is how
        // one of them eventually goes missing.
        let started = Event::new(true).and_then(|cancel_event| {
            let (replies, reader) = spawn_reader(handle, channel.input_len, cancel_event.raw())?;
            Ok((cancel_event, replies, reader))
        });
        let (cancel_event, replies, reader) = match started {
            Ok(parts) => parts,
            Err(e) => {
                // SAFETY: closes the handle this function opened, on the one
                // path that abandons it before a session takes ownership.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(e);
            }
        };

        Ok(Self {
            dev,
            channel,
            handle,
            replies,
            cancel_event,
            reader: Some(reader),
            gate_open: false,
        })
    }

    /// Runs the test against this session's channel: force the gate edge, send
    /// `T`, read the acknowledgement, then poll `Test` to completion.
    ///
    /// The sequence itself is [`run_protocol`], which is where the arguments
    /// are described and where it can be run without a device.
    pub(crate) fn run(
        &mut self,
        safety: &Safety,
        abort: &AtomicBool,
        tick: impl FnMut() -> Tick,
    ) -> SelfTestOutcome {
        run_protocol(self, safety, abort, tick)
    }
}

/// What the self-test protocol needs from the vendor channel, and no more.
///
/// The protocol below is the sequence the audit called the most delicate code
/// in the project — gate edge, drain, send, acknowledge, watch — and until this
/// trait existed none of it could be exercised: every step went straight to a
/// live UPS through a second open handle, so the only way to run the sequence
/// was to discharge a real battery under a real load. What was tested were the
/// pure helpers around it.
///
/// Six methods, each a fact the protocol genuinely consults. The two report ids
/// are here because the trace lines name them, and a trace that cannot say
/// which report carried the command is not much of a trace.
trait VendorChannel {
    /// Drives the gate low, pauses, then high — the rising edge that opens the
    /// channel — and records that it is open so it will be closed again.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`] if either edge cannot be written. An
    /// implementation must leave the gate recorded as open only when it
    /// actually is, or `Drop` will try to close a gate that was never opened.
    fn open_gate(&mut self, abort: &AtomicBool) -> Result<()>;
    /// Report id the gate flag lives on, for the trace.
    fn gate_report(&self) -> u8;
    /// Empties the reply queue without blocking.
    fn drain(&self);
    /// Sends one vendor command, answering when it went out.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`] if the frame cannot be built or written.
    /// The instant returned must be taken *after* the write settles: it is
    /// what the acknowledgement wait measures from, and one taken earlier
    /// would credit the device with time it never had.
    fn send(&self, payload: &[u8]) -> Result<Instant>;
    /// Report id the command frame goes out on, for the trace.
    fn out_report(&self) -> u8;
    /// Waits for the `#0` acknowledgement, ignoring the status stream.
    fn acknowledged(&self, sent_at: Instant, abort: &AtomicBool) -> Ack;
}

/// What one watch tick reports back to the protocol.
///
/// Two facts out of one poll, carried together because they must come from the
/// same instant: the code decides the verdict, and the discharge flag decides
/// whether the run is over. Read a second apart they can disagree — a verdict
/// from after the transfer back, paired with a discharge flag from before it,
/// would end the session while the load is still on the inverter, which is the
/// state this pairing exists to rule out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tick {
    /// The `Test` feature, or `None` when the read failed.
    pub(crate) test: Option<u32>,
    /// Whether the pack is still carrying the load, or `None` when the flag
    /// could not be read.
    pub(crate) discharging: Option<bool>,
}

/// How long to keep watching for the load to come back to mains after the
/// verdict.
///
/// The `Test` code turns from 5 to its result the moment the firmware has
/// decided, and the UPS then takes another second or two to move the load off
/// the inverter — measured at one to three seconds on the CP1350EPFCLCD. The
/// session is not over until it has: `PresentStatus.Discharging` is still set
/// in that window, which is precisely the condition the panel refuses to start
/// a new test on. Ending the session earlier made the buzzer button live while
/// the self-test button beside it was still greyed, for no reason the user
/// could see.
///
/// Generously longer than anything measured, because the cost of waiting too
/// long is a few seconds of a greyed button and the cost of not waiting is the
/// defect above.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// The self-test protocol, over any channel that can carry it.
///
/// `safety` is the pre-checked device state; a failing check returns `NotSafe`
/// without sending anything.
///
/// `abort` is raised when the application is shutting down. It is checked at
/// every step that can otherwise block for seconds — the gate edge pause, the
/// acknowledgement wait, and each poll tick — so a user who quits mid test does
/// not wait out the full 30 s window. On abort the outcome is `Cancelled` and
/// the gate is still closed by the session's `Drop`.
///
/// `tick` polls the device once and reports back a [`Tick`]. It is a closure
/// rather than a call on the device so this stays agnostic about how the two
/// fields are resolved — the caller owns the descriptor and the per-poll cache
/// that make those reads correct.
fn run_protocol(
    channel: &mut impl VendorChannel,
    safety: &Safety,
    abort: &AtomicBool,
    mut tick: impl FnMut() -> Tick,
) -> SelfTestOutcome {
    if let Some(reason) = safety.refusal_reason() {
        debug(&format!("self-test refused: {reason}"));
        return SelfTestOutcome::NotSafe;
    }
    debug("self-test: pre-test checks passed, opening vendor channel");

    // A fresh rising edge immediately before the command. The gate lapses
    // after seconds of silence, so opening it any earlier risks it being
    // shut by the time `T` goes out.
    if let Err(e) = channel.open_gate(abort) {
        crate::evlog::event(crate::evlog::Cat::Error, &format!("self-test: {e}"));
        return SelfTestOutcome::ChannelError;
    }
    if abort.load(Ordering::Relaxed) {
        debug("self-test: cancelled before sending command");
        return SelfTestOutcome::Cancelled;
    }
    debug(&format!(
        "self-test: gate edge 0->1 on report {}",
        channel.gate_report()
    ));

    // Discard anything already queued so the acknowledgement is not
    // confused with a status report waiting from before the command.
    channel.drain();

    let sent_at = match channel.send(CMD_SELF_TEST) {
        Ok(at) => at,
        Err(e) => {
            crate::evlog::event(crate::evlog::Cat::Error, &format!("self-test: {e}"));
            return SelfTestOutcome::ChannelError;
        }
    };
    debug(&format!(
        "self-test: sent 'T' ({}-byte frame) on report {}",
        CMD_SELF_TEST.len() + 2,
        channel.out_report()
    ));

    // `#0` is the accepted acknowledgement. A status summary (`#I...`) or
    // silence both mean `T` was not taken as a start command.
    match channel.acknowledged(sent_at, abort) {
        Ack::Ok => {}
        Ack::Cancelled => {
            debug("self-test: cancelled while awaiting acknowledgement");
            return SelfTestOutcome::Cancelled;
        }
        Ack::None => {
            debug(&format!(
                "self-test: no '#0' acknowledgement within {} ms",
                REPLY_TIMEOUT.as_millis()
            ));
            return SelfTestOutcome::NotAcknowledged;
        }
    }
    // Taken here rather than inside `acknowledged`: this is the moment the
    // acknowledgement was observed, and it is the origin of the
    // observation window. Timing the window from `sent_at` charged the
    // wait for `#0` against the 30 s budget, so the window was always
    // short by the acknowledgement latency — up to a full second.
    let acked_at = Instant::now();
    debug(&format!(
        "self-test: '#0' acknowledged in {} ms, watching Test",
        sent_at.elapsed().as_millis()
    ));

    let (outcome, last) = watch(acked_at, safety.test_before, abort, &mut tick);
    settle(Instant::now(), abort, last, &mut tick);
    outcome
}

/// Waits for the load to come off the inverter, once the verdict is in.
///
/// The verdict and the end of the run are two different moments (see
/// [`SETTLE_TIMEOUT`]), and this is the second one. Returns as soon as the
/// discharge flag reads clear — which for every outcome where no transfer ever
/// happened is the tick already in hand, so nothing is polled and nothing is
/// waited for.
///
/// An unreadable flag is not treated as clear. The whole point is to leave the
/// device in a state the panel can act on, and "could not read it" is not that;
/// the wait ends on its own deadline instead, which is the same answer the
/// session gives to a device that stops answering anywhere else.
///
/// `began` is when the verdict landed, and it is a parameter for the reason
/// [`watch`]'s `started_at` is one: the deadline is the one thing here the tick
/// closure cannot influence, and a caller that could not place the start could
/// only reach the expiry by spending [`SETTLE_TIMEOUT`] of real time.
fn settle(began: Instant, abort: &AtomicBool, last: Tick, tick: &mut impl FnMut() -> Tick) {
    let mut state = last;
    while state.discharging != Some(false) {
        if abort.load(Ordering::Relaxed) {
            debug("self-test: shutting down before the load returned to mains");
            return;
        }
        if began.elapsed() >= SETTLE_TIMEOUT {
            debug(&format!(
                "self-test: load still on battery {} s after the verdict; \
                 no longer waiting",
                SETTLE_TIMEOUT.as_secs()
            ));
            return;
        }
        interruptible_sleep(POLL_INTERVAL, abort);
        state = tick();
    }
    debug(&format!(
        "self-test: load back on mains {:.1} s after the verdict",
        began.elapsed().as_secs_f32()
    ));
}

impl VendorChannel for SelfTestSession<'_> {
    fn gate_report(&self) -> u8 {
        self.channel.gate.report_id
    }

    fn out_report(&self) -> u8 {
        self.channel.out_report_id
    }

    /// Drives the gate low, pauses, then high — the rising edge that opens the
    /// channel. Records that the gate is open so `Drop` will close it. The
    /// pause is interruptible so a shutdown during the 150 ms edge does not
    /// stall the exit.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`] from [`Self::write_gate`] on either edge. A
    /// failure on the rising edge leaves `gate_open` false, so `Drop` does not
    /// write a closing edge for a gate that never opened.
    fn open_gate(&mut self, abort: &AtomicBool) -> Result<()> {
        self.write_gate(false)?;
        interruptible_sleep(GATE_EDGE_PAUSE, abort);
        self.write_gate(true)?;
        self.gate_open = true;
        Ok(())
    }

    /// Builds and sends a vendor command frame: report id, payload length,
    /// the ASCII payload, and zeros. The tail past the payload is deliberately
    /// zero — the captured frames carry PowerPanel's own heap residue there,
    /// which the firmware ignores.
    ///
    /// The write is overlapped because the handle is (see `open_rw`); it is
    /// issued and then waited to completion synchronously, since there is
    /// nothing else for the caller to do until the command is on the wire.
    ///
    /// Both limits of the frame layout are checked rather than assumed. A
    /// payload longer than the output report used to panic inside the slice
    /// assignment — on a descriptor this code had merely mis-parsed, in a
    /// thread holding an open device — and one past 255 bytes had its length
    /// silently truncated into the one-byte field, so the firmware would have
    /// read a frame shorter than the one sent. Neither is reachable with the
    /// two-byte command this channel carries today; both are properties of the
    /// layout, not of the command, and a check that cannot fire costs nothing
    /// beside a write to hardware.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`] on both layout checks — a payload past 255
    /// bytes, which could not state its own length in the one-byte field, and
    /// one longer than the output report, which would not fit the frame — and
    /// propagated from [`Self::write_overlapped`] when the device refuses the
    /// write or accepts fewer bytes than were offered.
    fn send(&self, payload: &[u8]) -> Result<Instant> {
        let Ok(len) = u8::try_from(payload.len()) else {
            return Err(Error::SelfTestChannel(format!(
                "payload of {} bytes cannot state its own length in one byte",
                payload.len()
            )));
        };
        if self.channel.output_len < FRAME_HEADER + payload.len() {
            return Err(Error::SelfTestChannel(format!(
                "payload of {} bytes does not fit an output report of {}",
                payload.len(),
                self.channel.output_len
            )));
        }

        // Built by appending rather than by writing into a zeroed buffer at
        // three offsets. The header is two bytes because two are pushed, and
        // the payload lands after them because it is pushed next — the two
        // checks above have already established that it fits, and the trailing
        // zeros are the padding to the report length. Written as subscripts,
        // `FRAME_HEADER` had to mean the same thing in the guard above and in
        // the range below, and only a reader was keeping them equal.
        let mut frame = Vec::with_capacity(self.channel.output_len);
        frame.push(self.channel.out_report_id);
        frame.push(len);
        frame.extend_from_slice(payload);
        frame.resize(self.channel.output_len, 0);

        let written = self.write_overlapped(&frame)?;
        let at = Instant::now();
        if written == frame.len() {
            Ok(at)
        } else {
            Err(Error::SelfTestChannel(format!(
                "short write: {written} of {} bytes",
                frame.len()
            )))
        }
    }

    /// Waits for the `#0` acknowledgement on the vendor input report, ignoring
    /// the ordinary status stream. Any other reply, or none, is a refusal.
    /// `abort` is checked between reads so a shutdown does not wait out the full
    /// reply timeout.
    fn acknowledged(&self, sent_at: Instant, abort: &AtomicBool) -> Ack {
        let deadline = sent_at + REPLY_TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if abort.load(Ordering::Relaxed) {
                return Ack::Cancelled;
            }
            // Wake at least every poll interval to re-check `abort` even when no
            // reply is arriving, rather than blocking for the whole `left`.
            let step = left.min(POLL_INTERVAL);
            match self.replies.recv_timeout(step) {
                Ok(data) if data.first() == Some(&self.channel.in_report_id) => {
                    let payload = payload_of(&data);
                    if payload.starts_with(b"#0") {
                        return Ack::Ok;
                    }
                    // A reply on the right report but not `#0`: the command was
                    // seen but not taken as a start. Logged as hex because the
                    // payload is a vendor summary, not text worth decoding.
                    debug(&format!(
                        "self-test: unexpected reply on report {}: {}",
                        self.channel.in_report_id,
                        hex(payload)
                    ));
                    return Ack::None;
                }
                Ok(_) => continue,
                // A timeout on the stepped wait is not a refusal on its own: the
                // loop condition and the `abort` check decide whether to keep
                // waiting. A disconnected sender (`RecvError`) is terminal.
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ack::None,
            }
        }
        Ack::None
    }

    /// Empties the reply queue without blocking.
    fn drain(&self) {
        while self.replies.try_recv().is_ok() {}
    }
}

/// The two writes the channel is built on, kept off [`VendorChannel`].
///
/// Neither is a step of the protocol: one drives a feature report through the
/// monitor's own handle, the other is the overlapped mechanics `send` is made
/// of. A fake channel has nothing to say about either, and putting them in the
/// trait would have obliged every implementation to answer for the transport
/// rather than for the conversation.
impl SelfTestSession<'_> {
    /// Drives the gate flag through the monitor's zero-mask handle. The gate is
    /// a feature report, so `HidD_SetFeature` reaches it without the command
    /// handle.
    ///
    /// Read-modify-write, not a zeroed buffer: the old code built an all-zero
    /// report and set byte 1 to the flag, which both hardcoded the flag's
    /// position and wiped every other bit in report 37 on each edge. Any other
    /// vendor state living in that report was cleared as a side effect. Now the
    /// report is fetched first, the gate bit is set or cleared *by usage* with
    /// `write_flag`, and the rest of the report is written back unchanged.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`], wrapping whichever step failed — the read
    /// that fetches the current report, placing the flag by usage, or the
    /// write back. One variant for all three because a self-test aborts on the
    /// first channel failure and reports a single outcome; the message names
    /// the step, so the log still distinguishes them.
    fn write_gate(&self, open: bool) -> Result<()> {
        let mut buf = feature_buf(self.channel.feature_len, self.channel.gate.report_id);
        self.dev
            .get_feature(&mut buf)
            .map_err(|e| Error::SelfTestChannel(format!("gate read failed: {e}")))?;
        super::descriptor::write_flag(self.dev, self.channel.gate, &mut buf, open)
            .map_err(|e| Error::SelfTestChannel(format!("gate set failed: {e}")))?;
        self.dev
            .set_feature(&buf)
            .map_err(|e| Error::SelfTestChannel(format!("gate write failed: {e}")))
    }

    /// Issues one overlapped write on the command handle and waits for it,
    /// returning the number of bytes transferred. Each call owns its own event
    /// and `OVERLAPPED`, so writes and the reader's reads never share state.
    ///
    /// Every path out of here has settled the transfer first. On the success
    /// path `GetOverlappedResult(…, true)` did the waiting; on a failure path
    /// [`abandon`] does. An earlier version returned the error with `?`, which
    /// dropped `ov`, `event` and — one frame up — the buffer, while the kernel
    /// could still be writing into them. That is stack corruption arriving
    /// later, somewhere unrelated, with nothing to connect it to this function.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`] when the write is refused outright, when the
    /// overlapped wait fails, or when it completes with fewer bytes than the
    /// frame carried. Propagates the same variant from [`Event::new`] if the
    /// completion event cannot be created. Every one of those paths abandons
    /// the transfer first, so nothing the kernel may still be writing into is
    /// dropped while it does so.
    fn write_overlapped(&self, frame: &[u8]) -> Result<usize> {
        let event = Event::new(false)?;
        let mut ov = OVERLAPPED {
            hEvent: event.raw(),
            ..Default::default()
        };

        // SAFETY: `frame` and `ov` both outlive every path out of this
        // function — the success path waits, and both failure paths call
        // `abandon` — so the kernel is never left writing into freed memory.
        let started = unsafe { WriteFile(self.handle, Some(frame), None, Some(&mut ov)) };
        match started {
            Ok(()) => {}
            Err(e) if e.code() == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult() => {}
            Err(e) => {
                // The write never started, so there is nothing in flight — but
                // the cheapest way to be sure of that is to ask, and asking is
                // what the other failure path has to do anyway.
                abandon(self.handle, &ov);
                return Err(Error::SelfTestChannel(format!("write failed: {e}")));
            }
        }
        let mut written = 0u32;
        // SAFETY: `ov` is the same structure the write was started with and is
        // still live; `true` blocks until the transfer has finished.
        if let Err(e) = unsafe { GetOverlappedResult(self.handle, &ov, &mut written, true) } {
            abandon(self.handle, &ov);
            return Err(Error::SelfTestChannel(format!("write failed: {e}")));
        }
        Ok(written as usize)
    }
}

/// Cancels an overlapped operation and waits until the kernel has let go of it.
///
/// The two calls are a pair and neither is useful alone: `CancelIoEx` asks, and
/// the blocking `GetOverlappedResult` is what makes the answer true by the time
/// this returns. Until it does, the `OVERLAPPED` and the caller's buffer are
/// still the kernel's to write into, so a `return` in between them frees memory
/// that is still in use.
///
/// Both results are discarded on purpose: there is nothing to report and
/// nothing to decide. "Cancel failed" here means the operation had already
/// finished, which is the outcome being waited for.
///
/// Written once because it is used twice — the reader thread's shutdown does
/// exactly this, and the two were the same sequence written out separately
/// until one of the two places forgot it.
fn abandon(handle: HANDLE, ov: &OVERLAPPED) {
    // SAFETY: `ov` is a live reference for the whole of this function, which is
    // the point of it being a function: the caller cannot return between the
    // two calls.
    unsafe {
        let _ = CancelIoEx(handle, Some(ov));
    }
    let mut drained = 0u32;
    // SAFETY: the same live `ov`; `true` is what makes the cancellation
    // complete rather than merely requested by the time this returns.
    unsafe {
        let _ = GetOverlappedResult(handle, ov, &mut drained, true);
    }
}

/// What the reader thread does with an in-flight read once its wait returns.
///
/// The wait is over two handles — the read's own completion event and the
/// session's cancel event — and the interesting part is not which of them fired
/// but whether the kernel has let go of the read. That is the question the two
/// variants answer, and it is the one the buffer's lifetime depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterWait {
    /// The read's own event was signalled. The operation has finished; its
    /// length can be collected and the loop goes on.
    Collect,
    /// Anything else: cancellation, an abandoned wait, or a wait that failed.
    /// The read is still the kernel's, so it has to be cancelled and waited out
    /// before the thread's buffer and `OVERLAPPED` may drop.
    Abandon,
}

/// Classifies the return of `WaitForMultipleObjects` over (read, cancel).
///
/// Written as a function of the wait code alone so that the reader's exit rule
/// is a value rather than a condition buried in a loop: an exhaustive `match`
/// at the call site is then what proves the analysis complete, and the mapping
/// itself can be checked without a device — see `the_wait_code_decides_the_exit`.
///
/// Only `WAIT_OBJECT_0` means the read finished. `WAIT_OBJECT_0 + 1` is
/// cancellation, `WAIT_TIMEOUT` cannot occur under `INFINITE` but is not
/// assumed away, `WAIT_ABANDONED` belongs to mutexes and not to events, and
/// `WAIT_FAILED` is the wait itself failing. All four leave a read in flight,
/// and all four are the same instruction: get it back before returning.
const fn after_wait(waited: WAIT_EVENT) -> AfterWait {
    if waited.0 == WAIT_OBJECT_0.0 {
        AfterWait::Collect
    } else {
        AfterWait::Abandon
    }
}

/// What one reading of `Test` means for the run that was just started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    /// Nothing attributable to this run yet: the read failed, the code is not a
    /// result, or it is the same result that already stood before the command.
    Waiting,
    /// State 5 — the firmware has the test running.
    Running,
    /// This run's own result.
    Finished(SelfTestOutcome),
}

/// The rule that decides whether a `Test` code belongs to the run in progress.
///
/// A pure function of the three facts it needs, so the rule can be tested
/// without waiting out real seconds — the defect it replaces lived in a branch
/// that could only be reached by the clock, which is why nothing caught it.
///
/// `baseline` is what `Test` held immediately before the command. The register
/// keeps the previous test's code until the firmware overwrites it, so a
/// terminal code equal to the baseline is evidence of nothing. Only two things
/// attribute a result to this run: the code differs from the baseline, or state
/// 5 was observed in between.
fn progress_of(code: Option<u32>, baseline: Option<u32>, seen_in_progress: bool) -> Progress {
    if code == Some(TEST_IN_PROGRESS) {
        return Progress::Running;
    }
    match code.and_then(terminal_outcome) {
        Some(outcome) if seen_in_progress || code != baseline => Progress::Finished(outcome),
        _ => Progress::Waiting,
    }
}

/// Polls `Test` once a second until this test's own result appears, the window
/// closes, or `abort` is raised.
///
/// The attribution rule lives in [`progress_of`]; this is the loop that applies
/// it, times out and honours the abort flag.
///
/// The earlier version asked the clock instead of the baseline: a terminal code
/// read more than a second after the command was accepted as the outcome. The
/// acknowledgement takes 40–48 ms and the transition into state 5 an unmeasured
/// while longer, so a device that was merely slow to start reported the code
/// left over from last time — typically 1, and the panel said "Passed" for a
/// test whose result did not exist yet.
///
/// A free function, not a method: it needs nothing from the session but the
/// window origin, the baseline, the abort flag and the reader closure, and
/// keeping it out of `impl` makes that explicit.
///
/// `started_at` is the moment the command was acknowledged, not the moment it
/// was sent, so the whole of `POLL_TIMEOUT` is spent watching `Test`. Every
/// timestamp in the trace below is relative to the same origin, which makes
/// them read as progress through the window rather than as time since an
/// event that is already logged separately.
fn watch(
    started_at: Instant,
    baseline: Option<u32>,
    abort: &AtomicBool,
    tick: &mut impl FnMut() -> Tick,
) -> (SelfTestOutcome, Tick) {
    let mut seen_in_progress = false;
    let mut last_code: Option<u32> = None;
    let mut state = Tick::default();
    let deadline = started_at + POLL_TIMEOUT;

    loop {
        if abort.load(Ordering::Relaxed) {
            debug(&format!(
                "self-test: cancelled at {:.1} s",
                started_at.elapsed().as_secs_f32()
            ));
            return (SelfTestOutcome::Cancelled, state);
        }
        state = tick();
        let code = state.test;
        // Log each change in Test, so the trace shows the 6->5->1 progression
        // and how long it took rather than only the final outcome.
        if code != last_code {
            match code {
                Some(c) => debug(&format!(
                    "self-test: Test = {c} at {:.1} s",
                    started_at.elapsed().as_secs_f32()
                )),
                None => debug("self-test: Test read failed"),
            }
            last_code = code;
        }
        match progress_of(code, baseline, seen_in_progress) {
            Progress::Running => seen_in_progress = true,
            Progress::Finished(outcome) => {
                // The raw code is already in the trace: the block above logs
                // every change in `Test` with its timestamp. What this line
                // adds is the verdict and why it was attributed to this run,
                // which is the part that cannot be read off the codes alone.
                debug(&format!(
                    "self-test: finished at {:.1} s: {outcome:?} ({})",
                    started_at.elapsed().as_secs_f32(),
                    if seen_in_progress {
                        "after in-progress"
                    } else {
                        "changed from the pre-test value"
                    }
                ));
                return (outcome, state);
            }
            Progress::Waiting => {}
        }
        if Instant::now() >= deadline {
            debug(&format!(
                "self-test: timed out after {} s in state {}",
                POLL_TIMEOUT.as_secs(),
                last_code.map_or_else(|| "unknown".to_owned(), |c| c.to_string())
            ));
            return (SelfTestOutcome::Timeout, state);
        }
        // Interruptible so an abort raised mid-tick is seen within a fraction of
        // a second, not after the full poll interval.
        interruptible_sleep(POLL_INTERVAL, abort);
    }
}

/// Sleeps up to `total`, returning early if `abort` is raised. Polls the flag
/// in short slices so a shutdown is observed promptly without a condvar or a
/// dedicated timer.
fn interruptible_sleep(total: Duration, abort: &AtomicBool) {
    const SLICE: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + total;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if abort.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(left.min(SLICE));
    }
}

impl Drop for SelfTestSession<'_> {
    fn drop(&mut self) {
        // Close the gate we opened, matching PowerPanel and leaving the channel
        // the way it was found. This is the whole reason the session is a type:
        // the close happens on every exit path, including an early return or a
        // panic unwinding through `run`.
        //
        // It is not skipped when the session is being wound up because the stop
        // flag went up, and that is deliberate. Closing the gate is an
        // invariant of having opened it, not a step of the happy path, and a
        // channel left open on a device that keeps its state across a
        // reconnection is a worse thing to leave behind than a slow exit. The
        // cost is honest and belongs in the exit budget rather than after it:
        // two more control transfers, each of which can take the driver's
        // timeout on a device that has stopped answering. `Ups` gives up its
        // own series between transfers (see `Error::Stopped`); this one is the
        // part that has to be paid.
        if self.gate_open {
            let closed = self.write_gate(false);
            debug(match closed {
                Ok(()) => "self-test: gate closed",
                Err(_) => "self-test: gate close failed",
            });
        }
        // Bring the reader down before closing the handle, in this order:
        //
        //   1. Signal the cancel event.
        //   2. `join` waits for the reader to leave its loop.
        //   3. only then `CloseHandle`.
        //
        // Step 2 terminates because the reader's loop has exactly six exits and
        // every one of them is reached without waiting on the device:
        //
        //   1. `Event::new` refused before the loop began. No read was ever
        //      issued.
        //   2. The zero-timeout poll of the cancel event at the top of an
        //      iteration saw it signalled. No read is in flight — this is the
        //      exit that covers reads completing inline faster than the wait
        //      below is reached.
        //   3. `ReadFile` failed with something other than ERROR_IO_PENDING.
        //      The read never started, so there is nothing to take back.
        //   4. The wait returned anything but the read's own event —
        //      cancellation being the ordinary case. Here a read *is* in
        //      flight, and this is the one exit that calls `abandon`. The
        //      classification is `after_wait`, and the `match` over its result
        //      is what makes the four non-completion codes one case rather
        //      than three plus an assumption.
        //   5. `GetOverlappedResult` failed. It was called with `true`, so it
        //      waited: the operation is over by the time it answers, however
        //      it answers.
        //   6. The reply channel is disconnected. The read that produced those
        //      bytes has completed.
        //
        // Exits 3 and 5 not calling `abandon` is the part worth stating,
        // because a reader that skipped it on exit 4 would look the same until
        // it corrupted memory. On 3 the kernel never took the buffer; on 5 the
        // blocking `GetOverlappedResult` has already given it back.
        //
        // What none of the six may do is block indefinitely, and the one that
        // could is exit 4. It cannot, because the handle is overlapped — see
        // `spawn_reader`, which is where that argument is written out and why
        // this is a reference to it rather than a second copy. Closing the
        // handle only after the join also removes the use-after-close hazard:
        // the numeric handle cannot be reused by a reconnecting poll thread
        // while the reader is still inside a read on it.
        self.cancel_event.signal();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // SAFETY: an owned handle, closed once. The reader thread has been
        // joined by this point, so nothing else can still be using it.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Opens a read/write handle on the interface path. Shared read/write so the
/// system HID stack, which still holds the device, does not block the open.
///
/// `FILE_FLAG_OVERLAPPED` is required, not incidental: the reader's read must be
/// cancellable at shutdown with a hard completion guarantee, and that guarantee
/// only holds for overlapped I/O. Every use of this handle — the reader's reads
/// and the session's writes — therefore drives its own `OVERLAPPED`.
///
/// # Errors
///
/// [`Error::SelfTestChannel`], carrying the Win32 message, when `CreateFileW`
/// refuses — most often another process holding the interface without sharing
/// it, or a device unplugged between enumeration and this call.
fn open_rw(path: &str) -> Result<HANDLE> {
    let wide = crate::wide::nul_terminated(path);
    // SAFETY: `wide` is a NUL-terminated copy of the device path and outlives
    // the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0 | FILE_FLAG_OVERLAPPED.0),
            None,
        )
    }
    .map_err(|e| Error::SelfTestChannel(format!("cannot open command handle: {e}")))?;
    Ok(handle)
}

/// A raw handle that may cross a thread boundary, under the contract below.
struct SendHandle(HANDLE);

// SAFETY: the handle is only ever moved into the reader thread as a raw value
// and read from there; it is closed by the session, never by the thread, and
// the session joins the thread before closing it. Sending the raw handle across
// the boundary is sound under that contract.
unsafe impl Send for SendHandle {}

impl SendHandle {
    /// Reads the raw handle through `&self` so that a `move` closure calling
    /// this captures the whole wrapper, not just the inner field.
    ///
    /// This is the crux of the `Send` contract, not a convenience accessor.
    /// Under Rust 2021 disjoint closure captures, a `move` closure that touches
    /// `owned.0` directly captures only the `HANDLE` field — which is not
    /// `Send` — and the thread spawn fails to compile. Going through a method
    /// borrows the whole `SendHandle`, so the wrapper carrying `unsafe impl
    /// Send` is what crosses the thread boundary. It also keeps the code free of
    /// the `let owned = owned;` rebinding trick some clippy versions flag as a
    /// redundant local.
    fn raw(&self) -> HANDLE {
        self.0
    }
}

/// An owned Win32 event handle, closed on drop.
///
/// Used for two distinct roles in the overlapped reader: the per-read
/// completion event inside the `OVERLAPPED`, and the session-wide cancel event
/// that `Drop` signals to bring the reader down. Both are the same primitive,
/// so they share one RAII type rather than two ad-hoc `CloseHandle` calls.
struct Event(HANDLE);

impl Event {
    /// Creates an event. `manual_reset` stays signalled until reset by hand,
    /// which is what the cancel event needs — once cancellation is requested it
    /// must remain latched for every subsequent wait. The completion event is
    /// auto-reset (`manual_reset = false`) so each read starts unsignalled.
    ///
    /// # Errors
    ///
    /// [`Error::SelfTestChannel`] when `CreateEventW` fails, which in practice
    /// means the process is out of handles. Reported through the channel
    /// variant rather than a variant of its own: the caller is always a step
    /// of the self-test sequence, and the outcome it reports is the same one.
    fn new(manual_reset: bool) -> Result<Self> {
        // SAFETY: no security descriptor and no name — both explicitly null —
        // and the flags are by value. The handle is owned by this value.
        let handle = unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
            .map_err(|e| Error::SelfTestChannel(format!("cannot create event: {e}")))?;
        Ok(Self(handle))
    }

    fn signal(&self) {
        // SAFETY: signals an event this value owns and has not yet closed.
        unsafe {
            let _ = SetEvent(self.0);
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: an owned event handle, closed exactly once by `Drop`.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Starts the reader thread and returns the reply channel plus its join handle.
///
/// The reader issues **overlapped** reads and waits on two events: the read's
/// own completion event and the shared `cancel` event. `Drop` signals `cancel`;
/// the reader then aborts its in-flight read and exits. This is why the handle
/// is opened `FILE_FLAG_OVERLAPPED`: on an overlapped handle `CancelIoEx`
/// followed by `GetOverlappedResult` is guaranteed by the OS to complete the
/// read promptly, so the `join` in `Drop` cannot hang. A synchronous handle
/// gives no such guarantee — a blocked synchronous `ReadFile` on a silent
/// device returns only at the next report, which may never come, and the join
/// would wait forever. That was the flaw in the first version of this fix.
///
/// # Errors
///
/// [`Error::SelfTestChannel`] when the thread cannot be started, which is the
/// only fallible step here — the channel and the handle wrappers cannot fail.
/// `Builder` rather than `std::thread::spawn` precisely so that this is an
/// error and not a panic: the release profile is `panic = "abort"`, so the
/// plain form would take the whole utility down rather than fail one
/// self-test.
///
/// Read failures *inside* the reader are not reported here. The thread is
/// already running by then, and it ends its loop instead — a silent device and
/// a cancelled read are the same event to the caller, which is waiting on the
/// reply channel and sees it close.
fn spawn_reader(
    handle: HANDLE,
    report_len: usize,
    cancel: HANDLE,
) -> Result<(Receiver<Vec<u8>>, JoinHandle<()>)> {
    // Bounded on purpose — see `REPLY_QUEUE`. Overflow is dropped by the
    // reader rather than blocking it: a reader stalled on a full queue would
    // stop servicing the cancel event, and `Drop` joins that thread.
    let (tx, rx) = mpsc::sync_channel(REPLY_QUEUE);
    let owned = SendHandle(handle);
    // The cancel event is owned by the session, which outlives the reader (it
    // joins the thread in `Drop` before the event is closed). The reader only
    // borrows it, so it crosses the boundary as a raw handle in a `Send`
    // wrapper, not as an owning `Event`.
    let cancel = SendHandle(cancel);
    // `Builder`, not `std::thread::spawn`: the plain form panics when a thread
    // cannot be created, and the release profile is `panic = "abort"`, so a
    // failure to start this helper would take the whole utility down rather
    // than fail one self-test. The poll thread is started the same way for the
    // same reason.
    let reader = std::thread::Builder::new()
        .name("ups-selftest-reader".into())
        .spawn(move || {
            let cancel = cancel.raw();
            let file = owned.raw();
            // The completion event lives for the whole thread and is reused for
            // every read; auto-reset so each read begins unsignalled.
            let Ok(read_event) = Event::new(false) else {
                return;
            };
            let mut buf = vec![0u8; report_len.max(1)];

            loop {
                // Cancellation is checked before issuing a read as well as
                // while waiting for one. The wait below covers the ordinary
                // case, but a read that completes *inline* never reaches it,
                // so a burst of already-queued reports could otherwise carry
                // the loop past a cancel that had already been signalled. The
                // event is manual-reset, so a zero-timeout poll of it is a
                // pure test with no signal consumed.
                // SAFETY: `cancel` is the session's event, which outlives this
                // thread — the session joins before dropping it. A timeout of
                // zero polls rather than blocks.
                if unsafe { WaitForSingleObject(cancel, 0) } == WAIT_OBJECT_0 {
                    break;
                }

                let mut ov = OVERLAPPED {
                    hEvent: read_event.raw(),
                    ..Default::default()
                };

                // Issue the overlapped read. Success means it completed inline;
                // ERROR_IO_PENDING means it is in flight and will signal the
                // event.
                // SAFETY: `buf` and `ov` are locals of this thread's own loop
                // and every exit from it goes through `abandon` first, so the
                // kernel has let go of both before they drop.
                let started = unsafe { ReadFile(file, Some(&mut buf), None, Some(&mut ov)) };
                let pending = match started {
                    Ok(()) => false,
                    Err(e)
                        if e.code()
                            == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult() =>
                    {
                        true
                    }
                    // A real failure to even start the read: nothing left to do.
                    Err(_) => break,
                };

                if pending {
                    // Wait for either the read to finish or cancellation. Index
                    // 0 is the read event, 1 is cancel; `WaitForMultipleObjects`
                    // returns the lowest signalled index, so a completed read is
                    // handled even if cancel fires in the same instant — the
                    // read is then drained cleanly rather than left dangling.
                    // SAFETY: both handles outlive the wait — one is this
                    // loop's own event, the other the session's — and the array
                    // is a live temporary passed with its own length.
                    let waited = unsafe {
                        WaitForMultipleObjects(&[read_event.raw(), cancel], false, INFINITE)
                    };
                    match after_wait(waited) {
                        AfterWait::Collect => {}
                        AfterWait::Abandon => {
                            // Cancel (or a wait failure): abort the in-flight
                            // read and drain it, so the buffer and OVERLAPPED
                            // are no longer in use before the thread returns
                            // and they drop.
                            abandon(file, &ov);
                            break;
                        }
                    }
                }

                // The read completed (inline or signalled). Collect its length.
                let mut read = 0u32;
                // SAFETY: `ov` is the structure this iteration's read was
                // started with and is still live.
                if unsafe { GetOverlappedResult(file, &ov, &mut read, true) }.is_err() {
                    // Aborted or failed: either way the reader is done.
                    break;
                }
                let n = read as usize;
                if n == 0 {
                    continue;
                }
                // `n` is what the kernel says it wrote, and `buf` is what it
                // was given; taking the shorter of the two used to be spelled
                // as a `min` inside a range, which is a bound applied to a
                // slice by hand. `get` applies it to the slice itself, and its
                // fallback — the whole buffer — is the same answer a length
                // longer than the buffer deserves.
                let report = buf.get(..n).unwrap_or(&buf);
                match tx.try_send(report.to_vec()) {
                    Ok(()) => {}
                    // Nobody is reading: this is the unsolicited status
                    // stream during a test. Discard and keep going.
                    Err(mpsc::TrySendError::Full(_)) => {}
                    // The session is gone; so is the reason to read.
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        })
        .map_err(|e| Error::SelfTestChannel(format!("cannot start the reply reader: {e}")))?;
    Ok((rx, reader))
}

/// A feature report buffer of `len` bytes whose first byte is the report id.
///
/// Every feature exchange starts this way — the id is what the read is *for*,
/// and a zeroed buffer with nothing in byte 0 fetches report zero — so the
/// two-line preamble that used to open each call site is written once. The id
/// is the first byte because it is pushed first, and `resize` supplies the
/// rest; `len` below two would otherwise produce a buffer with no room for the
/// id it was built to carry.
fn feature_buf(len: usize, report_id: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len.max(1));
    buf.push(report_id);
    buf.resize(len.max(1), 0);
    buf
}

/// The ASCII payload of a vendor reply, honouring the count byte in byte 1.
///
/// The reply is a header of two bytes — the report id and a count — followed
/// by that many ASCII characters. Both are taken with `split_first`, so the
/// header is consumed by being named rather than by an offset repeated in a
/// subscript and again in a range: a device that sent fewer than two bytes
/// falls out of the same expression that reads them, and a count longer than
/// the report is bounded by the slice it is applied to.
fn payload_of(data: &[u8]) -> &[u8] {
    let Some((_report_id, rest)) = data.split_first() else {
        return &[];
    };
    let Some((&count, body)) = rest.split_first() else {
        return &[];
    };
    body.get(..count as usize).unwrap_or(body)
}

/// Formats bytes as space-separated hex, capped so a stray long reply cannot
/// flood the log. For the DEBUG trace only.
fn hex(bytes: &[u8]) -> String {
    const MAX: usize = 16;
    let shown: Vec<String> = bytes.iter().take(MAX).map(|b| format!("{b:02x}")).collect();
    let mut s = shown.join(" ");
    if bytes.len() > MAX {
        s.push_str(" …");
    }
    if s.is_empty() {
        s.push_str("(empty)");
    }
    s
}

/// Maps a `Test` code to the outcome it terminates in, or `None` when the code
/// is not a result at all.
///
/// The distinction is load-bearing. Codes 5 "in progress", 6 "never run" and
/// 7 "scheduled" describe a test that has not produced a verdict, and the
/// previous mapping folded every unrecognised code into `Failed` — so a `Test`
/// still reading 6 was reported to the user as a failed self-test. "The test
/// has not said anything yet" and "the test failed" are different facts, and
/// only the second is a result. A code this build does not recognise is
/// likewise not treated as a verdict: the watch keeps polling and, if nothing
/// terminal ever appears, returns `Timeout`, which is the honest answer.
fn terminal_outcome(code: u32) -> Option<SelfTestOutcome> {
    match code {
        1 => Some(SelfTestOutcome::Passed),
        2 => Some(SelfTestOutcome::PassedWithWarning),
        3 => Some(SelfTestOutcome::Failed),
        4 => Some(SelfTestOutcome::Aborted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The reader's exit rule, checked without a device.
    ///
    /// The rule is one sentence — only the read's own event means the read
    /// finished — and the whole cost of getting it wrong is on the other side:
    /// an exit that skipped `abandon` would leave the kernel writing into a
    /// buffer this thread is about to drop, and it would do so silently until
    /// it did not. `WaitForMultipleObjects` has five answers here and four of
    /// them mean the same thing, which is exactly the shape a condition gets
    /// wrong by naming only the one it happened to think of.
    ///
    /// Values, not absence of panic: every code is asserted to the exit it
    /// maps to.
    #[test]
    fn the_wait_code_decides_the_exit() {
        use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_FAILED, WAIT_TIMEOUT};

        assert_eq!(after_wait(WAIT_OBJECT_0), AfterWait::Collect);
        // Index 1 of the wait array is the session's cancel event.
        assert_eq!(
            after_wait(WAIT_EVENT(WAIT_OBJECT_0.0 + 1)),
            AfterWait::Abandon
        );
        // Cannot arrive under INFINITE, and is not assumed away.
        assert_eq!(after_wait(WAIT_TIMEOUT), AfterWait::Abandon);
        // Belongs to mutexes rather than to events, and is classified anyway.
        assert_eq!(after_wait(WAIT_ABANDONED), AfterWait::Abandon);
        assert_eq!(after_wait(WAIT_FAILED), AfterWait::Abandon);
    }

    /// A signalled cancel releases a thread waiting on it, and the join returns.
    ///
    /// This is the property `Drop` depends on and the one whose absence would
    /// hang the whole program: the session signals `cancel`, then joins the
    /// reader, then closes the handle. If the wait did not release, the join
    /// would never return and closing the panel would freeze the process.
    ///
    /// The transport under it — `ReadFile` on a HID handle — needs a device and
    /// is not exercised here. What is exercised is the part that has nothing to
    /// do with the device: that `Event`, `SendHandle` and the wait agree well
    /// enough for a thread to be brought down on demand.
    #[test]
    fn a_signalled_cancel_releases_the_thread_waiting_on_it() {
        let cancel = Event::new(true).expect("a manual-reset event");
        let crossing = SendHandle(cancel.raw());

        let released = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&released);
        let waiter = std::thread::spawn(move || {
            let handle = crossing;
            // SAFETY: the event outlives this thread — the join below happens
            // before `cancel` is dropped, which is the ordering the session's
            // own `Drop` keeps. The timeout bounds the test rather than the
            // production path, which waits without one.
            let waited = unsafe { WaitForSingleObject(handle.0, 5_000) };
            flag.store(waited == WAIT_OBJECT_0, Ordering::Release);
        });

        cancel.signal();
        waiter
            .join()
            .expect("a thread waiting on cancel must be joinable once signalled");
        assert!(
            released.load(Ordering::Acquire),
            "the wait returned for some reason other than the cancel being \
             signalled, so a session Drop could block on the join"
        );
    }

    /// Cancel latches; a read completion does not.
    ///
    /// The two events are created with different reset modes and the difference
    /// is load-bearing. Cancel is manual-reset because it must stay signalled
    /// for every subsequent wait — a reader that checks it, loops, and checks
    /// again has to see it both times. The completion event is auto-reset so
    /// each read starts unsignalled; were it manual, the second read would see
    /// the first one's completion and treat an empty buffer as a reply.
    #[test]
    fn only_the_cancel_event_stays_signalled() {
        let latching = Event::new(true).expect("a manual-reset event");
        latching.signal();
        // SAFETY: both waits are on an event this scope owns; a zero timeout
        // polls rather than blocks.
        let (first, second) = unsafe {
            (
                WaitForSingleObject(latching.raw(), 0),
                WaitForSingleObject(latching.raw(), 0),
            )
        };
        assert_eq!(first, WAIT_OBJECT_0);
        assert_eq!(
            second, WAIT_OBJECT_0,
            "cancel must stay latched, or a reader that loops sees it once"
        );

        let consuming = Event::new(false).expect("an auto-reset event");
        consuming.signal();
        // SAFETY: as above.
        let (first, second) = unsafe {
            (
                WaitForSingleObject(consuming.raw(), 0),
                WaitForSingleObject(consuming.raw(), 0),
            )
        };
        assert_eq!(first, WAIT_OBJECT_0);
        assert_ne!(
            second, WAIT_OBJECT_0,
            "a read completion must be consumed by the wait, or the next read \
             reports the previous one's result"
        );
    }

    /// The command goes out only after a gate edge and a drain, in that order.
    ///
    /// The order is the protocol, not a style: the gate lapses after seconds of
    /// silence, so the edge has to be immediately before the command; and the
    /// queue has to be emptied after the edge, or a status report that was
    /// already waiting is read as the answer to a command that had not been
    /// sent yet.
    #[test]
    fn the_command_is_sent_only_after_a_gate_edge_and_a_drain() {
        let mut channel = FakeChannel::acking();
        let abort = AtomicBool::new(false);

        // A verdict that differs from the standing one, so the watcher settles
        // on the first read instead of sitting out the whole 30 s window — the
        // sequence up to the command is what this test is about, and waiting
        // for a timeout to observe it would put half a minute into the suite.
        run_protocol(&mut channel, &safe(), &abort, || settled(Some(FAILED)));

        assert_eq!(
            channel.steps(),
            vec![
                Step::GateOpened,
                Step::Drained,
                Step::Sent(CMD_SELF_TEST.to_vec()),
                Step::AckAwaited,
            ],
            "the gate edge, the drain, the command and the wait, in that order"
        );
    }

    /// A refused pre-test check sends nothing at all.
    ///
    /// This is the check that protects a battery: a test started on a low
    /// charge, or while already on battery, discharges a live load. Returning
    /// `NotSafe` is not enough — nothing may reach the device, not even the
    /// gate edge.
    #[test]
    fn an_unsafe_device_is_never_touched() {
        let mut channel = FakeChannel::acking();
        let abort = AtomicBool::new(false);
        let unsafe_state = safety(Some(10), true, false, Some(PASSED));

        let outcome = run_protocol(&mut channel, &unsafe_state, &abort, || {
            panic!("`Test` must not be read for a device that was never commanded")
        });

        assert_eq!(outcome, SelfTestOutcome::NotSafe);
        assert!(
            channel.steps().is_empty(),
            "a refused test must not reach the device at all"
        );
    }

    /// `T` sent and no `#0` back is not a test that is running.
    ///
    /// The firmware answers `#0` when it takes `T` as a start command, and a
    /// status summary or silence when it does not. Reporting that as a started
    /// test would leave the panel watching a `Test` value that nothing is going
    /// to move, for the full 30 s window, and then call it a timeout.
    #[test]
    fn a_command_that_is_not_acknowledged_is_not_a_running_test() {
        let mut channel = FakeChannel::default();
        let abort = AtomicBool::new(false);

        let outcome = run_protocol(&mut channel, &safe(), &abort, || {
            panic!("`Test` must not be watched for a command that was refused")
        });

        assert_eq!(outcome, SelfTestOutcome::NotAcknowledged);
        assert_eq!(
            channel.steps().last(),
            Some(&Step::AckAwaited),
            "the command was sent, so the sequence must stop at the wait"
        );
    }

    /// A failure to drive the gate is reported as a channel error, and stops
    /// the run before the command.
    #[test]
    fn a_gate_that_will_not_open_stops_the_run() {
        let mut channel = FakeChannel {
            gate_fails: true,
            ..FakeChannel::acking()
        };
        let abort = AtomicBool::new(false);

        let outcome = run_protocol(&mut channel, &safe(), &abort, || {
            panic!("`Test` must not be watched when the channel never opened")
        });

        assert_eq!(outcome, SelfTestOutcome::ChannelError);
        assert!(
            channel.steps().is_empty(),
            "nothing follows a gate that did not open"
        );
    }

    /// A failed write is a channel error, not a refusal by the device.
    ///
    /// The two are told apart because they call for different words on screen:
    /// a device that refused the command is working, a write that did not go
    /// out is not.
    #[test]
    fn a_send_that_fails_is_a_channel_error() {
        let mut channel = FakeChannel {
            send_fails: true,
            ..FakeChannel::acking()
        };
        let abort = AtomicBool::new(false);

        let outcome = run_protocol(&mut channel, &safe(), &abort, || {
            panic!("`Test` must not be watched for a command that never went out")
        });

        assert_eq!(outcome, SelfTestOutcome::ChannelError);
        assert_eq!(
            channel.steps(),
            vec![Step::GateOpened, Step::Drained],
            "the run stops at the write"
        );
    }

    /// A shutdown between the gate edge and the command cancels the run.
    ///
    /// Every step that can block for seconds re-checks the flag, so quitting
    /// mid test does not wait out the 30 s window. The gate is still closed
    /// afterwards — by the session's `Drop`, which is why it is a type.
    #[test]
    fn a_shutdown_before_the_command_cancels_the_run() {
        let mut channel = FakeChannel::acking();
        let abort = AtomicBool::new(true);

        let outcome = run_protocol(&mut channel, &safe(), &abort, || {
            panic!("a cancelled run watches nothing")
        });

        assert_eq!(outcome, SelfTestOutcome::Cancelled);
        assert_eq!(
            channel.steps(),
            vec![Step::GateOpened],
            "the edge had already gone out; nothing after it should"
        );
    }

    /// An acknowledged command is watched to a verdict, and the verdict is the
    /// device's.
    #[test]
    fn an_acknowledged_command_is_watched_to_its_result() {
        let mut channel = FakeChannel::acking();
        let abort = AtomicBool::new(false);

        let outcome = run_protocol(&mut channel, &safe(), &abort, || settled(Some(FAILED)));

        assert_eq!(outcome, SelfTestOutcome::Failed);
    }

    /// A device in a state that permits a test: a full pack, on mains, not
    /// discharging, with a passed test standing from before.
    fn safe() -> Safety {
        safety(Some(100), true, false, Some(PASSED))
    }

    /// `Test` codes the watcher reads as verdicts, named so the protocol tests
    /// read as outcomes rather than as numbers. Their mapping is
    /// `terminal_outcome`'s, pinned by `terminal_codes_map_to_outcomes`.
    const PASSED: u32 = 1;
    const FAILED: u32 = 3;

    /// A vendor channel with no device behind it.
    ///
    /// Every step of the protocol is scripted rather than performed: the gate
    /// edge is a bool, `send` records the frame, `acknowledged` answers
    /// whatever the test set. What is being checked is the *conversation* —
    /// which steps happen, in what order, and what each answer leads to — which
    /// is the part that cannot be checked against a real UPS without
    /// discharging a battery under load to find out.
    #[derive(Default)]
    struct FakeChannel {
        /// What `acknowledged` answers. `None` is silence, which is a refusal.
        ack: Option<Ack>,
        /// Whether the gate edge fails instead of opening.
        gate_fails: bool,
        /// Whether the send fails instead of going out.
        send_fails: bool,
        /// Every step performed, in order, so a test can assert on the sequence
        /// rather than only on its outcome. Behind a `RefCell` because the
        /// trait lends the channel out by shared reference for the steps that
        /// do not change it — the real one writes to a device through a handle,
        /// which needs no `&mut`, and the fake must not widen the trait to suit
        /// its own bookkeeping.
        log: RefCell<Vec<Step>>,
    }

    /// One step of the protocol, as the fake observed it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Step {
        GateOpened,
        Drained,
        Sent(Vec<u8>),
        AckAwaited,
    }

    impl FakeChannel {
        /// A channel that acknowledges the command, which is what every test
        /// not about the acknowledgement wants.
        fn acking() -> Self {
            Self {
                ack: Some(Ack::Ok),
                ..Self::default()
            }
        }

        /// The steps performed, in order.
        fn steps(&self) -> Vec<Step> {
            self.log.borrow().clone()
        }
    }

    impl VendorChannel for FakeChannel {
        fn open_gate(&mut self, _abort: &AtomicBool) -> Result<()> {
            if self.gate_fails {
                return Err(Error::SelfTestChannel("gate write failed".to_owned()));
            }
            self.log.borrow_mut().push(Step::GateOpened);
            Ok(())
        }

        fn gate_report(&self) -> u8 {
            37
        }

        fn drain(&self) {
            self.log.borrow_mut().push(Step::Drained);
        }

        fn send(&self, payload: &[u8]) -> Result<Instant> {
            if self.send_fails {
                return Err(Error::SelfTestChannel("short write".to_owned()));
            }
            self.log.borrow_mut().push(Step::Sent(payload.to_vec()));
            Ok(Instant::now())
        }

        fn out_report(&self) -> u8 {
            41
        }

        fn acknowledged(&self, _sent_at: Instant, _abort: &AtomicBool) -> Ack {
            self.log.borrow_mut().push(Step::AckAwaited);
            self.ack.unwrap_or(Ack::None)
        }
    }

    /// A `Safety` snapshot built from the four readings the guards consult.
    ///
    /// `test_before` is the raw pre-test `Test` value; `None` stands for a
    /// device that has never been tested this session as much as for a failed
    /// read, and neither blocks a start.
    fn safety(
        charge: Option<u32>,
        ac: bool,
        discharging: bool,
        test_before: Option<u32>,
    ) -> Safety {
        Safety {
            charge_percent: charge,
            ac_present: Some(ac),
            discharging: Some(discharging),
            test_before,
        }
    }

    #[test]
    fn a_full_battery_on_mains_is_safe() {
        assert!(safety(Some(100), true, false, None)
            .refusal_reason()
            .is_none());
        assert!(safety(Some(90), true, false, None)
            .refusal_reason()
            .is_none());
        // A previous result standing in the register is not a running test.
        assert!(safety(Some(100), true, false, Some(1))
            .refusal_reason()
            .is_none());
    }

    #[test]
    fn a_low_battery_is_not_safe() {
        assert!(safety(Some(89), true, false, None)
            .refusal_reason()
            .is_some());
    }

    #[test]
    fn an_unknown_charge_is_not_safe() {
        // A test drains the battery; a charge that could not be read is not
        // evidence the pack is full, so it must block rather than default open.
        assert!(safety(None, true, false, None).refusal_reason().is_some());
    }

    #[test]
    fn no_mains_is_not_safe() {
        assert!(safety(Some(100), false, false, None)
            .refusal_reason()
            .is_some());
    }

    #[test]
    fn already_discharging_is_not_safe() {
        assert!(safety(Some(100), true, true, None)
            .refusal_reason()
            .is_some());
    }

    #[test]
    fn a_running_test_blocks_another() {
        assert!(safety(Some(100), true, false, Some(TEST_IN_PROGRESS))
            .refusal_reason()
            .is_some());
    }

    /// C5: an unread mains flag must block the test, not proceed. Reading it as
    /// present because the field happened to come back false-ish was the unsafe
    /// direction — a test started on a UPS whose mains state was actually
    /// unknown.
    #[test]
    fn unknown_mains_is_not_safe() {
        let s = Safety {
            charge_percent: Some(100),
            ac_present: None,
            discharging: Some(false),
            test_before: None,
        };
        let reason = s.refusal_reason().expect("unknown mains must refuse");
        assert!(
            reason.contains("mains"),
            "reason should name mains: {reason}"
        );
    }

    /// C5: likewise an unread discharge flag must block. This is the worse of
    /// the two: reading it as `false` let a test start on a pack that might
    /// already be discharging.
    #[test]
    fn unknown_discharge_is_not_safe() {
        let s = Safety {
            charge_percent: Some(100),
            ac_present: Some(true),
            discharging: None,
            test_before: None,
        };
        assert!(s.refusal_reason().is_some());
    }

    #[test]
    fn refusal_reason_names_the_specific_cause() {
        // The reason is what the debug log logs, so it must be the specific
        // failing check, not a generic "not safe".
        assert!(safety(Some(50), true, false, None)
            .refusal_reason()
            .unwrap()
            .contains("50%"));
        assert!(safety(None, true, false, None)
            .refusal_reason()
            .unwrap()
            .contains("unknown"));
        assert!(safety(Some(100), false, false, None)
            .refusal_reason()
            .unwrap()
            .contains("mains"));
        assert!(safety(Some(100), true, true, None)
            .refusal_reason()
            .unwrap()
            .contains("discharging"));
    }

    #[test]
    fn hex_caps_and_marks_long_payloads() {
        assert_eq!(hex(&[0x23, 0x30]), "23 30");
        assert_eq!(hex(&[]), "(empty)");
        assert!(hex(&[0u8; 40]).contains('…'));
        // Exactly the cap is not over it. The mark says the line is short of
        // the reply; saying so about a reply that is entirely on the line
        // sends the reader looking for bytes that were never withheld.
        assert!(
            !hex(&[0u8; 16]).contains('…'),
            "a payload that fits is not marked as cut"
        );
    }

    /// A tick reporting `code` from a UPS whose load is already back on mains.
    ///
    /// The settle wait ends on the tick it is handed when the flag reads clear,
    /// so a test that is not about settling says so once here rather than
    /// spelling out a struct at every call.
    fn settled(code: Option<u32>) -> Tick {
        Tick {
            test: code,
            discharging: Some(false),
        }
    }

    /// The observation window runs forward from the acknowledgement, and a
    /// result inside it is a result rather than a timeout.
    ///
    /// The deadline is the one thing in `watch` that the reader closure cannot
    /// influence, and both ways of getting it wrong are silent: a window that
    /// starts in the past reports `Timeout` on a test the device completed —
    /// the panel then says the self-test did not finish while the UPS says it
    /// passed — and a window that never expires leaves the thread polling a
    /// device that has stopped answering.
    ///
    /// One real second is spent here, which is the poll interval: the second
    /// read has to happen on the next tick, and shortening the interval for
    /// the test would be testing a different function.
    #[test]
    fn a_result_arriving_inside_the_window_is_not_a_timeout() {
        let abort = AtomicBool::new(false);
        let reads = std::cell::Cell::new(0);
        let mut read_test = || {
            reads.set(reads.get() + 1);
            // In progress on the first tick, the verdict on the second.
            if reads.get() == 1 {
                settled(Some(TEST_IN_PROGRESS))
            } else {
                settled(Some(PASSED))
            }
        };

        let (outcome, last) = watch(Instant::now(), Some(FAILED), &abort, &mut read_test);

        assert_eq!(outcome, SelfTestOutcome::Passed);
        assert_eq!(reads.get(), 2, "the second read is the one that decides");
        // The tick that decided, handed on whole. The settle wait that follows
        // ends on it when the load is already back, so a fabricated one would
        // answer for a device nobody asked.
        assert_eq!(
            last,
            settled(Some(PASSED)),
            "the verdict travels with the tick it was read from"
        );
    }

    /// A window that has expired ends the wait, and the line says what state
    /// the device was left in.
    ///
    /// `started_at` is a parameter precisely so the expiry can be reached
    /// without waiting out the real window. The state in the message is the
    /// last code actually read: a timeout reported as "state unknown" against
    /// a device that spent thirty seconds in state 5 loses the one fact that
    /// distinguishes a slow test from a device that never started one.
    #[test]
    fn an_expired_window_times_out_and_names_the_state_it_left() {
        let _guard = crate::testsupport::serial_log();
        let was_on = crate::evlog::debug_enabled();
        crate::evlog::set_debug(true);

        let started_at = Instant::now()
            .checked_sub(POLL_TIMEOUT)
            .expect("the process has been running longer than the poll window");
        let abort = AtomicBool::new(false);
        let log = crate::evlog::path();
        let before = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);

        let (outcome, _) = watch(started_at, Some(FAILED), &abort, &mut || {
            settled(Some(TEST_IN_PROGRESS))
        });

        let bytes = std::fs::read(&log).unwrap_or_default();
        let from = usize::try_from(before).unwrap_or(usize::MAX);
        let trace = String::from_utf8_lossy(bytes.get(from..).unwrap_or_default()).into_owned();
        crate::evlog::set_debug(was_on);

        assert_eq!(outcome, SelfTestOutcome::Timeout);
        assert!(
            trace.contains(&format!("in state {TEST_IN_PROGRESS}")),
            "the timeout must name the state the device was left in: {trace}"
        );
    }

    /// The session does not end while the load is still on the inverter.
    ///
    /// The verdict and the end of the run are different moments: the firmware
    /// turns `Test` from 5 to its result as soon as it has decided, and the UPS
    /// then takes a second or two to move the load back to mains. Ending here
    /// left `PresentStatus.Discharging` set — which is exactly what the panel
    /// refuses to start a new test on — so the buzzer button came back live
    /// while the self-test button beside it stayed greyed.
    #[test]
    fn the_session_waits_for_the_load_to_come_off_the_inverter() {
        let abort = AtomicBool::new(false);
        let ticks = std::cell::Cell::new(0usize);
        let mut tick = || {
            ticks.set(ticks.get() + 1);
            Tick {
                test: Some(PASSED),
                // Still on the inverter for the first two ticks; back on
                // mains on the third.
                discharging: Some(ticks.get() < 3),
            }
        };

        settle(
            Instant::now(),
            &abort,
            Tick {
                test: Some(PASSED),
                discharging: Some(true),
            },
            &mut tick,
        );

        assert_eq!(
            ticks.get(),
            3,
            "the wait ends on the tick that reports the load back on mains"
        );
    }

    /// A load already on mains costs no poll and no wait.
    ///
    /// Every outcome where the UPS never transferred — a refused command, a
    /// channel that would not open — arrives here with the flag already clear,
    /// and must not spend a device transaction to learn what it was told.
    #[test]
    fn a_load_already_on_mains_is_not_waited_for() {
        let abort = AtomicBool::new(false);
        settle(Instant::now(), &abort, settled(Some(PASSED)), &mut || {
            panic!("a settled load must not be polled again")
        });
    }

    /// An unreadable discharge flag is not read as "settled".
    ///
    /// The wait exists to leave the device in a state the panel can act on,
    /// and "could not read it" is not that state. It ends on its own deadline
    /// instead — which the abort flag stands in for here, since waiting out
    /// `SETTLE_TIMEOUT` in a test would spend fifteen real seconds.
    ///
    /// The assertion is on the *reason* the wait ended, not on whether it
    /// polled again. Both a wait that gave up on the abort and a wait that
    /// never started poll nothing, so counting polls cannot tell them apart —
    /// and "never started" is precisely the mistake, an unknown flag taken for
    /// a clear one. The trace says which happened; nothing else does.
    #[test]
    fn an_unreadable_discharge_flag_does_not_end_the_wait() {
        let _guard = crate::testsupport::serial_log();
        let was_on = crate::evlog::debug_enabled();
        crate::evlog::set_debug(true);
        let log = crate::evlog::path();
        let before = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);

        let abort = AtomicBool::new(true);
        settle(
            Instant::now(),
            &abort,
            Tick {
                test: Some(PASSED),
                discharging: None,
            },
            &mut || settled(Some(PASSED)),
        );

        let bytes = std::fs::read(&log).unwrap_or_default();
        let from = usize::try_from(before).unwrap_or(usize::MAX);
        let trace = String::from_utf8_lossy(bytes.get(from..).unwrap_or_default()).into_owned();
        crate::evlog::set_debug(was_on);

        assert!(
            trace.contains("shutting down before the load returned to mains"),
            "an unknown flag keeps waiting, so the abort is what ends it: {trace}"
        );
        assert!(
            !trace.contains("load back on mains"),
            "an unknown flag was reported as a load that had come back: {trace}"
        );
    }

    /// A load that never comes back does not hold the session forever.
    ///
    /// The device can stop answering, or answer `Discharging` indefinitely
    /// after a test that ended badly. Without the deadline the poll thread
    /// would watch it for as long as it kept saying so, and the panel would
    /// show a test in progress that nothing was ever going to finish.
    ///
    /// `began` is placed in the past rather than waiting out the real window,
    /// which is why it is a parameter.
    #[test]
    fn a_load_that_never_returns_gives_up_on_the_deadline() {
        let _guard = crate::testsupport::serial_log();
        let was_on = crate::evlog::debug_enabled();
        crate::evlog::set_debug(true);
        let log = crate::evlog::path();
        let before = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);

        let began = Instant::now()
            .checked_sub(SETTLE_TIMEOUT)
            .expect("the process has been running longer than the settle window");
        let abort = AtomicBool::new(false);
        settle(
            began,
            &abort,
            Tick {
                test: Some(FAILED),
                discharging: Some(true),
            },
            &mut || panic!("an expired wait must not poll the device again"),
        );

        let bytes = std::fs::read(&log).unwrap_or_default();
        let from = usize::try_from(before).unwrap_or(usize::MAX);
        let trace = String::from_utf8_lossy(bytes.get(from..).unwrap_or_default()).into_owned();
        crate::evlog::set_debug(was_on);

        assert!(
            trace.contains(&format!(
                "load still on battery {} s after the verdict",
                SETTLE_TIMEOUT.as_secs()
            )),
            "the wait must say it gave up and how long it waited: {trace}"
        );
    }

    /// The sleep lasts, and an abort cuts it short.
    ///
    /// Both halves are invisible from the outside. A sleep that returns at
    /// once turns the one-second poll of `Test` into a spin over a USB control
    /// transfer for the whole thirty-second window; a sleep that ignores the
    /// flag holds the shutdown for up to a second per tick, which is what the
    /// slicing exists to avoid.
    #[test]
    fn the_sleep_lasts_unless_it_is_aborted() {
        const NAP: Duration = Duration::from_millis(60);

        let quiet = AtomicBool::new(false);
        let started = Instant::now();
        interruptible_sleep(NAP, &quiet);
        assert!(
            started.elapsed() >= NAP / 2,
            "an uninterrupted sleep must actually sleep: {:?}",
            started.elapsed()
        );

        let raised = AtomicBool::new(true);
        let started = Instant::now();
        interruptible_sleep(Duration::from_secs(5), &raised);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a raised abort must be seen within a slice, not a whole sleep: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn terminal_codes_map_to_outcomes() {
        assert_eq!(terminal_outcome(1), Some(SelfTestOutcome::Passed));
        assert_eq!(
            terminal_outcome(2),
            Some(SelfTestOutcome::PassedWithWarning)
        );
        assert_eq!(terminal_outcome(3), Some(SelfTestOutcome::Failed));
        assert_eq!(terminal_outcome(4), Some(SelfTestOutcome::Aborted));
    }

    /// 5, 6 and 7 are states, not results, and reporting them as one is a lie
    /// in both directions.
    ///
    /// The previous mapping sent every unrecognised code to `Failed`, so a
    /// `Test` still reading 6 "never run" — exactly what a device that ignored
    /// the command leaves standing — came back to the user as "the self-test
    /// failed".
    #[test]
    fn non_terminal_codes_are_not_results() {
        assert_eq!(terminal_outcome(5), None, "5 is in progress");
        assert_eq!(terminal_outcome(6), None, "6 is never run");
        assert_eq!(terminal_outcome(7), None, "7 is scheduled");
        assert_eq!(terminal_outcome(0), None);
        assert_eq!(terminal_outcome(99), None);
    }

    /// The defect this rule replaces: a code left over from the *previous* test
    /// must never be reported as this test's result.
    ///
    /// The device acknowledges `T` in 40–48 ms and enters state 5 an unmeasured
    /// while later. Until it does, `Test` still holds whatever the last test
    /// left — typically 1 "Passed". The old watcher accepted any terminal code
    /// seen more than a second after the command, so a firmware slow to start
    /// produced "Passed" for a test that had not run at all.
    #[test]
    fn the_previous_result_is_not_reported_as_this_ones() {
        // Passed standing before the command, still standing now: nothing has
        // been learned about the run just started.
        assert_eq!(progress_of(Some(1), Some(1), false), Progress::Waiting);
        // Same for a device that has never been tested and still says so.
        assert_eq!(progress_of(Some(6), Some(6), false), Progress::Waiting);
        // A failed read tells us nothing either.
        assert_eq!(progress_of(None, Some(1), false), Progress::Waiting);
    }

    /// Two things attribute a result to this run, and only these two.
    #[test]
    fn a_result_is_attributed_by_change_or_by_having_seen_state_five() {
        // Changed from the pre-test value: this run's own result, even though
        // state 5 was never caught between two polls a second apart.
        assert_eq!(
            progress_of(Some(3), Some(1), false),
            Progress::Finished(SelfTestOutcome::Failed)
        );
        // Unchanged, but state 5 was observed in between, so the firmware has
        // run a test and this is its verdict.
        assert_eq!(
            progress_of(Some(1), Some(1), true),
            Progress::Finished(SelfTestOutcome::Passed)
        );
        // State 5 itself is progress, never a result.
        assert_eq!(progress_of(Some(5), Some(1), false), Progress::Running);
        assert_eq!(progress_of(Some(5), Some(1), true), Progress::Running);
    }

    /// A non-terminal code is not promoted to a result by having changed.
    ///
    /// 6 "never run" appearing where 1 stood is a change, but it is still not a
    /// verdict — the watch must keep waiting and, failing anything terminal,
    /// time out rather than invent an outcome.
    #[test]
    fn a_changed_but_non_terminal_code_is_still_not_a_result() {
        assert_eq!(progress_of(Some(6), Some(1), false), Progress::Waiting);
        assert_eq!(progress_of(Some(7), Some(1), true), Progress::Waiting);
    }

    #[test]
    fn reply_payload_honours_the_count_byte() {
        // report id 40, count 2, "#0", then residue that must be ignored.
        let data = [40, 2, b'#', b'0', 0xaa, 0xbb];
        assert_eq!(payload_of(&data), b"#0");
    }

    #[test]
    fn a_short_reply_has_an_empty_payload() {
        assert_eq!(payload_of(&[40]), b"");
        assert_eq!(payload_of(&[]), b"");
    }
}
