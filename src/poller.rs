//! Background polling. Runs off the UI thread and reports via a channel.
//!
//! # Pacing
//!
//! Everything that touches the device flows through one clock: `next_contact`,
//! the earliest moment the next burst of transfers is allowed. Traffic is
//! capped at one poll per second because these units stop answering under
//! aggressive polling and recover only on a physical reconnect — so the cap
//! is a device-safety property, not a politeness. A wake (hot-plug event,
//! beeper click) therefore never pulls a poll forward while a device is
//! connected; it only lets the thread act sooner on things that are not
//! polls: commands are applied as soon as the thread is awake, and a
//! *disconnected* device may be re-probed immediately, which is the entire
//! point of the `WM_DEVICECHANGE` wake.
//!
//! The one exception is the very first poll after a successful connect: it
//! fires immediately, so the panel shows a reading at once instead of an empty
//! skeleton held for a whole interval. That is a single startup pair of
//! contacts — connect, then one poll — not sustained fast polling, so it does
//! not threaten the device the way a standing sub-second cadence would. Every
//! poll after it goes back through `next_contact` and the one-per-second floor.
//!
//! # Sleeping
//!
//! The thread parks until the deadline and is unparked by the UI when there
//! is a reason to look up early. The previous version slept in 100 ms slices
//! and re-checked flags on each — ten wake-ups a second, forever, on a
//! program whose UI thread goes to lengths to idle at zero. `park_timeout`
//! gives the same prompt reaction to `stop` and `wake` without the metronome.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::hid::{Beeper, Identity, Reading, SelfTestOutcome, Ups};
/// Three consecutive read failures mark the device as disconnected.
const FAILURES_BEFORE_DISCONNECT: u32 = 3;

/// How long to wait between attempts to reach a device that is not answering.
///
/// A type rather than an index into a table, because the index was being kept
/// by hand in one place and the table read directly in two others: the shortest
/// step appeared as an index into that table at the hot-plug floor and again at
/// the probe after a run of failed reads, so one idea — "the closest together
/// two attempts may ever be" — was spelled three ways and could be changed in
/// one of them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    step: usize,
}

impl Backoff {
    /// The schedule, in seconds. The last value repeats indefinitely: a device
    /// that has been absent for a minute is probably absent, and probing it
    /// every ten seconds forever costs nothing while staying responsive to a
    /// plug that never generates an event.
    const SECS: [u64; 4] = [1, 2, 5, 10];

    /// The shortest step, and therefore the floor on how often the device may
    /// be probed however the attempt was provoked.
    ///
    /// The hot-plug wake is what makes this a floor rather than a schedule.
    /// The notification subscription covers the whole HID class, so any mouse
    /// or receiver being plugged in — or one device flapping — arrives here,
    /// and collapsing the deadline all the way to "now" on each one drove a
    /// full interface enumeration plus a `connect` at whatever rate the events
    /// came, with the backoff having no effect at all.
    const FLOOR: Duration = Duration::from_secs(Self::SECS[0]);

    /// The wait before the next attempt, advancing the schedule by one step.
    ///
    /// Saturating at the last entry rather than wrapping: the point of a
    /// backoff is that it stops growing, not that it starts over.
    fn take(&mut self) -> Duration {
        // The last entry is the saturation point, so it is also the answer for
        // any step at or past the end. Taken with `get` and that same last
        // entry as the fallback, the clamp is stated once instead of as a
        // `len() - 1` written twice and subtracted from a length that a table
        // of zero entries would make wrap.
        let secs = Self::SECS
            .get(self.step)
            .or_else(|| Self::SECS.last())
            .copied()
            .unwrap_or(1);
        self.step = (self.step + 1).min(Self::SECS.len().saturating_sub(1));
        Duration::from_secs(secs)
    }
}

/// The earliest the next poll may start after this pass touched the device.
///
/// A write is a contact, so the spacing runs from it. The configured interval
/// spaces one poll from the *previous poll*; it has nothing to say about a poll
/// that follows a write, and letting it govern that one is what made a buzzer
/// click take a whole interval to show its result. On a five-second interval a
/// user pressed the button and watched the row sit unchanged for five seconds;
/// on a minute, for a minute.
///
/// This used to be `planned.max(now + floor)`, and the `max` was justified by a
/// pass that wrote while a long backoff was pending. That cannot happen: a
/// backoff is only ever installed by a failed *connect*, and every write path
/// requires an open device — `plan_beeper` refuses without one, and the
/// self-test branch is inside `if let Some(device)`. So the `max` never
/// preserved a backoff; it preserved the ordinary interval, which is the one
/// thing it must not do here.
///
/// The floor is measured from now, and now is the moment of the write, so the
/// device still gets its full second of quiet after its last transfer. That is
/// what the floor is for, and it is untouched.
fn after_contact(now: Instant) -> Instant {
    now + Duration::from_millis(u64::from(crate::config::POLL_MIN_MS))
}

/// The deadline a hot-plug wake collapses to, while no device is connected.
///
/// A wake means "look up early", not "look up now". It may shorten the
/// reconnect backoff — hot-plug is the reason the wake exists, and there is no
/// pacing to protect on a device that is not being talked to — but not below
/// [`Backoff::FLOOR`] measured from the last attempt. `None` means no attempt
/// has been made yet, and nothing to pace against.
fn after_wake(now: Instant, last_attempt: Option<Instant>) -> Instant {
    let floor = last_attempt.map_or(now, |t| t + Backoff::FLOOR);
    now.max(floor)
}

/// The configured interval, floored at one poll per second.
///
/// Some CyberPower units stop responding under aggressive polling until they
/// are physically reconnected, so the floor is enforced here regardless of what
/// the configuration says.
fn paced_interval(control: &Control) -> Duration {
    let ms = control
        .interval_ms
        .load(Ordering::Relaxed)
        .max(crate::config::POLL_MIN_MS);
    Duration::from_millis(u64::from(ms))
}

pub(crate) enum Message {
    Connected {
        identity: Box<Identity>,
        ambiguous: bool,
    },
    Update(Box<Reading>),
    /// The result of the read that confirms a buzzer write.
    ///
    /// An observation of the mode, exactly like the one a poll carries in
    /// [`Reading::beeper`], and `App` folds the two through one entry point.
    /// `None` is an attempt that came back with nothing, and it is reported
    /// rather than swallowed: a write went through whose result nobody could
    /// read, and a row still claiming the old mode as current would be saying
    /// more than is known.
    ///
    /// This variant existed before, with this signature, and was deleted for
    /// causing the buzzer row to lose its control until the next restart. What
    /// made it dangerous is gone, and neither part of it was the `Option`. It
    /// was the *only* source of the mode — nothing else read report 12, so a
    /// `None` stood for the rest of the session — and it landed in a field
    /// that could hold nothing, so the mode was discarded rather than
    /// demoted. The mode is polled now, and the field is a
    /// [`crate::app::BeeperView`], which has no state that has forgotten it.
    ///
    /// What it buys is the click feeling immediate: the confirming read
    /// happens whether or not anyone listens, so this costs no transfer and
    /// answers in milliseconds, where the poll behind it answers in a second.
    BeeperRead(Option<Beeper>),
    /// A self-test has started (`true`) or finished (`false`). Sent so the
    /// panel can grey the button and show progress for the run's duration
    /// without polling for the state itself.
    SelfTestRunning(bool),
    /// The outcome of a finished self-test.
    SelfTestResult(SelfTestOutcome),
    Disconnected,
    /// A device was found but could not be brought up. Carries the localisable
    /// category, not a raw string: the panel shows a translated line while the
    /// log keeps the full technical detail.
    Failed(crate::error::ConnectFailure),
}

/// Commands sent to the poll thread. The HID handle lives there, so writes
/// must be routed rather than performed from the UI thread.
pub(crate) enum Command {
    /// Change the buzzer to whatever it is not.
    ///
    /// An intent, not a state. The target is worked out on the poll thread
    /// from the mode it reads at the moment it acts, because that thread owns
    /// the handle and is the only place the current mode can be asked for and
    /// used in the same breath. Naming the target here instead meant naming it
    /// from the panel's copy — a value that is stale whenever commands are
    /// queued behind a self-test, and stale anyway whenever the buzzer is
    /// changed from the front panel of the UPS.
    ToggleBeeper,
    /// Run a self-test. Like the buzzer, this touches the device and so is
    /// executed on the poll thread that owns the handle.
    RunSelfTest,
}

/// Control state shared between the UI thread and the poll thread.
///
/// One `Arc` holding three atomics, rather than three `Arc`s holding one each.
/// They are always created together, always cloned together and always passed
/// together, so splitting them bought nothing and cost three allocations, three
/// clone lines, and three more parameters on `run` — enough to need a
/// `too_many_arguments` suppression, which is the smell that pointed here.
///
/// Atomics rather than a mutex or a channel: the UI thread must never block on
/// the poll thread, and every field is a single value with no invariant tying
/// it to the others. The store is paired with an `unpark` of the poll thread,
/// which is the flag's wake-up — the same rule the UI side states as "every
/// flag store is followed by a wake".
struct Control {
    /// Poll interval in milliseconds. Read when the next contact is
    /// scheduled, so a change from the settings dialog takes effect on the
    /// next cycle.
    interval_ms: AtomicU32,
    /// Asks the poll thread to exit.
    ///
    /// Behind its own `Arc` because it is consulted in three places that do
    /// not share a scope: the parking loop here, the self-test protocol, and —
    /// since the seventh audit — the series of control transfers inside
    /// `connect` and `poll`. The device layer must be able to hold it for as
    /// long as it holds the device, and it must not be made to know what a
    /// `Control` is to do so.
    stop: Arc<AtomicBool>,
    /// Asks the thread to look up before the deadline: apply queued commands,
    /// or re-probe a disconnected device without waiting out the backoff.
    wake: AtomicBool,
}

pub(crate) struct PollHandle {
    pub rx: Receiver<Message>,
    tx_cmd: Sender<Command>,
    control: Arc<Control>,
    /// Kept so signals can unpark the thread, and so shutdown can join it —
    /// which is what guarantees a beeper write queued just before exit is
    /// carried out rather than cut off mid-transfer by process teardown.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PollHandle {
    fn unpark(&self) {
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }

    pub(crate) fn set_interval(&self, ms: u32) {
        self.control.interval_ms.store(ms, Ordering::Relaxed);
        // No unpark: the new interval applies when the next contact is
        // scheduled, exactly as documented on the field. Waking early here
        // would poll ahead of the pace the old interval promised the device.
    }

    /// Called on `WM_DEVICECHANGE`. A disconnected device is re-probed at once
    /// instead of waiting out the backoff; a connected one is not polled
    /// early — the once-per-second cap holds.
    pub(crate) fn wake(&self) {
        self.control.wake.store(true, Ordering::Relaxed);
        self.unpark();
    }

    pub(crate) fn stop(&self) {
        self.control.stop.store(true, Ordering::Relaxed);
        self.unpark();
    }

    /// Stops the thread and waits for it to finish its current step.
    ///
    /// The join is the point: `stop` alone lets `main` return while a beeper
    /// write or a log line is mid-flight, and process exit then kills the
    /// thread wherever it happens to be. The thread checks `stop` at every
    /// park and between steps, so the wait is bounded by one device
    /// transaction burst.
    pub(crate) fn stop_and_join(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Queues a buzzer toggle; applied as soon as the poll thread is awake.
    pub(crate) fn toggle_beeper(&self) {
        let _ = self.tx_cmd.send(Command::ToggleBeeper);
        self.control.wake.store(true, Ordering::Relaxed);
        self.unpark();
    }

    /// Queues a self-test; run as soon as the poll thread is awake. The run
    /// takes up to half a minute and holds the poll thread for its duration —
    /// the panel is told it started and finished via `SelfTestRunning`.
    ///
    /// Returns whether the request reached a thread that will answer it. The
    /// caller raises its "a test is running" flag on the strength of that
    /// answer, and only a message from the poll thread lowers it again — so a
    /// request queued into a channel nobody reads would latch the flag
    /// permanently. That is not hypothetical: `spawn` keeps the handle usable
    /// when the thread could not be started (the utility comes up and logs why
    /// rather than dying), and the command channel still accepts sends.
    /// Reporting `false` here is what keeps the flag and reality in step.
    #[must_use]
    pub(crate) fn run_self_test(&self) -> bool {
        if self.thread.is_none() {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "self-test not queued: the polling thread is not running",
            );
            return false;
        }
        if self.tx_cmd.send(Command::RunSelfTest).is_err() {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "self-test not queued: the polling thread has stopped",
            );
            return false;
        }
        self.control.wake.store(true, Ordering::Relaxed);
        self.unpark();
        true
    }
}

impl Drop for PollHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A handle with no thread behind it, and the sender that feeds it.
///
/// The device side of this program is a thread and a USB handle, and neither
/// can exist in a test. What can be tested is everything downstream of the
/// messages that thread sends — the state transitions in `App`, the events
/// they fire, the icon and tooltip that follow — and this is what lets a test
/// send those messages itself.
///
/// The handle is inert on purpose. `stop`, `wake` and `set_interval` write to
/// a `Control` nobody reads, and `shutdown` finds no thread to join, so a test
/// may call the whole of `App`'s surface without a special case anywhere in
/// production code.
#[cfg(test)]
pub(crate) fn detached() -> (Sender<Message>, PollHandle) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (tx_cmd, _rx_cmd) = std::sync::mpsc::channel();
    let handle = PollHandle {
        rx,
        tx_cmd,
        control: Arc::new(Control {
            interval_ms: AtomicU32::new(1000),
            stop: Arc::new(AtomicBool::new(false)),
            wake: AtomicBool::new(false),
        }),
        thread: None,
    };
    (tx, handle)
}

pub(crate) fn spawn(
    vid: u16,
    pid: u16,
    interval_ms: u32,
    repaint: impl Fn() + Send + 'static,
) -> PollHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    let (tx_cmd, rx_cmd) = std::sync::mpsc::channel();
    let control = Arc::new(Control {
        interval_ms: AtomicU32::new(interval_ms),
        stop: Arc::new(AtomicBool::new(false)),
        wake: AtomicBool::new(false),
    });
    let thread_control = Arc::clone(&control);

    // `repaint` is passed straight through. It used to be wrapped in a
    // closure that did nothing but call it, under a local binding also named
    // `wake` — which shadowed the wake *flag* in the same scope, so two
    // unrelated things answered to one name in the code that coordinates
    // them.
    //
    // The repaint is deliberately unconditional. It was once gated on a
    // window being open, to avoid repainting a hidden one. But the tray icon
    // and its tooltip are on screen whether or not a window is, and the same
    // pass refreshes them: gating meant a reading arrived, sat unread in the
    // channel, and nothing ran — so a UPS that connected fine still showed
    // the grey "disconnected" icon until the user opened the panel and let
    // the backlog through. The icon was reporting the state of the UI, not of
    // the device. Waking with no window open is cheap by construction:
    // `tick()` skips layout when the panel is absent.
    //
    // A failed spawn would leave the utility sitting in the tray with a grey
    // icon, polling nothing, forever — the one failure a monitoring tool must
    // not have silently. The handle is still returned so the UI comes up and
    // the log says why it has no data.
    let thread = std::thread::Builder::new()
        .name("ups-poll".into())
        .spawn(move || run(vid, pid, tx, rx_cmd, &thread_control, repaint));
    let thread = match thread {
        Ok(t) => Some(t),
        Err(e) => {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                &format!("could not start the polling thread: {e}; no readings will be taken"),
            );
            None
        }
    };

    PollHandle {
        rx,
        tx_cmd,
        control,
        thread,
    }
}

/// Why a wait ended.
enum Wait {
    /// The deadline passed.
    Deadline,
    /// The wake flag was raised before the deadline.
    Woken,
    /// The stop flag was raised; the thread must exit.
    Stop,
}

/// What one drain of the command channel found.
///
/// Counts, not actions: deciding what to do with them needs to know whether a
/// device is present, and that is not a question for a loop whose job is to
/// empty a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Drained {
    /// How many buzzer toggles arrived. See [`plan_beeper`].
    beeper_toggles: usize,
    /// How many self-test requests arrived. See [`plan_self_test`].
    self_test_requests: usize,
}

/// Takes every command waiting on the channel and counts it.
///
/// Both command kinds are counted here rather than acted on, which is what
/// makes the summary a value: [`plan_beeper`] and [`plan_self_test`] decide
/// from it, and both decisions can then be checked without a device on the
/// other end.
///
/// The buzzer used to be written from inside this loop, one write per command.
/// That is what turned a queue into a burst: a self-test holds the poll thread
/// for a dozen seconds inside a single cycle, so six clicks during one arrived
/// as six commands and reached the device as six consecutive `HidD_SetFeature`
/// calls the moment the test returned. Collapsing is not possible while each
/// command is applied as it is taken off the channel — separating the two is
/// the smallest shape that admits it.
fn drain_commands(rx_cmd: &Receiver<Command>) -> Drained {
    let mut drained = Drained::default();
    while let Ok(cmd) = rx_cmd.try_recv() {
        match cmd {
            Command::ToggleBeeper => drained.beeper_toggles += 1,
            Command::RunSelfTest => drained.self_test_requests += 1,
        }
    }
    drained
}

/// What the loop should do about the buzzer toggles one drain carried.
///
/// The counts ride on the variants rather than in a field beside them, so
/// there is no figure to interpret against an action it does not describe:
/// `cancelled` is meaningful only where a write survives, `toggles` only where
/// none does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Beep {
    /// No toggle arrived. The ordinary pass: no transaction, no line.
    Idle,
    /// An odd number of toggles, so one is left over: read the current mode
    /// and write its opposite. `cancelled` is how many annulled each other
    /// first, for the log.
    Toggle { cancelled: usize },
    /// An even, non-zero number of toggles. They annul exactly and nothing is
    /// written — but the user pressed a button, so the log says why the device
    /// did not move.
    Cancelled { toggles: usize },
    /// Toggles arrived with no device to apply them to.
    NoDevice { toggles: usize },
}

/// Decides what one drain's buzzer toggles come to.
///
/// **Toggles annul in pairs; at most one write survives.** The command is an
/// intent — "change it" — not a state, so two of them are not two requests for
/// the same thing but a request and its undo. Parity is therefore the exact
/// answer rather than a heuristic: an odd count leaves one real change, an
/// even count leaves none, and either way the device sees no more than one
/// transaction.
///
/// This is what an absolute `SetBeeper(mode)` could not express. The mode was
/// computed in the panel from the state it happened to hold, and while
/// commands queued — a self-test holds the poll thread for a dozen seconds —
/// no fresh state could reach it, so six clicks produced six commands all
/// naming the same target and six identical writes. Restating a stale target
/// six times and toggling six times are different requests, and only the
/// second is what the button means.
fn plan_beeper(toggles: usize, device_present: bool) -> Beep {
    if toggles == 0 {
        return Beep::Idle;
    }
    if !device_present {
        return Beep::NoDevice { toggles };
    }
    if toggles % 2 == 0 {
        Beep::Cancelled { toggles }
    } else {
        // Every toggle but the survivor annulled another. `toggles` is at
        // least one here — zero returned above — so the subtraction cannot
        // wrap.
        Beep::Toggle {
            cancelled: toggles - 1,
        }
    }
}

/// Carries out a [`Beep`], returning whether the device was touched.
///
/// The return value means "this pass spent a device transaction", not "this
/// pass intended to": it pushes the next poll out by a whole interval, so
/// every path that reaches the wire must report it and every path that does
/// not must not.
///
/// **The current mode is read here, not carried in the command.** The panel's
/// copy is up to a poll interval old, which is not the same as what the device
/// holds: it goes stale while commands queue, and it goes stale anyway
/// whenever someone presses the button on the UPS itself. The handle
/// lives on this thread, so this is the only place where "what it is now" can
/// be asked at the moment the answer is used. The cost is one extra control
/// transfer on an explicit click, which is what buys a toggle that is correct
/// rather than probably correct.
///
/// The confirming read is forwarded as an observation, which is what makes a
/// click visible before the next poll. It is not a second kind of truth about
/// the mode: it is the same fact a poll reports, folded by the same code, into
/// a type that cannot lose it. Sending it costs nothing, because the read
/// happens either way — it is what the log line below is written from.
fn apply_beeper(plan: Beep, ups: Option<&Ups>, tx: &Sender<Message>, repaint: &impl Fn()) -> bool {
    let cancelled = match plan {
        Beep::Idle => return false,
        Beep::NoDevice { toggles } => {
            crate::evlog::event(
                crate::evlog::Cat::Device,
                &format!("{toggles} buzzer toggle(s) dropped: device disconnected"),
            );
            return false;
        }
        Beep::Cancelled { toggles } => {
            crate::evlog::event(
                crate::evlog::Cat::Device,
                &format!("{toggles} buzzer toggles annul each other; nothing written"),
            );
            return false;
        }
        Beep::Toggle { cancelled } => cancelled,
    };
    if cancelled > 0 {
        crate::evlog::event(
            crate::evlog::Cat::Device,
            &format!("{cancelled} buzzer toggle(s) annulled; one change survives"),
        );
    }
    let Some(device) = ups else {
        // Unreachable through `plan_beeper`, which answers `NoDevice` when
        // there is none. Stated as a return rather than a panic: the cost of
        // being wrong here is one missed buzzer change, and a poll thread that
        // aborts the process over it would be the worse failure.
        return false;
    };
    // Past this point the device has been touched, whatever follows.
    let Some(current) = device.read_beeper() else {
        crate::evlog::event(
            crate::evlog::Cat::Error,
            "buzzer toggle dropped: current mode could not be read",
        );
        return true;
    };
    let next = current.toggled();
    // The same composition the panel's `Beeper::action_key` offers a button
    // on, so a mode that has no caption also has no command. Reached when the
    // device reports a mode this build does not model: the panel then shows no
    // button, but this thread does not take the panel's word for it.
    let Some(raw) = next.to_raw() else {
        crate::evlog::event(
            crate::evlog::Cat::Error,
            &format!("buzzer toggle dropped: no wire value for {next:?} (from {current:?})"),
        );
        return true;
    };
    match device.set_beeper(raw) {
        Ok(()) => {
            // Read back rather than assumed. The device may clamp or ignore
            // the value, and reporting the requested state as though it took
            // effect would be the monitoring tool lying about the thing it
            // just changed.
            let confirmed = device.read_beeper();
            crate::evlog::event(
                crate::evlog::Cat::Config,
                &match confirmed {
                    Some(state) => {
                        format!("UPS beeper {current:?} -> {next:?}, device reports {state:?}")
                    }
                    None => format!("UPS beeper {current:?} -> {next:?}, readback unavailable"),
                },
            );
            let _ = tx.send(Message::BeeperRead(confirmed));
            repaint();
        }
        Err(e) => {
            // "change failed", not "write failed". `Ups::set_beeper` is a
            // read-modify-write, so the error can come from either phase, and
            // the one seen in the field was `UPS beeper write failed: feature
            // report 12 read failed` — a line that contradicts itself and
            // sends the reader looking for a write fault that did not happen.
            // The operation is named here; which phase of it failed is the
            // error's own to say, and it does.
            crate::evlog::event(
                crate::evlog::Cat::Error,
                &format!("UPS beeper change {current:?} -> {next:?} failed: {e}"),
            );
        }
    }
    true
}

/// Parks until `deadline`, a wake, or a stop — whichever comes first.
///
/// Spurious unparks are absorbed by the loop: with neither flag set and time
/// still on the clock, the thread simply parks again for the remainder.
fn wait_until(deadline: Instant, control: &Control) -> Wait {
    loop {
        if control.stop.load(Ordering::Relaxed) {
            return Wait::Stop;
        }
        if control.wake.swap(false, Ordering::Relaxed) {
            return Wait::Woken;
        }
        let now = Instant::now();
        if now >= deadline {
            return Wait::Deadline;
        }
        std::thread::park_timeout(deadline - now);
    }
}

/// What this thread knows about its session with the device.
///
/// Six values that live across iterations of the poll loop and are read and
/// written by more than one phase of it. Carried separately they were six
/// `&mut` parameters waiting to happen, and the loop could not be broken into
/// its phases without them; named together they are one thing — the state of
/// contact — and each phase becomes a method on it.
struct Session {
    /// The open device, or `None` while there is none.
    ups: Option<Ups>,
    /// Consecutive read failures on an open handle.
    failures: u32,
    /// The reconnect delay, which grows while attempts keep failing.
    backoff: Backoff,
    /// True once a connect failure has been reported, so the identical state
    /// is not re-sent on every backoff retry.
    reported_failure: bool,
    /// The earliest moment the next burst of device traffic may start. Every
    /// step that touched the device schedules the one after it; nothing else
    /// moves this forward, and only the disconnected hot-plug case moves it
    /// back.
    next_contact: Instant,
    /// The moment the most recent connect attempt started, or `None` while no
    /// attempt has been made yet. Only the hot-plug path reads it: a wake may
    /// collapse the reconnect backoff, but not below the shortest step of that
    /// backoff, and this is the anchor that bound is measured from.
    last_attempt: Option<Instant>,
    /// The stop flag, handed to every device that is opened here.
    ///
    /// A connect performs six control transfers and a poll ten, and on a
    /// device that has stopped answering each returns only on the driver's
    /// timeout. Held by the `Ups`, the flag lets those series give up between
    /// transfers instead of running to the end of an exit nobody is watching.
    stop: Arc<AtomicBool>,
}

impl Session {
    fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            ups: None,
            failures: 0,
            backoff: Backoff::default(),
            reported_failure: false,
            next_contact: Instant::now(),
            last_attempt: None,
        }
    }

    /// One connect attempt, and the schedule that follows from its outcome.
    ///
    /// A method rather than a phase inside the loop because every value it
    /// touches is one of this type's own: the handle it may open, the backoff
    /// it resets or advances, the flag that keeps a failing run from repeating
    /// itself in the log, and the two instants it schedules from.
    fn attempt_connect(&mut self, vid: u16, pid: u16, tx: &Sender<Message>, repaint: &impl Fn()) {
        // Stamped before the attempt, not after: a hot-plug wake
        // paces itself off this moment, and pacing from the
        // moment the attempt *finished* would let a slow
        // enumeration shorten the gap it is meant to guarantee.
        self.last_attempt = Some(Instant::now());
        match Ups::connect(vid, pid, self.stop.clone()) {
            Ok((device, ambiguous)) => {
                self.backoff = Backoff::default();
                self.failures = 0;
                self.reported_failure = false;
                let identity = Box::new(device.identity.clone());
                self.ups = Some(device);
                let msg = Message::Connected {
                    identity,
                    ambiguous,
                };
                if tx.send(msg).is_err() {
                    return;
                }
                repaint();
                // The first reading must appear at once: a user who
                // opens the panel wants a value, not an empty skeleton
                // held for a whole poll interval. Connect established
                // the handle and read the identity; the first `poll`
                // follows immediately, and only the polls
                // *after* it take the configured spacing. This is one
                // startup pair of contacts, not sustained fast polling
                // — the `POLL_MIN_MS` floor that protects the firmware
                // governs the steady cadence through `paced_interval`,
                // which every later poll still goes through.
                self.next_contact = Instant::now();
            }
            Err(e) => {
                // Repaint only on the first failure of a run: the
                // retry loop reports the same state every time, and
                // waking the UI on each attempt is pure waste when no
                // device is present at all.
                let first = !self.reported_failure;
                self.reported_failure = true;
                if first {
                    // Logged here, at the point the attempt actually
                    // failed, rather than left to the notification
                    // state machine downstream.
                    //
                    // The state machine cannot report this one. It
                    // only emits `Disconnected` on a connected -> not
                    // connected *transition*, and at startup there is
                    // no such transition — the state begins
                    // disconnected and stays there. So a utility that
                    // never found the device wrote `SESSION started`
                    // and then nothing at all, for hours, which is
                    // precisely the case where the log has to say
                    // something. A successful connect was recorded
                    // and a failed one was not.
                    //
                    // Category is DEVICE rather than ERROR when the
                    // device is simply absent: an unplugged UPS is a
                    // fact about the world, not a malfunction of the
                    // utility. ERROR is reserved for what the utility
                    // could not do — a refused open, a descriptor it
                    // could not parse — which is a different thing to
                    // investigate.
                    if matches!(e, Error::DeviceNotFound { .. }) {
                        crate::evlog::event(
                            crate::evlog::Cat::Device,
                            &format!(
                                "not found: no HID device matching vid={vid:#06x} pid={pid:#06x}"
                            ),
                        );
                        let _ = tx.send(Message::Disconnected);
                    } else {
                        crate::evlog::event(
                            crate::evlog::Cat::Error,
                            &format!("connect failed: {e}"),
                        );
                        let _ = tx.send(Message::Failed(e.connect_failure()));
                    }
                    repaint();
                }
                self.next_contact = Instant::now() + self.backoff.take();
            }
        }
    }
}

fn run(
    vid: u16,
    pid: u16,
    tx: Sender<Message>,
    rx_cmd: Receiver<Command>,
    control: &Control,
    repaint: impl Fn(),
) {
    let mut session = Session::new(control.stop.clone());

    while !control.stop.load(Ordering::Relaxed) {
        // Commands are user actions and are applied as soon as the thread is
        // awake. They are drained *every* cycle, whether or not a device is
        // connected: draining only while connected let commands pile up in the
        // unbounded channel during a disconnect and then fire all at once on
        // reconnect — a queue of self-tests, each moving the UPS onto its
        // inverter for half a minute, run without any fresh confirmation. A
        // command that arrives with no device is discarded with a log line, not
        // deferred.
        //
        // The primary guard against duplicate self-tests is in `App`, which
        // raises `self_test_running` the instant one is requested. The de-dup
        // here is a second, independent line of defence: this thread cannot
        // assume every producer on the channel guards, so it collapses repeated
        // `RunSelfTest` in one drain into a single run rather than trusting the
        // sender. Beeper commands collapse the same way and for the same
        // reason — the panel greys the buzzer button while a test runs, and
        // this thread does not take that on trust. The single self-test runs
        // after the beeper write, which is order-independent of it: the two
        // touch unrelated device state.
        let drained = drain_commands(&rx_cmd);
        let beeper = plan_beeper(drained.beeper_toggles, session.ups.is_some());
        let mut wrote = apply_beeper(beeper, session.ups.as_ref(), &tx, &repaint);

        // Exactly one terminal `SelfTestRunning(false)` per drain that carried
        // a request, however many requests it carried.
        //
        // This is a contract, not a courtesy, and it is stated about the drain
        // rather than about each request because that is what holds: duplicates
        // collapse above, so a drain of three requests answers once. `App`
        // raises `self_test_running` the moment it queues a request — that
        // optimistic raise is what stops a second confirmation queueing a
        // second run — and the flag is lowered by nothing but the terminal
        // `SelfTestRunning(false)` sent from here. A drain that consumed
        // requests and stayed silent latched the panel into "test in progress"
        // for the rest of the session, with the button greyed and no way back.
        // Refusing to run is therefore reported exactly like finishing: an
        // outcome, then the running flag going down.
        let plan = plan_self_test(drained.self_test_requests, session.ups.is_some());
        if plan.dropped > 0 {
            crate::evlog::event(
                crate::evlog::Cat::Device,
                &format!(
                    "{} duplicate self-test command(s) dropped; \
                     one run answers them all",
                    plan.dropped
                ),
            );
        }
        match plan.action {
            SelfTest::Idle => {}
            SelfTest::Run => {
                if let Some(device) = session.ups.as_ref() {
                    wrote = true;
                    // The test moves the UPS onto its inverter for a dozen
                    // seconds and holds this thread for the whole of it. The
                    // panel is told it began and ended, and the outcome is
                    // logged and forwarded. `control.stop` is passed as the
                    // abort flag so a shutdown does not wait out the full test
                    // window.
                    //
                    // Readings keep flowing while it runs, which is the point
                    // of the observer: this loop does not reach its own poll
                    // step until the call returns, so without it the panel
                    // would spend up to half a minute redrawing the reading
                    // taken before the device switched to battery — a stale
                    // number shown as a current one, while the battery is
                    // actually discharging under load. `run_self_test` polls
                    // once a second, the same floor the ordinary path keeps.
                    let _ = tx.send(Message::SelfTestRunning(true));
                    repaint();
                    let outcome = device.run_self_test(&control.stop, |reading| {
                        let _ = tx.send(Message::Update(Box::new(reading)));
                        repaint();
                    });
                    crate::evlog::event(
                        crate::evlog::Cat::Device,
                        &format!("self-test finished: {outcome:?}"),
                    );
                    let _ = tx.send(Message::SelfTestResult(outcome));
                    let _ = tx.send(Message::SelfTestRunning(false));
                    repaint();
                }
            }
            SelfTest::Refuse => {
                // The device went away between the user confirming and this
                // thread waking.
                crate::evlog::event(
                    crate::evlog::Cat::Device,
                    "self-test not run: device disconnected",
                );
                refuse_self_test(&tx);
                repaint();
            }
        }

        if wrote {
            session.next_contact = after_contact(Instant::now());
        }

        if Instant::now() >= session.next_contact {
            match session.ups.as_ref() {
                None => session.attempt_connect(vid, pid, &tx, &repaint),
                // A poll interrupted by the stop flag is not a failed read:
                // the loop is about to exit, and counting it would log a
                // disconnect on the way out of a healthy process.
                Some(device) => match device.poll() {
                    Err(crate::error::Error::Stopped) => return,
                    Ok(reading) => {
                        // A success ends any failure run outright — see
                        // `contact_after_failure` for why this is a reset and
                        // not a decrement.
                        session.failures = 0;
                        if tx.send(Message::Update(Box::new(reading))).is_err() {
                            return;
                        }
                        repaint();
                        session.next_contact = Instant::now() + paced_interval(control);
                    }
                    Err(e) => match contact_after_failure(session.failures) {
                        Contact::Faltering(count) => {
                            session.failures = count;
                            // A failed poll consumed its transfer budget like
                            // a successful one.
                            session.next_contact = Instant::now() + paced_interval(control);
                        }
                        Contact::Lost => {
                            session.ups = None;
                            session.failures = 0;
                            session.backoff = Backoff::default();
                            session.reported_failure = false;
                            // The state machine downstream logs "connection
                            // lost" from the transition, but not why. Losing
                            // a device to failed reads and never finding one
                            // at all are different faults with different
                            // causes — a pulled cable versus a wrong PID —
                            // and the count is what distinguishes them when
                            // reading the file later.
                            //
                            // The last error is named here and nowhere else.
                            // `error.rs` distinguishes `DeviceUnresponsive`
                            // from `FeatureRead { code, cause }` precisely so
                            // the reader can tell "the device stopped
                            // answering" from "one report came back short",
                            // and dropping the value at the match arm threw
                            // that distinction away at the only moment it was
                            // wanted. One line, on the transition, so the
                            // "events only" rule of the log still holds.
                            crate::evlog::event(
                                crate::evlog::Cat::Device,
                                &format!(
                                    "read failed {FAILURES_BEFORE_DISCONNECT} times in a row, treating device as disconnected; last error: {e}"
                                ),
                            );
                            let _ = tx.send(Message::Disconnected);
                            repaint();
                            // The reconnect probe keeps the ordinary pacing
                            // rather than firing immediately: a device that
                            // just failed three reads in a row is the one
                            // most likely to be in the wedged state that more
                            // traffic makes worse. A genuine replug arrives
                            // as WM_DEVICECHANGE and collapses this wait, down
                            // to the same floor every other reconnect probe
                            // observes — which is why the failing read counts
                            // as an attempt and is stamped here.
                            let now = Instant::now();
                            session.last_attempt = Some(now);
                            session.next_contact = now + Backoff::FLOOR;
                        }
                    },
                },
            }
        }

        match wait_until(session.next_contact, control) {
            Wait::Stop => return,
            Wait::Deadline => {}
            Wait::Woken => {
                // A wake means "look up early", not "poll early". Commands
                // are collected at the top of the loop either way; the only
                // deadline a wake may move is the reconnect backoff, because
                // hot-plug is the reason that wake exists and there is no
                // pacing to protect on a device that is not being talked to.
                //
                // Collapsing it all the way to "now" on every wake was
                // unbounded, though: the subscription covers the whole HID
                // class, so any mouse, keyboard or receiver being plugged in
                // — or one device flapping — drove a full interface
                // enumeration plus a `connect` at the rate the events
                // arrived, with the backoff having no effect at all. The
                // floor keeps hot-plug fast (a reaction within a second
                // rather than within ten) while capping probes at one per
                // shortest backoff step.
                if session.ups.is_none() {
                    session.next_contact = after_wake(Instant::now(), session.last_attempt);
                }
            }
        }
    }
}

/// What a drain of commands asks of the self-test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelfTest {
    /// No request in this drain.
    Idle,
    /// Run one test.
    Run,
    /// Answer the request without running: there is no device to run it on.
    Refuse,
}

/// The self-test decision for one drain, and what collapsing cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelfTestPlan {
    action: SelfTest,
    /// Requests beyond the first, dropped rather than queued.
    dropped: usize,
}

/// Decides what one drain of commands means for the self-test.
///
/// Two rules, and they are separate on purpose. **Collapsing** is about the
/// requests: N confirmations in one drain are one run, because the user
/// pressing twice means one test and not two. **Refusing** is about the
/// device: a request that arrives after the UPS went away is answered rather
/// than dropped, because `App` raised its running flag the moment it queued
/// the request and nothing but the terminal `SelfTestRunning(false)` lowers it
/// again.
///
/// Lifted out of `run` so both rules can be checked as values. Inside the loop
/// they were a `bool` set in one place and read in another, several hundred
/// lines apart, and neither was reachable from a test: `run` owns a device
/// handle and a channel and cannot be called without them.
fn plan_self_test(requested: usize, device_present: bool) -> SelfTestPlan {
    let action = match (requested, device_present) {
        (0, _) => SelfTest::Idle,
        (_, true) => SelfTest::Run,
        (_, false) => SelfTest::Refuse,
    };
    SelfTestPlan {
        action,
        dropped: requested.saturating_sub(1),
    }
}

/// What a failed read means for the connection.
///
/// Two outcomes and no third: a read that succeeded never reaches this
/// function, so "still connected" is not a case it has to represent. Adding a
/// `Holding` variant for symmetry would have bought one `unreachable!` at the
/// only call site — a state the type admits and the code swears cannot happen,
/// which is the arrangement this project spends its effort removing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contact {
    /// Not given up on yet. Carries the new consecutive-failure count.
    Faltering(u32),
    /// The failure run reached [`FAILURES_BEFORE_DISCONNECT`].
    Lost,
}

/// Applies one failed read to the consecutive-failure count.
///
/// A single failed read is not a disconnect. USB is allowed the occasional
/// short transfer, and treating one as a lost device would flap the tray icon
/// on a healthy machine; three in a row is a device that has actually stopped
/// answering. The threshold lives in [`FAILURES_BEFORE_DISCONNECT`] and the
/// rule for applying it lives here, where it can be checked without a device.
///
/// The counterpart rule — a successful read resets the count rather than
/// decrementing it — is one assignment in the success arm. Three failures
/// separated by successful reads are three isolated glitches, not a device on
/// its way out.
fn contact_after_failure(failures: u32) -> Contact {
    let failures = failures.saturating_add(1);
    if failures >= FAILURES_BEFORE_DISCONNECT {
        Contact::Lost
    } else {
        Contact::Faltering(failures)
    }
}

/// Answers a self-test request that cannot be carried out.
///
/// The shape of the answer is the whole point, so it lives in one function
/// rather than inline: an outcome, then `SelfTestRunning(false)`. `App` raises
/// its running flag when it queues the request and lowers it on nothing else,
/// so a refusal that reported only the outcome — or nothing at all — would
/// leave the panel claiming a test is in progress for the rest of the session.
///
/// `ChannelError` is the honest outcome for every refusal this thread can
/// issue: the variant means the vendor channel could not be reached, and a
/// device that is no longer there is exactly that.
fn refuse_self_test(tx: &Sender<Message>) {
    let _ = tx.send(Message::SelfTestResult(SelfTestOutcome::ChannelError));
    let _ = tx.send(Message::SelfTestRunning(false));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N confirmations in one drain are one run, and the rest are counted.
    ///
    /// The user pressing the button twice means one test, not two: a self-test
    /// puts the UPS on its inverter for a dozen seconds, and running it again
    /// immediately is the opposite of what the second press asks for. What the
    /// collapse must not do is stay silent about it — the dropped count is what
    /// the log line is built from.
    #[test]
    fn duplicate_self_test_requests_collapse_into_one_run() {
        assert_eq!(
            plan_self_test(1, true),
            SelfTestPlan {
                action: SelfTest::Run,
                dropped: 0
            }
        );
        assert_eq!(
            plan_self_test(3, true),
            SelfTestPlan {
                action: SelfTest::Run,
                dropped: 2
            }
        );
    }

    /// A drain with no request decides nothing, whether or not there is a
    /// device.
    ///
    /// The pairing matters: `Idle` must not depend on the device, or an absent
    /// UPS would have every empty drain answering a request nobody made — and
    /// each answer lowers `App`'s running flag, which is a message about a test
    /// that was never asked for.
    #[test]
    fn a_drain_with_no_request_decides_nothing() {
        assert_eq!(plan_self_test(0, true).action, SelfTest::Idle);
        assert_eq!(plan_self_test(0, false).action, SelfTest::Idle);
        assert_eq!(plan_self_test(0, false).dropped, 0);
    }

    /// A request that outlived its device is refused, not dropped.
    ///
    /// `App` raises `self_test_running` when it queues the request and lowers
    /// it on nothing but the terminal `SelfTestRunning(false)`. A drain that
    /// consumed the request and stayed silent would latch the panel into "test
    /// in progress" for the rest of the session, button greyed, with no way
    /// back. Refusing is reported exactly like finishing.
    #[test]
    fn a_request_without_a_device_is_answered_rather_than_dropped() {
        assert_eq!(plan_self_test(1, false).action, SelfTest::Refuse);
        assert_eq!(
            plan_self_test(4, false),
            SelfTestPlan {
                action: SelfTest::Refuse,
                dropped: 3
            }
        );
    }

    /// One failed read is a glitch; three in a row is a disconnect.
    ///
    /// The whole point of the threshold is that USB is allowed the occasional
    /// short transfer. Giving up on the first would flap the tray icon on a
    /// perfectly healthy machine, which is a worse fault than reacting a couple
    /// of seconds late to a real unplug.
    #[test]
    fn a_device_is_given_up_on_only_after_three_failures_in_a_row() {
        assert_eq!(contact_after_failure(0), Contact::Faltering(1));
        assert_eq!(contact_after_failure(1), Contact::Faltering(2));
        assert_eq!(contact_after_failure(2), Contact::Lost);
    }

    /// The count is pinned to the constant, not to the number three.
    ///
    /// Written this way so raising `FAILURES_BEFORE_DISCONNECT` moves the test
    /// with it rather than breaking it: the property is "the last failure
    /// before the threshold still falters, and the one that reaches it does
    /// not", which is true at any threshold above one.
    #[test]
    fn the_disconnect_threshold_follows_its_constant() {
        let last_tolerated = FAILURES_BEFORE_DISCONNECT - 2;
        assert_eq!(
            contact_after_failure(last_tolerated),
            Contact::Faltering(FAILURES_BEFORE_DISCONNECT - 1)
        );
        assert_eq!(
            contact_after_failure(FAILURES_BEFORE_DISCONNECT - 1),
            Contact::Lost
        );
    }

    /// The poll pace never goes below the firmware floor, whatever is
    /// configured.
    ///
    /// This is the one rule in this file that protects the hardware rather than
    /// the user's patience: some CyberPower units stop answering under
    /// aggressive polling until they are physically unplugged and back in.
    /// `Config` clamps the value on load, so a floor here looks redundant —
    /// which is exactly why it is worth pinning. It is the last check before
    /// the wire, and it is the one that still holds if a value ever reaches
    /// `Control` by another route: the interval is a live atomic that the
    /// settings dialog writes into a running thread, not a number that is only
    /// read at startup.
    #[test]
    fn the_poll_pace_never_goes_below_the_firmware_floor() {
        let control = control_with_interval(1);
        assert_eq!(
            paced_interval(&control),
            Duration::from_millis(u64::from(crate::config::POLL_MIN_MS)),
            "a one-millisecond interval must be raised to the floor"
        );

        // Zero is not a special case in the code, and must not become one: it
        // is the value an uninitialised or badly parsed setting arrives as, and
        // it asks for a poll with no pause at all.
        assert_eq!(
            paced_interval(&control_with_interval(0)),
            Duration::from_millis(u64::from(crate::config::POLL_MIN_MS))
        );

        // Above the floor the configured value is honoured exactly. A floor
        // that also silently rounded or capped would make the setting a
        // suggestion, and the interval the user chose is the one the panel
        // says it is refreshing at.
        let slow = crate::config::POLL_MIN_MS + 4_000;
        assert_eq!(
            paced_interval(&control_with_interval(slow)),
            Duration::from_millis(u64::from(slow))
        );
    }

    /// A wait ends at once on stop, and one wake is consumed by one wait.
    ///
    /// Two properties of the same three lines, and both are about not waiting.
    /// Stop is checked before anything else, so a shutdown does not sit out a
    /// poll interval — up to a minute, during which the process is closing and
    /// the user is looking at a window that will not go away. And the wake flag
    /// is *taken* rather than read: left raised, it would return `Woken`
    /// immediately on every following wait, turning the loop into a spin that
    /// re-probes the device as fast as it can.
    ///
    /// Neither case parks, so this test does not sleep: the deadlines below are
    /// either far away and short-circuited, or already past.
    #[test]
    fn a_wait_ends_at_once_on_stop_and_consumes_one_wake() {
        let control = control_with_interval(crate::config::POLL_MIN_MS);
        let far = Instant::now() + Duration::from_secs(3600);

        control.wake.store(true, Ordering::Relaxed);
        assert!(
            matches!(wait_until(far, &control), Wait::Woken),
            "a raised wake ends the wait without reaching the deadline"
        );
        assert!(
            !control.wake.load(Ordering::Relaxed),
            "the wake must be consumed, or every later wait returns at once"
        );
        assert!(
            matches!(wait_until(Instant::now(), &control), Wait::Deadline),
            "the wake is spent; only the deadline is left"
        );

        // Stop outranks a pending wake: the thread is going away, and looking
        // up early is not something it still needs to do.
        control.stop.store(true, Ordering::Relaxed);
        control.wake.store(true, Ordering::Relaxed);
        assert!(matches!(wait_until(far, &control), Wait::Stop));
    }

    /// Shared control state with `ms` as the configured interval.
    fn control_with_interval(ms: u32) -> Control {
        Control {
            interval_ms: AtomicU32::new(ms),
            stop: Arc::new(AtomicBool::new(false)),
            wake: AtomicBool::new(false),
        }
    }

    /// The backoff grows and then stops growing.
    ///
    /// Saturation is the property worth pinning: wrapping back to one second
    /// would turn a device that has been absent for an hour into a probe every
    /// second, which is the load the backoff exists to prevent.
    #[test]
    fn the_backoff_saturates_rather_than_wrapping() {
        let mut b = Backoff::default();
        let taken: Vec<u64> = (0..6).map(|_| b.take().as_secs()).collect();
        assert_eq!(taken, vec![1, 2, 5, 10, 10, 10]);
    }

    /// A successful connect starts the schedule over.
    ///
    /// The loop does this by assigning a fresh `Backoff`, so what is checked
    /// here is that a fresh one really is the first step and not, say, wherever
    /// the previous one had got to.
    #[test]
    fn a_fresh_backoff_starts_at_the_shortest_step() {
        let mut exhausted = Backoff::default();
        for _ in 0..5 {
            let _ = exhausted.take();
        }
        assert_eq!(Backoff::default().take(), Backoff::FLOOR);
    }

    /// A write spaces the next poll by the floor, measured from the write.
    ///
    /// Not by the configured interval. The interval spaces one poll from the
    /// previous poll; a poll that follows a write answers a different question
    /// — "what did that do?" — and the only thing entitled to delay it is the
    /// pacing floor that protects the firmware.
    ///
    /// This used to take the planned deadline as well and keep it when it was
    /// later, on the argument that a write must not shorten a standing
    /// reconnect backoff. The argument does not apply: a backoff is installed
    /// only by a failed connect, and no write path runs without an open
    /// device. What the `max` actually preserved was the ordinary interval, so
    /// a click on a five-second interval was answered five seconds later.
    #[test]
    fn a_write_spaces_the_next_poll_by_the_floor_from_the_write() {
        let now = Instant::now();
        let minimum = Duration::from_millis(u64::from(crate::config::POLL_MIN_MS));

        assert_eq!(after_contact(now), now + minimum);

        // Independent of how far off the next poll would otherwise have been:
        // the deadline is not an input, which is the property that fixes the
        // long-interval case.
        let later = now + Duration::from_secs(30);
        assert_eq!(after_contact(later), later + minimum);
        assert!(
            after_contact(now) < now + Duration::from_secs(5),
            "a five-second interval must not delay the answer to a click"
        );
    }

    /// A wake shortens the reconnect wait, but only to the floor.
    ///
    /// The subscription covers the whole HID class, so a mouse being plugged in
    /// arrives here too. Collapsing to "now" on each one drove a full interface
    /// enumeration plus a connect at whatever rate the events came, and the
    /// backoff had no effect at all. The floor caps that at one probe per
    /// shortest step while keeping hot-plug a reaction within a second rather
    /// than within ten.
    #[test]
    fn a_wake_collapses_the_backoff_no_further_than_the_floor() {
        let now = Instant::now();

        // An attempt a moment ago: the next one waits out the floor from it.
        let just_tried = now
            .checked_sub(Duration::from_millis(100))
            .expect("the clock is not that young");
        assert_eq!(
            after_wake(now, Some(just_tried)),
            just_tried + Backoff::FLOOR
        );

        // An attempt long ago: nothing to wait for, probe at once.
        let long_ago = now
            .checked_sub(Duration::from_secs(30))
            .expect("the clock is not that young");
        assert_eq!(after_wake(now, Some(long_ago)), now);

        // No attempt yet — the first wake of a session has nothing to pace
        // against, so it probes immediately.
        assert_eq!(after_wake(now, None), now);
    }

    /// A refused self-test still lowers the running flag.
    ///
    /// This is the contract that makes the optimistic raise in `App` safe. The
    /// flag goes up the instant the user confirms — that is what stops a second
    /// confirmation queueing a second run — and comes down only on a terminal
    /// message from this thread. A path that consumed a request and answered
    /// nothing latched the panel into "test in progress" with the button greyed
    /// and no way back short of a restart.
    ///
    /// The order matters too: the outcome is sent before the flag drops, so the
    /// panel never has a frame in which the test is finished but its result is
    /// still the previous run's.
    #[test]
    fn a_refused_self_test_is_still_answered() {
        let (tx, rx) = std::sync::mpsc::channel();
        refuse_self_test(&tx);

        let outcome = rx.try_recv().expect("a refusal must report an outcome");
        assert!(
            matches!(
                outcome,
                Message::SelfTestResult(SelfTestOutcome::ChannelError)
            ),
            "a refusal must name why the test did not run"
        );

        let running = rx
            .try_recv()
            .expect("a refusal must lower the running flag");
        assert!(
            matches!(running, Message::SelfTestRunning(false)),
            "without this the panel stays on 'test in progress' for good"
        );

        assert!(
            rx.try_recv().is_err(),
            "a refusal is exactly two messages: the outcome and the flag"
        );
    }

    /// A drain empties the queue and counts every request it carried.
    ///
    /// The counts gate the pass that follows. The self-test count is what the
    /// collapse line is built from: a count that does not move leaves a
    /// confirmed request unanswered and the panel showing a test in progress
    /// that nothing will ever finish. The toggle count is the same figure for
    /// the same purpose on the other command.
    #[test]
    fn a_drain_counts_requests_and_empties_the_queue() {
        let (tx_cmd, rx_cmd) = std::sync::mpsc::channel();

        assert_eq!(
            drain_commands(&rx_cmd),
            Drained::default(),
            "an empty queue asks for nothing"
        );

        for cmd in [
            Command::RunSelfTest,
            Command::ToggleBeeper,
            Command::RunSelfTest,
        ] {
            tx_cmd.send(cmd).expect("the receiver is still alive");
        }

        assert_eq!(
            drain_commands(&rx_cmd),
            Drained {
                beeper_toggles: 1,
                self_test_requests: 2,
            },
            "every request is counted"
        );
        assert!(
            rx_cmd.try_recv().is_err(),
            "the queue is drained, not sampled"
        );
    }

    /// Toggles annul in pairs, and at most one write survives a drain.
    ///
    /// This is the self-test burst, as a value, and the reason the command is
    /// an intent rather than a target mode. A test holds the poll thread for a
    /// dozen seconds inside one cycle, so clicks made during it arrive
    /// together in the next drain. Six of them mean "change it six times",
    /// which is to say "leave it alone" — and one write is the most the device
    /// ever needs to see for any number of them.
    #[test]
    fn toggles_annul_in_pairs_and_leave_at_most_one_write() {
        assert_eq!(plan_beeper(0, true), Beep::Idle, "nothing was asked for");
        assert_eq!(
            plan_beeper(1, true),
            Beep::Toggle { cancelled: 0 },
            "one click is one change, with nothing annulled"
        );
        assert_eq!(
            plan_beeper(2, true),
            Beep::Cancelled { toggles: 2 },
            "a change and its undo leave the device alone"
        );
        assert_eq!(
            plan_beeper(3, true),
            Beep::Toggle { cancelled: 2 },
            "the odd one out survives; the pair before it is reported"
        );
        assert_eq!(
            plan_beeper(6, true),
            Beep::Cancelled { toggles: 6 },
            "the burst that prompted this work writes nothing at all"
        );
    }

    /// Parity holds at every count, not just the ones written out above.
    ///
    /// The property is what matters — an odd count leaves exactly one change,
    /// an even one leaves none, and no count produces more than one write —
    /// so it is asserted as a property rather than as five examples that a
    /// sixth case could slip past.
    #[test]
    fn every_toggle_count_leaves_one_change_or_none() {
        for toggles in 0..=32usize {
            let plan = plan_beeper(toggles, true);
            let writes = usize::from(matches!(plan, Beep::Toggle { .. }));
            assert_eq!(
                writes,
                toggles % 2,
                "{toggles} toggles must leave {} change(s), not {writes}",
                toggles % 2
            );
            let accounted = match plan {
                Beep::Idle => 0,
                Beep::Toggle { cancelled } => cancelled + 1,
                Beep::Cancelled { toggles } | Beep::NoDevice { toggles } => toggles,
            };
            assert_eq!(
                accounted, toggles,
                "every toggle is either applied or reported as annulled"
            );
        }
    }

    /// A toggle with no device is refused once, however many arrived.
    ///
    /// Refusing per command was one line per click; the burst that prompted
    /// this work would have written six of them.
    #[test]
    fn toggles_to_an_absent_device_are_refused_once() {
        assert_eq!(plan_beeper(6, false), Beep::NoDevice { toggles: 6 });
        assert_eq!(
            plan_beeper(0, false),
            Beep::Idle,
            "an empty drain is not a request, so an absent device is not news"
        );
    }

    /// A plan that writes nothing touches no device and reports no observation.
    ///
    /// The return value is what re-bases the poll schedule, so a pass that
    /// never reached the wire must not claim it did — it would pull the next
    /// poll forward on the strength of a contact that never happened.
    ///
    /// The silence matters as much. An observation is a thing the device said;
    /// a command that was refused before any transfer heard nothing, and
    /// sending `BeeperRead(None)` for it would demote a perfectly current view
    /// to `Stale` because a click arrived while the cable was out.
    #[test]
    fn a_plan_that_writes_nothing_is_not_contact_with_the_device() {
        let (tx, rx) = std::sync::mpsc::channel();
        let repaints = std::cell::Cell::new(0usize);
        let repaint = || repaints.set(repaints.get() + 1);

        for plan in [
            Beep::Idle,
            Beep::NoDevice { toggles: 3 },
            Beep::Cancelled { toggles: 2 },
        ] {
            assert!(
                !apply_beeper(plan, None, &tx, &repaint),
                "{plan:?} never reaches the wire"
            );
        }

        assert!(
            rx.try_recv().is_err(),
            "nothing was observed, so nothing must be reported as an observation"
        );
        assert_eq!(repaints.get(), 0, "nothing changed, so nothing repaints");
    }
}
