//! Application state: owns the config, locale, theme, poll handle and the
//! notification state machine.

use crate::config::Config;
use crate::error::Error;
use crate::hid::{Beeper, Identity, Reading};
use crate::lang::Locale;
use crate::notify::{self, Event, UpsState};
use crate::poller::{Message, PollHandle};
use crate::strings::{self, Language};
use crate::ui::format::format_runtime;
use crate::ui::panel::PanelData;
use crate::ui::settings::SettingsDraft;
use crate::ui::theme::{Builtin, Theme};
use crate::ui::tray::{self, IconCache};
use crate::warning::{ConfigWarning, DeviceWarning, Warnings};
/// What the poll thread has told this session about the device, as one value.
///
/// Two `bool`s said this before — `connected` and `probed` — and one of their
/// four combinations, "open but never heard from", could not occur and had to
/// be argued away instead of being unrepresentable. The distinction they drew
/// is real and is kept: "still connecting" and "failed to connect" look the
/// same to a panel that only knows whether a handle is open, and confusing them
/// made the window open at startup showing the device-not-found view — a small
/// window full of error text — and jump to full size a poll interval later.
///
/// Not [`crate::notify::Link`], which answers a different question. `Link` is
/// `Up` once the device has *answered a poll*; this is `Open` as soon as the
/// handle exists. Between those two moments they disagree, and each is right
/// about its own question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    /// Nothing has come back from the poll thread yet this session.
    Unprobed,
    /// The poll thread has answered, and there is no device to talk to.
    Absent,
    /// The device is open.
    Open,
}

/// What the utility knows about the buzzer mode.
///
/// Three states, and the point of the type is the two it forbids. There is no
/// "current but no mode" and no "stale after never having heard one", so the
/// row that draws from it has three shapes and no argument to make about which
/// combinations can occur. `Link` next door is the same shape for the same
/// reason.
///
/// The state this replaces was a plain `Option<Beeper>` written by the write
/// path alone, and it could lose the mode: one refused readback set it to
/// `None`, the row lost its value *and* its control, and the control was the
/// only thing that would have read the mode again. The mode is polled now, so
/// silence heals in a cycle; and it cannot be dropped on the floor even for
/// that cycle, because [`Self::Stale`] carries it. A failed observation moves
/// between variants and never discards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BeeperView {
    /// Nothing has been heard about the buzzer on this connection. Either the
    /// device has no `AudibleAlarmControl`, or nothing has answered yet.
    #[default]
    Unknown,
    /// The mode, as of the most recent attempt to observe it.
    Current(Beeper),
    /// The mode last observed, with the most recent attempt having failed.
    ///
    /// The value is still shown as absent — a measurement nobody could take is
    /// not a measurement — but the button keeps this mode's caption and greys,
    /// because a control that vanishes for a cycle takes the pointer's target
    /// with it.
    Stale(Beeper),
}

impl BeeperView {
    /// The mode this view carries, current or not.
    pub(crate) fn mode(self) -> Option<Beeper> {
        match self {
            Self::Unknown => None,
            Self::Current(mode) | Self::Stale(mode) => Some(mode),
        }
    }

    /// The view after one attempt to observe the mode.
    ///
    /// `None` is an attempt that came back with nothing — a poll that could
    /// not read the report, or a write whose confirming read was refused. It
    /// demotes rather than erases, which is the whole reason this is a type
    /// and not an `Option`.
    fn observed(self, seen: Option<Beeper>) -> Self {
        match (seen, self.mode()) {
            (Some(mode), _) => Self::Current(mode),
            (None, Some(mode)) => Self::Stale(mode),
            (None, None) => Self::Unknown,
        }
    }
}

/// Everything the program knows, and nothing about how it is shown.
///
/// The model layer in full: the device's last reading, the configuration, the
/// locale and theme, and the handful of session facts that are neither — the
/// outage tally, the standing warnings, whether a self-test is under way. It
/// owns the poll thread's handle and drains its messages, and it produces
/// [`Event`]s for the caller to deliver; it never touches a window, a tray
/// icon or a notification.
///
/// That separation is what makes this type testable at all. `shell` is the
/// only thing above it and the only thing that knows Win32, so every question
/// about *what the program should do* — is the device present, has the mains
/// just failed, should this event be announced — is decided here, in code that
/// runs under Wine with no window and no device. The rule is enforced rather
/// than trusted: the CI step `domain modules stay platform-free` fails the
/// build if `windows::` appears in this file.
///
/// Fields are `pub` to `shell`, which reads them to build rows. Almost all of
/// them are read-only from there: the state moves through the methods below,
/// because a reading assigned from outside would not raise the events that go
/// with it. `draft` is the exception and is assigned directly — it is the
/// settings dialog's working copy, created when the window opens and dropped
/// when it closes, so its lifetime is the window's and the window is `shell`'s.
/// It lives here rather than in `shell` because `apply_settings` is what turns
/// it into a `Config`, and that decision belongs to the model.
pub(crate) struct App {
    pub config: Config,
    pub locale: Locale,
    pub theme: Theme,
    pub languages: &'static [Language],
    pub themes: &'static [Builtin],

    pub reading: Option<Reading>,
    pub identity: Option<Identity>,
    pub presence: Presence,
    /// What is known about the buzzer mode, folded from every observation of
    /// it — the poll's read and the confirming read after a write.
    ///
    /// Both producers report the same kind of fact, so both go through
    /// [`Self::observe_beeper`] and neither writes this field directly. The
    /// readback exists whether or not anything listens to it, so hearing it
    /// costs no transfer and is what makes the row answer a click in
    /// milliseconds instead of waiting out a poll interval.
    pub beeper: BeeperView,
    /// True while a self-test *this utility asked for* is under way, so the
    /// panel can grey the button. Raised optimistically in `run_self_test`
    /// when the request is queued, and cleared only by `SelfTestRunning(false)`
    /// from the poller. The poller's own `SelfTestRunning(true)` message is
    /// redundant with the optimistic raise and simply reaffirms it.
    ///
    /// It does not answer "is a test running" — the device's `Test` register
    /// does, and it sees tests started from the front panel of the UPS as well.
    /// This covers only the window the register cannot: between queueing a
    /// request and the device reporting it. The panel joins the two.
    pub self_test_running: bool,
    pub draft: Option<SettingsDraft>,
    /// Standing complaints, one slot per source. Split by source because a
    /// successful connect answers for the device and a successful save answers
    /// for the file, and neither answers for the other — see `warning.rs`.
    pub warnings: Warnings,
    /// Raised once when the device is missing at startup, so the caller can
    /// surface the panel instead of sitting silently in the tray.
    pub request_show_panel: bool,
    /// Guards the above so the panel is not forced open on every reconnect
    /// attempt, which would make the window impossible to dismiss.
    announced_missing: bool,

    poll: PollHandle,
    state: UpsState,
    /// Power failures observed since this session started.
    ///
    /// Deliberately not shown in the panel: it is a count of what this process
    /// saw, not a device reading, and putting it on the panel would invite it
    /// being read as the UPS's own tally. It goes to the operational log,
    /// where it is a timestamped record of what this process observed.
    outages_this_session: u32,
    /// Events produced by the last pump, drained by the caller for delivery.
    pending: Vec<Event>,
    pub icons: IconCache,
}

/// The theme for a configured code, logging any fallback.
///
/// Themes are compiled in (there is no `themes/` directory any more), so the
/// only failure is a code the INI names that no theme has. The INI is
/// hand-edited, so that is ordinary input rather than corruption; the utility
/// falls back to the default (dark) theme and keeps running. It is logged for
/// the same reason the locale fallback is: default colours with nothing
/// anywhere saying why are otherwise indistinguishable from the setting having
/// been ignored.
fn resolve_theme(code: &str) -> Theme {
    Theme::by_code(code).or_log(|| {
        crate::evlog::event(
            crate::evlog::Cat::Config,
            &format!("unknown theme '{code}', falling back to dark"),
        );
    })
}

/// The locale for a configured language code, logging any fallback.
///
/// The INI is hand-edited, so a code naming a language that does not exist is
/// ordinary input rather than corruption, and the utility keeps running in a
/// language somebody can read. It is logged because the alternative — an
/// English window for a user who asked for something else — is otherwise
/// indistinguishable from the setting having been ignored.
fn resolve_locale(code: &str) -> Locale {
    Locale::by_code(code).or_log(|| {
        crate::evlog::event(
            crate::evlog::Cat::Config,
            &format!("unknown language '{code}', falling back to en"),
        );
    })
}

impl App {
    /// Takes the already-loaded config rather than loading it again. Reading
    /// and re-serialising the INI twice at startup was not just wasted work:
    /// `Config::load` writes the file back when it clamps a value, so a second
    /// load meant a second write before the UI had even appeared.
    pub(crate) fn new(config: Config, config_error: Option<Error>, poll: PollHandle) -> Self {
        let locale = resolve_locale(&config.language);
        let theme = resolve_theme(&config.theme);
        let languages = Locale::available();
        let themes = &Builtin::ALL;
        let icons = IconCache::new(&theme);

        let mut warnings = Warnings::default();
        if let Some(e) = config_error {
            // Logged in English with the path, while the panel shows the
            // localized text. The log is read by whoever is diagnosing the
            // machine, often not its owner and often much later, so it must not
            // change meaning with the interface language.
            warnings.set_config(match e {
                Error::ConfigNotWritable { path, cause } => {
                    crate::evlog::event(
                        crate::evlog::Cat::Error,
                        &format!("config not writable: {} ({cause})", path.display()),
                    );
                    ConfigWarning::NotWritable(path)
                }
                other => {
                    crate::evlog::event(
                        crate::evlog::Cat::Error,
                        &format!("config load failed: {other}"),
                    );
                    ConfigWarning::SaveFailed(other.to_string())
                }
            });
        }

        Self {
            config,
            locale,
            theme,
            languages,
            themes,
            reading: None,
            identity: None,
            presence: Presence::Unprobed,
            beeper: BeeperView::default(),
            self_test_running: false,
            draft: None,
            warnings,
            request_show_panel: false,
            announced_missing: false,
            poll,
            state: UpsState::default(),
            outages_this_session: 0,
            pending: Vec::new(),
            icons,
        }
    }

    /// Drains poll messages and fires notifications on state transitions only.
    ///
    /// Coalescing is deliberate and has two halves that must not be confused.
    /// Every `Update` is passed through `emit_events`, because each one may
    /// carry a genuine transition — a brief dip onto battery and back is two
    /// real edges, and dropping the middle `Update` would lose the event. But
    /// only the *last* reading is retained for display (`self.reading`) and only
    /// the caller repaints, once, after `pump` returns: the panel shows one
    /// coalesced frame however many readings arrived. So transitions are never
    /// coalesced, frames always are.
    ///
    /// The channel is unbounded on purpose — the poll thread owns the device and
    /// must never block on a stalled UI — but that means a long stall could, in
    /// principle, back up an unbounded queue. A per-call drain cap bounds the
    /// work one `pump` does; when it engages, this asks for another device pass
    /// before returning, and that request is what makes "the rest is collected
    /// next pass" true.
    ///
    /// It is not true without it. The poll thread posts one wake per message,
    /// but the loop drains the whole message queue and runs a *single* pass, so
    /// three hundred queued readings and three hundred wakes still amount to
    /// one `tick`. The forty-four left over past the cap would then wait for
    /// the next poll — up to a second with the panel open, and indefinitely
    /// with it closed and the poll thread parked. The cap is far above the
    /// one-per-second steady state, so this only ever engages after a real
    /// stall; it must still terminate when it does.
    pub(crate) fn pump(&mut self) {
        const MAX_PER_PUMP: usize = 256;
        // The log turns itself off from wherever the failing write happened —
        // the poll thread, the HID layer, this one — and none of those places
        // holds anything it could notify, so the condition is asked for here
        // instead. Once per pass is often enough for something that stays true
        // for the rest of the session, and it costs one relaxed atomic load.
        if crate::evlog::disabled() {
            self.warnings.set_log_disabled(crate::evlog::path());
        }
        let mut messages = Vec::new();
        while let Ok(msg) = self.poll.rx.try_recv() {
            messages.push(msg);
            if messages.len() >= MAX_PER_PUMP {
                break;
            }
        }
        // Asked for here rather than after the loop below, so the request is
        // beside the condition that caused it. A posted message cannot be
        // observed before this call returns to the loop, so the position has
        // no other effect.
        if messages.len() >= MAX_PER_PUMP {
            crate::ui::tray_window::wake_device();
        }

        for msg in messages {
            // Any message at all means the poll thread has reported once, so
            // the UI can stop showing the "connecting" skeleton.

            // A device that is gone cannot have a test running. The flag is
            // raised optimistically when the request is queued (see
            // `App::run_self_test`), and the poll thread answers every request
            // it takes — but a request can be outrun by the disconnect itself,
            // so every way of losing the device has to lower it.
            //
            // Decided once, before the arms, rather than assigned inside each
            // of them: the two places that did it were the same rule written
            // twice, and a third way to lose a device would have had to
            // remember it a third time. `device_is_gone` is an exhaustive
            // match, so a new `Message` variant cannot be added without
            // deciding which side of this it falls on.
            if device_is_gone(&msg) {
                self.self_test_running = false;
            }
            match msg {
                Message::Connected {
                    identity,
                    ambiguous,
                } => {
                    // A later disconnect is worth announcing again.
                    self.announced_missing = false;
                    crate::evlog::event(
                        crate::evlog::Cat::Device,
                        &format!(
                            "connected: {} (firmware {}, serial {})",
                            identity.model.as_deref().unwrap_or("unknown"),
                            identity.firmware.as_deref().unwrap_or("unknown"),
                            identity.serial.as_deref().unwrap_or("unknown"),
                        ),
                    );
                    self.identity = Some(*identity);
                    self.presence = Presence::Open;
                    if ambiguous {
                        // Several matching devices: the poller took the first
                        // rather than failing, and the choice is surfaced to
                        // the user without blocking.
                        //
                        // Not logged here. `Ups::connect` already wrote the
                        // line at the moment of the choice, naming the
                        // interface it opened and the ones it passed over —
                        // information this layer does not have. A second line
                        // saying only "several devices" would add a repeat
                        // without adding a fact.
                        self.warnings.set_device(DeviceWarning::Multiple);
                    } else {
                        // A successful connect retires whatever the previous
                        // failure said about the *device*; leaving it up would
                        // tell the user the device is missing while it is
                        // plainly reporting. It says nothing about the
                        // configuration file, which is why that complaint has
                        // its own slot and survives this.
                        self.warnings.clear_device();
                    }
                }
                Message::Update(reading) => {
                    self.presence = Presence::Open;
                    // An unreadable AC flag holds its previous value instead
                    // of collapsing to false. Without this a single failed
                    // feature read looks exactly like a mains failure to the
                    // state machine below, and fires a notification for an
                    // event that never happened — the phantom "switched to
                    // battery" balloons.
                    //
                    // Resolved in `notify` rather than here so the rule is
                    // testable without an App and a poll thread. The resolved
                    // value is not written back into the reading: the panel now
                    // resolves its own display from the raw `Option` (an unread
                    // mains flag shows a dash, not a state), and the state
                    // machine keeps its resolved view in `next`. Writing back
                    // would erase the unknown/clear distinction the panel needs.
                    let next = UpsState::from_reading_with(&self.state, &reading);
                    // Nothing is logged for an ordinary poll. The log records
                    // events, and a reading that changed nothing is not one;
                    // writing every poll is what made the old file grow
                    // without bound.
                    self.emit_events(next, Some(&reading));
                    // A poll is an attempt to observe the mode, and reports as
                    // one whether or not it managed to read the report.
                    self.observe_beeper(reading.beeper);
                    // A test is running, so whatever stopped the last one from
                    // starting no longer stands. Asked of the device rather
                    // than of the utility's own record: a test started from
                    // the front panel answers the complaint just as well.
                    if reading.test_result == Some(crate::hid::TestResult::InProgress) {
                        self.warnings.clear_self_test();
                    }
                    self.reading = Some(*reading);
                }
                // The confirming read after a write, folded through the same
                // entry point as the poll's. It arrives within milliseconds of
                // the click, where the poll behind it arrives within a second,
                // and both say the same kind of thing.
                Message::BeeperRead(seen) => self.observe_beeper(seen),
                Message::SelfTestRunning(running) => {
                    self.self_test_running = running;
                }
                Message::SelfTestResult(outcome) => {
                    // A verdict needs nothing here: the device's `Test`
                    // register carries it, the Last test line reads that
                    // register, and it stays current afterwards instead of
                    // being frozen at whatever this session last ran. Only the
                    // outcomes meaning no test happened have nowhere else to
                    // be seen, and they are a complaint rather than a result.
                    if let Some(reason) = outcome.not_run() {
                        self.warnings.set_self_test(reason);
                    }
                }
                Message::Disconnected => {
                    // Announce the first failure only: repeating it on every
                    // backoff retry would keep re-opening the window.
                    if !self.announced_missing {
                        self.announced_missing = true;
                        self.request_show_panel = true;
                    }
                    self.presence = Presence::Absent;
                    // Not an observation: what a device that has gone last
                    // said about its buzzer is not knowledge about the one
                    // that comes back, so the view is reset rather than
                    // demoted to `Stale`.
                    self.beeper = BeeperView::Unknown;
                    // From the current state, not from nothing: a utility that
                    // started with no device attached has never been in
                    // contact, and this must not move it to "was connected,
                    // now lost". That move was what made the first successful
                    // connection announce itself as a restoration.
                    let next = self.state.contact_lost();
                    self.emit_events(next, None);
                    self.reading = None;
                }
                Message::Failed(failure) => {
                    // Surfaced like the not-found case: a device that is
                    // present but cannot be opened is exactly as invisible
                    // behind a silent grey tray icon, and the panel is what
                    // carries the error text.
                    if !self.announced_missing {
                        self.announced_missing = true;
                        self.request_show_panel = true;
                    }
                    self.presence = Presence::Absent;
                    // Not logged here: the poll thread already wrote the line
                    // at the point the attempt failed, with the vid/pid and the
                    // Win32 detail. This layer shows the user a localised line
                    // for the same failure — the category, translated — while
                    // the log keeps the technical specifics.
                    self.warnings
                        .set_device(DeviceWarning::Unavailable(failure));
                    // Same as the disconnect above: a device that could never
                    // be opened is a link that was never up.
                    let next = self.state.contact_lost();
                    self.emit_events(next, None);
                    self.reading = None;
                }
            }
        }
    }

    /// Re-renders the tray bitmaps if a system metric they depend on moved.
    ///
    /// Driven by the tray window's `metrics_changed` event rather than run on
    /// every pass of the application loop. The unconditional call this
    /// replaces was cheap only because `IconCache::refresh` guards itself, so
    /// the loop paid for a decision it could not see and did not own — and
    /// paid it on passes that had handled no message at all. The theme half of
    /// the same job already lives where the theme changes, in
    /// `apply_settings`.
    pub(crate) fn on_metrics_changed(&mut self) {
        self.icons.refresh(&self.theme);
    }

    /// Queues transitions for delivery, filtered by the per-event settings.
    ///
    /// Every transition is logged, whether or not the user sees a balloon:
    /// that they switched a notification off does not mean the device did not
    /// report the event. The reading that produced it is attached, so the
    /// line is self-contained evidence rather than a bare claim.
    fn emit_events(&mut self, next: UpsState, reading: Option<&Reading>) {
        let events = notify::diff(&self.state, &next);
        self.state = next;
        if events.is_empty() {
            return;
        }

        let metrics = reading.map(crate::evlog::metrics).unwrap_or_default();
        let plan = plan_emission(events, &metrics, self.outages_this_session, &self.config);
        self.outages_this_session = plan.outages;
        for (cat, line) in &plan.log {
            crate::evlog::event(*cat, line);
        }
        self.pending.extend(plan.notify);
    }

    /// Drains queued notifications for the caller to show.
    pub(crate) fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn take_show_panel_request(&mut self) -> bool {
        std::mem::take(&mut self.request_show_panel)
    }

    /// Status, charge, load and runtime, dropped from the end to fit the
    /// tooltip field.
    ///
    /// Whole lines are dropped rather than cut, so the tooltip never ends
    /// mid-word — except in the one case where the first line alone is longer
    /// than the whole field, which no translation produces today. That case
    /// used to leave `out` empty and hand the shell a blank tooltip; now the
    /// line is passed on whole and cut by `set_tooltip`, which owns the limit.
    /// A tooltip cut short still says something, and a blank one says nothing
    /// at all.
    pub(crate) fn tooltip(&self) -> String {
        if self.presence != Presence::Open {
            return self.locale.t(strings::Key::StateDisconnected).to_owned();
        }
        let Some(r) = &self.reading else {
            return self.locale.t(strings::Key::StateDisconnected).to_owned();
        };

        // Unknown mains is not asserted as either state: the tooltip leads with
        // the online/on-battery line only when the flag actually read, matching
        // the panel. When unknown, the charge and load lines below still carry
        // useful information, so the line is simply omitted rather than guessed.
        let mut parts = Vec::new();
        match r.ac_present {
            Some(true) => parts.push(self.locale.t(strings::Key::StateOnline).to_owned()),
            Some(false) => parts.push(self.locale.t(strings::Key::StateOnBattery).to_owned()),
            None => {}
        }

        if let Some(c) = r.charge_percent {
            parts.push(format!(
                "{}: {} {}",
                self.locale.t(strings::Key::PanelCharge),
                c,
                self.locale.t(strings::Key::UnitPercent)
            ));
        }
        if let Some(p) = r.load_percent {
            parts.push(format!(
                "{}: {} {}",
                self.locale.t(strings::Key::PanelLoadPercent),
                p,
                self.locale.t(strings::Key::UnitPercent)
            ));
        }
        if let Some(s) = r.runtime_seconds {
            parts.push(format!(
                "{}: {}",
                self.locale.t(strings::Key::PanelRuntime),
                format_runtime(s, self.locale.t(strings::Key::UnitMinutes))
            ));
        }

        fit_lines(parts, crate::ui::tray_window::TIP_MAX_UTF16)
    }

    pub(crate) fn panel_data(&self) -> PanelData<'_> {
        PanelData {
            reading: self.reading.as_ref(),
            identity: self.identity.as_ref(),
            presence: self.presence,
            beeper: self.beeper,
            warnings: self.warnings.lines(self.locale),
            // Always false here. The skeleton is entered from inside `build`,
            // which sets the flag on its own probe; the live data this
            // produces is by definition data that arrived.
            probing: false,
            self_test_running: self.self_test_running,
        }
    }

    /// Folds one attempt to observe the buzzer mode into what is known.
    ///
    /// The single entry point for both producers — the poll's read and the
    /// confirming read after a write — because they report the same kind of
    /// fact and the field must never be written any other way. `None` is an
    /// attempt that came back with nothing, and it demotes the view rather
    /// than emptying it; see [`BeeperView`] for why that distinction is the
    /// type's whole job.
    fn observe_beeper(&mut self, seen: Option<crate::hid::Beeper>) {
        self.beeper = self.beeper.observed(seen);
    }

    /// Toggles the UPS buzzer. This is the only write the utility performs,
    /// and only on an explicit click.
    ///
    /// Nothing is decided here and nothing is logged. The target mode is not
    /// computed from [`Self::beeper`] — that is what the panel draws, and it
    /// is as old as the last observation: the mode cannot
    /// move while commands are queued behind a self-test, and it moves without
    /// any poll at all when the buzzer is changed from the front panel of the
    /// UPS. Six clicks during one self-test each read the same stale value and
    /// produced six commands naming the same target, which reached the device
    /// as six identical writes.
    ///
    /// So the click is forwarded as the intent it is. The poll thread owns the
    /// handle, reads the current mode at the moment it acts, and writes its
    /// opposite; the request is not the event, so the outcome — including a
    /// refusal by the device — is logged there.
    pub(crate) fn toggle_beeper(&self) {
        self.poll.toggle_beeper();
    }

    /// Requests a self-test. Queued to the poll thread that owns the handle;
    /// the outcome and running state come back as messages.
    ///
    /// The `self_test_running` flag is raised **here**, optimistically, the
    /// moment the request is queued — not when the poller's `SelfTestRunning`
    /// message arrives. Raising it on the returning message left a window
    /// between the user's confirmation and that message in which a second
    /// confirmation queued a second run. This is the single guard that makes a
    /// self-test one-at-a-time; the poller keeps a second, independent check
    /// only as defence in depth, since it cannot assume every caller guards.
    /// The flag stays raised for the whole run regardless of how it began, and
    /// comes down on exactly two events: the poll thread's terminal
    /// `SelfTestRunning(false)` — which that thread sends whether it ran the
    /// test or refused it — or contact with the device being lost, since a test
    /// cannot be in progress on a device that is gone. Between them every path
    /// out of "running" is covered, which is what an optimistic raise requires:
    /// a flag raised on intent must be lowered by something that cannot fail to
    /// happen.
    ///
    /// The raise is conditional on the request actually being queued. If the
    /// poll thread is not running there is nothing to answer it, and raising
    /// the flag would grey the button for the rest of the session.
    pub(crate) fn run_self_test(&mut self) {
        if self.self_test_running {
            return;
        }
        if self.poll.run_self_test() {
            self.self_test_running = true;
        }
    }

    /// Commits settings: reloads locale and theme in place and pushes the new
    /// interval to the poller without recreating the HID connection.
    pub(crate) fn apply_settings(&mut self) {
        // Applied before the save is logged, so the CONFIG line below is
        // itself written under the level the user just chose. Switching to
        // Debug and seeing nothing until the next event would read as the
        // setting having failed to take.
        crate::evlog::set_debug(self.config.log_level == crate::config::LogLevel::Debug);

        // Logged after the save, not before it. Written first, the line
        // claimed the settings had been applied even when the write that
        // followed failed — leaving a file that asserted a change it also
        // recorded as impossible two lines later.
        let saved = self.config.save();
        match &saved {
            Err(e) => {
                // English, with the path, regardless of interface language:
                // the localized text goes to the panel, the log stays
                // readable by whoever is diagnosing the machine later.
                match e {
                    Error::ConfigNotWritable { path, cause } => {
                        crate::evlog::event(
                            crate::evlog::Cat::Error,
                            &format!(
                                "settings not saved, config not writable: {} ({cause})",
                                path.display()
                            ),
                        );
                    }
                    other => {
                        crate::evlog::event(
                            crate::evlog::Cat::Error,
                            &format!("settings not saved: {other}"),
                        );
                    }
                }
            }
            Ok(()) => {
                crate::evlog::event(
                    crate::evlog::Cat::Config,
                    &format!(
                        "settings applied: poll {}ms, language {}, theme {}, \
                         notifications {}, log {}",
                        self.config.poll_interval_ms,
                        self.config.language,
                        self.config.theme,
                        if self.config.notifications_enabled {
                            "on"
                        } else {
                            "off"
                        },
                        // Recorded because it changes what the rest of the
                        // file will contain. A reader finding a quiet stretch
                        // needs to know whether nothing happened or nothing
                        // was being written down.
                        self.config.log_level.as_str(),
                    ),
                );
            }
        }
        match saved {
            Err(e) => self.warnings.set_config(match e {
                Error::ConfigNotWritable { path, .. } => ConfigWarning::NotWritable(path),
                other => ConfigWarning::SaveFailed(other.to_string()),
            }),
            // A successful save clears the previous complaint about the file.
            // Without this a one-off failure would stay on screen for the rest
            // of the run, long after the user had fixed it. It leaves the
            // device slot alone: pressing OK in Settings is not evidence about
            // how many UPSes are attached.
            Ok(()) => self.warnings.clear_config(),
        }

        self.locale = resolve_locale(&self.config.language);
        self.theme = resolve_theme(&self.config.theme);
        self.icons.refresh(&self.theme);
        self.poll.set_interval(self.config.poll_interval_ms);
    }

    /// Called from the `WM_DEVICECHANGE` hook to skip the reconnect backoff.
    pub(crate) fn on_device_change(&self) {
        self.poll.wake();
    }

    pub(crate) fn icon_state(&self) -> crate::ui::tray::IconState {
        tray::state_for(self.reading.as_ref(), self.presence)
    }

    pub(crate) fn shutdown(&mut self) {
        // Joined, not merely signalled: `main` returns right after this, and
        // process exit kills threads wherever they stand. Without the join a
        // beeper write queued a moment before "Exit" could be cut off
        // mid-transfer, with the intent already logged and no outcome ever
        // recorded. The thread checks the stop flag at every park and between
        // steps, so the wait is bounded by one transaction burst.
        self.poll.stop_and_join();
        // A failure here is not cosmetic: this write is what persists the
        // panel position and any setting changed since the last save. Losing
        // it silently means the user moves the window, exits, and finds it
        // back in the centre with nothing anywhere saying why.
        if let Err(e) = self.config.save() {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                &format!("settings not saved on exit: {e}"),
            );
        }
    }
}

/// What a batch of transitions comes to, before anything is written or shown.
struct Emission {
    /// Category and text of every line to write, in order.
    log: Vec<(crate::evlog::Cat, String)>,
    /// The events that passed the notification switches.
    notify: Vec<Event>,
    /// The session outage counter after this batch.
    outages: u32,
}

/// Whether `msg` means the device is no longer there.
///
/// Written as a function so the rule can be checked as a value. It used to be
/// two assignments in two `match` arms, guarded by a test that searched
/// `pump`'s source text for them — and that test located the end of an arm by
/// looking for sixteen spaces before the next `Message::`. Reindent `pump` by
/// one level and the search misses, the fallback takes the whole rest of the
/// body, and the test goes on passing while checking nothing. A test that stops
/// checking without failing is the worst kind there is.
///
/// Exhaustive rather than `matches!`: adding a `Message` variant should not
/// silently default it to "the device is still here". The compiler asks.
fn device_is_gone(msg: &Message) -> bool {
    match msg {
        Message::Disconnected | Message::Failed(_) => true,
        Message::Connected { .. }
        | Message::Update(_)
        | Message::BeeperRead(_)
        | Message::SelfTestRunning(_)
        | Message::SelfTestResult(_) => false,
    }
}

/// Decides what a batch of transitions produces, without producing any of it.
///
/// Split out of [`App::emit_events`] so the rule it embodies can be *tested*
/// rather than read. The rule is that every event reaches the log whatever the
/// notification switches say — it is why the log stays a complete record of
/// what the device did even for a user who has muted every balloon — and it
/// used to be checked by a test that read this file, located the string
/// `"crate::evlog::event("` and the string `"notifications_enabled"` in the
/// function's source, and asserted that the first came earlier. That test
/// passed on a function that logged nothing (both `find`s would have panicked,
/// which is at least loud) and, worse, would have passed on one that logged
/// the wrong events in the right order. It measured the shape of the source,
/// not the behaviour.
///
/// `App` could not be built in a unit test — it owns a poll thread and a tray
/// icon — but that was never a reason the *decision* had to live inside it.
/// Here it takes what it needs and returns what it decided, and the property
/// is one assertion: with every switch off, `log` is still one line per event.
///
/// The outage counter is threaded through rather than mutated in place for the
/// same reason: a counter that only exists on `App` is a counter no test can
/// watch increment.
fn plan_emission(
    events: Vec<Event>,
    metrics: &str,
    outages: u32,
    config: &crate::config::Config,
) -> Emission {
    let mut outages = outages;
    let mut log = Vec::with_capacity(events.len());

    for ev in &events {
        // Counted before the line is built, so the number in the text is this
        // outage rather than the previous one.
        if ev.kind == notify::EventKind::PowerFailure {
            outages += 1;
        }
        let mut line = ev.kind.info().log_text.to_owned();
        if ev.kind == notify::EventKind::PowerFailure {
            let _ = std::fmt::Write::write_fmt(
                &mut line,
                format_args!(" (outage #{outages} this session)"),
            );
        }
        if !metrics.is_empty() {
            let _ = std::fmt::Write::write_fmt(&mut line, format_args!(" — {metrics}"));
        }
        let cat = match ev.kind {
            notify::EventKind::Disconnected | notify::EventKind::Reconnected => {
                crate::evlog::Cat::Device
            }
            _ => crate::evlog::Cat::Power,
        };
        log.push((cat, line));
    }

    // The gate applies to `notify` and to nothing else. `log` above is already
    // complete: it was built from every event, before this line was reached
    // and without consulting a switch.
    let notify = if config.notifications_enabled {
        events
            .into_iter()
            .filter(|e| config.notifies_for(e.kind))
            .collect()
    } else {
        Vec::new()
    };

    Emission {
        log,
        notify,
        outages,
    }
}

/// Joins `parts` with newlines, dropping whole lines from the end until the
/// result fits `max_units` UTF-16 code units.
///
/// Whole lines rather than a cut, so the tooltip never ends mid-word. The one
/// exception is a first line longer than the whole field: it is returned as it
/// is, for the boundary that owns the limit to cut, because a tooltip cut short
/// still says something and an empty one says nothing at all. That case used to
/// return the empty string, which is how the tray could end up with no tooltip
/// at all rather than a shortened one.
///
/// A free function, and not a method, because the rule is about text: it needs
/// nothing from the application state, and taking it out of `tooltip` is what
/// lets it be tested without a device, a poll thread and a window.
fn fit_lines(parts: Vec<String>, max_units: usize) -> String {
    let mut out = String::new();
    for part in parts {
        let candidate = if out.is_empty() {
            part
        } else {
            format!("{out}\n{part}")
        };
        if candidate.encode_utf16().count() > max_units {
            if out.is_empty() {
                out = candidate;
            }
            break;
        }
        out = candidate;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::Presence;
    use super::{device_is_gone, fit_lines, plan_emission, App, BeeperView};
    use crate::config::Config;
    use crate::error::ConnectFailure;
    use crate::notify::{Event, EventKind};
    use crate::poller::Message;
    use crate::ui::tray::IconState;

    /// Repeated failures to reach the device announce themselves once.
    ///
    /// This is the first check that runs a whole chain rather than one of its
    /// links: the messages a poll thread would send go in, and what comes out
    /// is what the shell would act on — the notification queue, the tray icon
    /// and the "connected" flag. The links were each covered before; the joins
    /// between them were not, and a state machine is mostly joins.
    ///
    /// One announcement, not three, is the property. A UPS unplugged for the
    /// evening fails every poll for hours, and a balloon per poll would be a
    /// notification every second until the user gives up on the utility.
    ///
    /// The link has to be established first, and that is not ceremony: a
    /// utility started with no UPS attached has lost nothing, so its failures
    /// announce nothing at all. That is the neighbouring property, and the
    /// first version of this test asserted the opposite until it ran.
    #[test]
    fn a_device_that_stays_unreachable_is_announced_once() {
        let (tx, mut app) = app_with_poll();
        connect(&tx, &mut app);

        for _ in 0..3 {
            tx.send(Message::Failed(ConnectFailure::Unresponsive))
                .expect("the detached channel stays open");
        }
        app.pump();

        assert_eq!(
            disconnects(&mut app),
            1,
            "three failed polls are one condition, not three events"
        );
        assert_ne!(
            app.presence,
            Presence::Open,
            "a failed connect is not a connection"
        );
        assert_eq!(
            app.icon_state(),
            IconState::Disconnected,
            "the tray icon must show the device is gone"
        );

        // A fourth failure in a later pass adds nothing: the condition has not
        // changed, and neither has what the user has already been told.
        tx.send(Message::Failed(ConnectFailure::Unresponsive))
            .expect("the detached channel stays open");
        app.pump();
        assert_eq!(
            disconnects(&mut app),
            0,
            "a standing condition does not re-announce itself"
        );
    }

    /// A device that was never reached announces nothing when it fails.
    ///
    /// Losing a link that never existed is not losing anything, and a utility
    /// launched on a machine whose UPS is unplugged would otherwise open with
    /// a balloon about a device the user has not connected yet.
    #[test]
    fn a_device_that_was_never_there_is_not_announced_as_lost() {
        let (tx, mut app) = app_with_poll();

        tx.send(Message::Failed(ConnectFailure::Unresponsive))
            .expect("open");
        app.pump();

        assert_eq!(disconnects(&mut app), 0, "nothing was lost");
        assert_ne!(
            app.presence,
            Presence::Unprobed,
            "any message at all ends the `connecting` skeleton"
        );
        assert_eq!(
            app.icon_state(),
            IconState::Disconnected,
            "the icon still says there is no device, announcement or not"
        );
    }

    /// A reconnection re-arms the announcement, so the next loss is reported.
    ///
    /// The other half of the property above, and the one a naive "announce
    /// once ever" would fail: silence after the first disconnect is right only
    /// while the device is still gone.
    #[test]
    fn a_reconnection_re_arms_the_disconnect_announcement() {
        let (tx, mut app) = app_with_poll();
        connect(&tx, &mut app);

        tx.send(Message::Disconnected).expect("open");
        app.pump();
        assert_eq!(disconnects(&mut app), 1, "a live link was lost");

        connect(&tx, &mut app);
        tx.send(Message::Disconnected).expect("open");
        app.pump();
        assert_eq!(
            disconnects(&mut app),
            1,
            "a device lost again is news again"
        );
    }

    /// Brings the link up: a device is found, and it answers a poll.
    ///
    /// Both halves are needed. `Connected` says a handle was opened; it is the
    /// first *reading* that makes the link `Up`, because that is the first
    /// evidence the device is answering. Draining the events afterwards leaves
    /// the queue empty for the assertion that follows.
    fn connect(tx: &std::sync::mpsc::Sender<Message>, app: &mut App) {
        tx.send(Message::Connected {
            identity: Box::default(),
            ambiguous: false,
        })
        .expect("open");
        tx.send(Message::Update(Box::default())).expect("open");
        app.pump();
        assert_eq!(
            app.presence,
            Presence::Open,
            "the fixture must leave a live link"
        );
        let _ = app.take_events();
    }

    /// How many disconnect announcements are queued, draining the queue.
    fn disconnects(app: &mut App) -> usize {
        app.take_events()
            .iter()
            .filter(|e| e.kind == EventKind::Disconnected)
            .count()
    }

    /// An `App` with a poll handle that has no thread behind it, and the sender
    /// that stands in for the device.
    fn app_with_poll() -> (std::sync::mpsc::Sender<Message>, App) {
        let (tx, poll) = crate::poller::detached();
        (tx, App::new(Config::default(), None, poll))
    }

    /// Lines are dropped whole, and the tooltip is never empty when there is
    /// anything at all to say.
    ///
    /// The empty case is the one that shipped: the loop kept only candidates
    /// that fitted, so a first line longer than the field left nothing, and the
    /// shell got a blank tooltip where a shortened one would have done.
    #[test]
    fn the_tooltip_drops_whole_lines_but_never_everything() {
        let parts = |n: usize| (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>();

        assert_eq!(fit_lines(parts(3), 100), "line 0\nline 1\nline 2");
        assert_eq!(
            fit_lines(parts(3), 14),
            "line 0\nline 1",
            "the third line does not fit and is dropped whole"
        );
        assert_eq!(fit_lines(Vec::new(), 100), "");

        let long = "x".repeat(200);
        assert_eq!(
            fit_lines(vec![long.clone()], 127),
            long,
            "a first line past the field is handed on to be cut, not dropped"
        );

        // A line that fills the field exactly fits in it. Dropped at the
        // boundary, the tooltip loses its last line on precisely the readings
        // that make it longest — a device on battery, which is when it is
        // read.
        assert_eq!(
            fit_lines(vec!["ab".to_owned(), "cd".to_owned()], 5),
            "ab\ncd",
            "five units is room for five units"
        );
    }

    /// One event of each kind, for the emission tests.
    fn one_of_each_kind() -> Vec<Event> {
        crate::notify::EventKind::ALL
            .into_iter()
            .map(|kind| {
                let info = kind.info();
                Event {
                    title_key: info.title_key,
                    body_key: info.body_key,
                    severity: info.severity,
                    kind,
                }
            })
            .collect()
    }

    /// Every event reaches the log whatever the switches say.
    ///
    /// This is what lets a user mute every balloon and still have a complete
    /// record of what the device did: declining a popup does not unmake the
    /// event. The property used to be checked by reading this file — locating
    /// `"crate::evlog::event("` and `"notifications_enabled"` in the source of
    /// `emit_events` and asserting the first appeared earlier — which measured
    /// the shape of the text rather than the behaviour, and would have passed
    /// just as happily on a function that logged the wrong events in the right
    /// order. `plan_emission` returns the decision instead of performing it,
    /// so the property is now one assertion about a value.
    #[test]
    fn every_event_is_logged_whatever_the_switches_say() {
        let events = one_of_each_kind();
        let mut all_off = crate::config::Config::default();
        all_off.notifications_enabled = false;
        all_off.on_power_failure = false;
        all_off.on_power_restored = false;
        all_off.on_low_battery = false;
        all_off.on_device_fault = false;
        all_off.on_overload = false;
        all_off.on_voltage_out_of_range = false;
        all_off.on_frequency_out_of_range = false;
        all_off.on_runtime_limit = false;
        all_off.on_connection_lost = false;
        all_off.on_connection_restored = false;
        all_off.on_boost_started = false;
        all_off.on_boost_ended = false;

        let plan = plan_emission(events.clone(), "", 0, &all_off);
        assert_eq!(
            plan.log.len(),
            events.len(),
            "muting the balloons must not remove a line from the log"
        );
        assert!(
            plan.notify.is_empty(),
            "every switch is off, so nothing may be queued for display"
        );
        for (_, line) in &plan.log {
            assert!(!line.is_empty(), "an event was logged as an empty line");
        }
    }

    /// The master switch alone silences the balloons and nothing else.
    ///
    /// Stated separately from the per-event switches because it is a separate
    /// control and a separate way to get this wrong: an early return placed
    /// one statement too high would skip the logging for every event at once.
    #[test]
    fn the_master_switch_silences_balloons_but_not_the_log() {
        let events = one_of_each_kind();
        let mut muted = crate::config::Config::default();
        muted.notifications_enabled = false;

        let plan = plan_emission(events.clone(), "", 0, &muted);
        assert_eq!(plan.log.len(), events.len());
        assert!(plan.notify.is_empty());

        // And with it on, the same batch is queued in full — otherwise the
        // assertion above would hold for a function that queues nothing ever.
        let plan = plan_emission(events.clone(), "", 0, &crate::config::Config::default());
        assert_eq!(plan.notify.len(), events.len());
    }

    /// The outage counter advances once per mains failure and appears in the
    /// line it numbers.
    ///
    /// It lived on `App` as a private field, so nothing could watch it move.
    /// Threading it through `plan_emission` is what makes "the number in the
    /// text is *this* outage, not the previous one" a checkable statement.
    #[test]
    fn outages_are_counted_and_named_in_the_line() {
        let failure = crate::notify::EventKind::PowerFailure;
        let info = failure.info();
        let batch = vec![Event {
            title_key: info.title_key,
            body_key: info.body_key,
            severity: info.severity,
            kind: failure,
        }];
        let config = crate::config::Config::default();

        let first = plan_emission(batch.clone(), "", 0, &config);
        assert_eq!(first.outages, 1);
        assert!(
            first.log[0].1.contains("outage #1"),
            "got {:?}",
            first.log[0].1
        );

        let second = plan_emission(batch, "", first.outages, &config);
        assert_eq!(second.outages, 2);
        assert!(
            second.log[0].1.contains("outage #2"),
            "got {:?}",
            second.log[0].1
        );
    }

    /// A log line carries the metric snapshot when there is one, and carries
    /// its own category.
    ///
    /// The snapshot is what makes a line evidence rather than an assertion —
    /// "switched to battery" next to the charge and load at that instant — and
    /// the separator must not appear when there is nothing after it, which is
    /// every line written before the first successful poll. The category is
    /// what `findstr DEVICE` and `findstr POWER` sort the file by: a
    /// connection event filed under POWER puts a cable being pulled in among
    /// the mains events, which is the column a reader scans to find out what
    /// the *supply* did.
    #[test]
    fn a_log_line_carries_its_metrics_and_its_own_category() {
        let events = one_of_each_kind();
        let config = Config::default();

        let bare = plan_emission(events.clone(), "", 0, &config);
        for (_, line) in &bare.log {
            assert!(
                !line.contains(" — "),
                "no snapshot means no separator: {line}"
            );
        }

        let measured = plan_emission(events.clone(), "charge 80%", 0, &config);
        for (_, line) in &measured.log {
            assert!(
                line.ends_with(" — charge 80%"),
                "the snapshot belongs on every line of the batch: {line}"
            );
        }

        // Exactly the two connection events are device lines; everything else
        // is about the supply.
        let device_events: Vec<EventKind> = events
            .iter()
            .zip(&measured.log)
            .filter(|(_, (cat, _))| *cat == crate::evlog::Cat::Device)
            .map(|(ev, _)| ev.kind)
            .collect();
        assert!(device_events.contains(&EventKind::Disconnected));
        assert!(device_events.contains(&EventKind::Reconnected));
        assert_eq!(
            device_events.len(),
            2,
            "only losing and regaining the device is a device event: {device_events:?}"
        );
    }

    /// Losing the device lowers the self-test running flag.
    ///
    /// The flag is raised optimistically when a test is requested, so every way
    /// out of "running" has to lower it or the panel is stuck showing a test in
    /// progress with its button greyed. The poll thread answers every request
    /// it takes off the channel, run or refused
    /// (`poller::tests::a_refused_self_test_is_still_answered`); this covers
    /// the other direction — the device going away independently of any
    /// request — so the two together leave no path that latches the flag.
    ///
    /// A failed observation of the buzzer demotes the view and keeps the mode.
    ///
    /// This is the defect that produced the whole change, reduced to the one
    /// transition it turns on. The mode used to live in an `Option` written
    /// only by the write path: one refused readback emptied it, the row lost
    /// its value *and* its control, and the control was the only thing that
    /// would have read the mode again. Nothing could get out of that state
    /// short of restarting the utility.
    ///
    /// Two properties are asserted, and the second is the one that used to
    /// fail. The view stops calling the mode current — a value nobody could
    /// read is not a measurement — and it still carries it, so the button has
    /// a caption to keep wearing while it greys.
    ///
    /// The recovery is asserted from `Stale` and not only from `Current`,
    /// because a fold that healed only the first failure would pass a shorter
    /// test and still strand a device that missed two reads in a row.
    #[test]
    fn a_failed_observation_of_the_buzzer_demotes_without_forgetting() {
        use crate::hid::Beeper;

        let fresh = BeeperView::Unknown.observed(Some(Beeper::Enabled));
        assert_eq!(fresh, BeeperView::Current(Beeper::Enabled));

        let missed = fresh.observed(None);
        assert_eq!(
            missed,
            BeeperView::Stale(Beeper::Enabled),
            "an unread mode must stop being current without being forgotten"
        );
        assert_eq!(
            missed.mode(),
            Some(Beeper::Enabled),
            "the button draws its caption from this"
        );

        assert_eq!(
            missed.observed(None),
            missed,
            "a second failure changes nothing; there is no state below stale"
        );
        assert_eq!(
            missed.observed(Some(Beeper::Disabled)),
            BeeperView::Current(Beeper::Disabled),
            "any successful observation restores currency, from any depth"
        );

        assert_eq!(
            BeeperView::Unknown.observed(None),
            BeeperView::Unknown,
            "a device that has never answered has no mode to hold"
        );
    }

    /// A test of the decision itself, not of `pump`'s source text. What it
    /// replaced searched that text and located the end of a `match` arm by
    /// sixteen spaces of indentation — see [`device_is_gone`] for why that is
    /// the worst way for a test to fail.
    #[test]
    fn losing_the_device_clears_the_self_test_flag() {
        assert!(device_is_gone(&Message::Disconnected));
        assert!(device_is_gone(&Message::Failed(ConnectFailure::Busy)));

        assert!(!device_is_gone(&Message::SelfTestRunning(true)));
        assert!(!device_is_gone(&Message::SelfTestResult(
            crate::hid::SelfTestOutcome::Passed
        )));
        assert!(!device_is_gone(&Message::Update(Box::default())));
    }

    /// Every event kind is gated by its own configuration field.
    ///
    /// The four-switches-for-twelve-events state this replaced is easy to
    /// drift back into: the shortest way to add an event is to reuse a
    /// neighbouring switch, and nothing fails when that happens. Counting
    /// distinct `on_*` fields in `Config::notifies_for` makes the reduction
    /// visible.
    #[test]
    fn notifies_for_reads_a_switch_for_every_kind() {
        // Behavioural, not textual: every switch on must allow every kind, and
        // every switch off must suppress every kind. This catches a kind
        // hardcoded to a constant (the bug AVR once was, always off) and a
        // kind wired to the wrong field, without parsing source.
        use crate::notify::EventKind;

        let all_on = crate::config::Config::default(); // every switch defaults on
        let mut all_off = crate::config::Config::default();
        all_off.on_power_failure = false;
        all_off.on_power_restored = false;
        all_off.on_low_battery = false;
        all_off.on_device_fault = false;
        all_off.on_overload = false;
        all_off.on_voltage_out_of_range = false;
        all_off.on_frequency_out_of_range = false;
        all_off.on_runtime_limit = false;
        all_off.on_connection_lost = false;
        all_off.on_connection_restored = false;
        all_off.on_boost_started = false;
        all_off.on_boost_ended = false;

        for kind in EventKind::ALL {
            assert!(
                all_on.notifies_for(kind),
                "{kind:?} is off with every switch on: it is wired to the wrong field or hardcoded off"
            );
            assert!(
                !all_off.notifies_for(kind),
                "{kind:?} still fires with every switch off: it ignores its switch"
            );
        }
    }
}
