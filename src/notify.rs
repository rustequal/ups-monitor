//! Notification state machine.
//!
//! Delivery lives in `tray_window`: balloons go through the icon this process
//! owns. This module only decides *what* to report, by comparing the previous
//! and current state. Events fire on transitions only, never on every poll.

use crate::hid::Reading;
use crate::ui::tray_window::Severity;
/// Connection and power state, tracked so notifications fire only on edges.
///
/// Every field here is one the utility will report a transition for. A flag
/// that is only ever displayed does not belong in this struct — and,
/// conversely, a flag added to the panel without being added here is one the
/// device can raise and lower while the log stays silent. That is exactly
/// what happened to the four fault flags added alongside the transfer
/// thresholds: they reached the panel and nothing else.
///
/// Each flag is `Option<bool>`, because "not measured" is a third state and
/// has to be one the type can hold. It used to be inferred from a neighbouring
/// field: the flags were plain `bool` and a separate `initialised` flag was
/// consulted to decide whether the `false` in front of you was a measurement or
/// a placeholder. That made one field answer two different questions — "has
/// there been any poll this session" and "is there a previous measurement of
/// *this* flag" — and the two answers diverge exactly when the device
/// reconnects. The disconnected state had to claim it was initialised to keep
/// `diff` emitting `Reconnected`, which simultaneously claimed every
/// placeholder `false` was a real measurement; the first successful poll after
/// a replug then compared a genuine reading against them and announced "mains
/// restored" for a UPS that had never left the mains.
///
/// With the unknown state written down, [`UpsState::contact_lost`] simply says
/// `None` everywhere, the question about the link is answered once by [`Link`],
/// and an edge is what it claims to be: a change between two measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct UpsState {
    pub link: Link,
    pub ac_present: Option<bool>,
    pub low_battery: Option<bool>,
    pub internal_failure: Option<bool>,
    pub overload: Option<bool>,
    /// Mains voltage outside the transfer window.
    pub voltage_out_of_range: Option<bool>,
    /// Mains frequency outside tolerance.
    pub frequency_out_of_range: Option<bool>,
    /// AVR correcting mains by transformer tap. Reported, but not as a fault.
    pub boost: Option<bool>,
    /// The remaining-runtime threshold has been crossed.
    pub runtime_limit_expired: Option<bool>,
}

/// Whether the utility has ever been in contact with the device, and whether it
/// is now.
///
/// Three states, not a `bool` beside an `initialised` flag. Those were two
/// fields answering one question with an inconsistency the type allowed: a
/// start with no UPS attached wrote `connected: false, initialised: true`,
/// which reads as "contact was had and then lost". It never had been. When the
/// UPS was finally plugged in, `diff` saw `false -> true` and announced that
/// the link had been *restored* — a balloon about a connection the user had
/// never made, the same class of defect as the phantom "mains restored": an
/// event reported for a transition nobody observed.
///
/// `initialised` is not kept alongside this. It was exactly `link != Never`,
/// which is a second copy of one fact and therefore a pair that can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Link {
    /// Nothing has answered yet this session. The starting state, and also
    /// where a utility started without a device stays until one appears.
    #[default]
    Never,
    /// Contact was established and has since been lost.
    Lost,
    /// The device is answering.
    Up,
}

/// Carries an unreadable flag forward instead of reading it as clear.
///
/// A failed feature read arrives as `None`, which must not be treated as the
/// device reporting the flag clear: those two were once both `false`, and
/// `diff` then fired a transition for an event that never happened — the
/// phantom "switched to battery" balloons, and the same latent bug for every
/// other fault flag. An unknown value holds whatever the flag was last known to
/// be, so a transition is only ever reported when the device actually said so.
///
/// When there is no previous measurement either — before the first poll of a
/// connection, or on the poll that regains one — the result stays `None`. That
/// is the honest answer, and it is what stops an unknown from being promoted
/// into a fabricated edge.
fn resolve(previous: Option<bool>, current: Option<bool>) -> Option<bool> {
    current.or(previous)
}

impl UpsState {
    /// Builds the next state, resolving every unreadable flag against `prev` so
    /// an unknown value holds rather than reads as a transition.
    pub(crate) fn from_reading_with(prev: &UpsState, r: &Reading) -> Self {
        Self {
            link: Link::Up,
            ac_present: resolve(prev.ac_present, r.ac_present),
            low_battery: resolve(prev.low_battery, r.below_capacity_limit),
            internal_failure: resolve(prev.internal_failure, r.internal_failure),
            overload: resolve(prev.overload, r.overload),
            voltage_out_of_range: resolve(prev.voltage_out_of_range, r.voltage_out_of_range),
            frequency_out_of_range: resolve(prev.frequency_out_of_range, r.frequency_out_of_range),
            boost: resolve(prev.boost, r.boost),
            runtime_limit_expired: resolve(prev.runtime_limit_expired, r.runtime_limit_expired),
        }
    }

    /// Builds a state from a reading with no previous state, so every unknown
    /// flag stays unknown.
    ///
    /// Test-only convenience: the running utility always has a previous state
    /// and calls `from_reading_with`; only the tests build a state from a
    /// reading in isolation. `#[cfg(test)]` rather than an unused-code
    /// suppression.
    #[cfg(test)]
    pub(crate) fn from_reading(r: &Reading) -> Self {
        Self::from_reading_with(&Self::default(), r)
    }

    /// The state after contact is lost, seen from this one.
    ///
    /// Every flag becomes unknown, because nothing is being measured. The link
    /// is where the previous state matters and why this takes `self` instead of
    /// being a constructor: losing a device that was never there is not losing
    /// anything. A utility started with no UPS attached stays at `Never`, so
    /// the connection made an hour later is a first contact rather than a
    /// restoration, and no balloon claims a link was re-established that had
    /// never existed.
    pub(crate) fn contact_lost(&self) -> Self {
        Self {
            link: match self.link {
                Link::Never => Link::Never,
                Link::Lost | Link::Up => Link::Lost,
            },
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    PowerFailure,
    PowerRestored,
    LowBattery,
    DeviceFault,
    Overload,
    Disconnected,
    Reconnected,
    VoltageOutOfRange,
    FrequencyOutOfRange,
    RuntimeLimitExpired,
    BoostStarted,
    BoostEnded,
}

/// The static facts about one kind of event, in one place.
///
/// Everything that used to be spelled out per kind in six different files —
/// the balloon's title and body keys, its severity, the Settings switch that
/// gates it, and the English log phrasing — is here, once. [`Event::of`] fills
/// the first three into the `Event` that `diff` emits; `app::plan_emission`
/// reads `log_text`; `SettingsDraft::EVENT_SWITCHES` reads `switch_key`. A new
/// `EventKind` variant does not compile until [`EventKind::info`] gives it a
/// row, which is the single decision point that used to be six.
pub(crate) struct EventInfo {
    /// Balloon title.
    pub title_key: crate::strings::Key,
    /// Balloon body.
    pub body_key: crate::strings::Key,
    /// Tray icon urgency and log category weight.
    pub severity: Severity,
    /// The Settings switch label, and — being the checkbox's own key — the
    /// thing that ties an event to the control that mutes it.
    pub switch_key: crate::strings::Key,
    /// English, unlocalised log phrasing. The log stays English on purpose:
    /// it is read by whoever diagnoses a problem, often not the machine's
    /// owner and long after the fact.
    pub log_text: &'static str,
}

impl EventKind {
    /// Every kind, in the order Settings lists the switches. The one place the
    /// set is enumerated; the exhaustiveness of [`Self::info`] guarantees each
    /// entry here is fully described.
    ///
    /// Test-only: the program drives events off state transitions in `diff` and
    /// off `EVENT_SWITCHES`, never off this array; the tests use it to walk
    /// every kind. `#[cfg(test)]` rather than an unused-code suppression.
    #[cfg(test)]
    pub(crate) const ALL: [EventKind; 12] = [
        // Mains and battery: the events about the power itself.
        EventKind::PowerFailure,
        EventKind::PowerRestored,
        EventKind::LowBattery,
        EventKind::RuntimeLimitExpired,
        EventKind::VoltageOutOfRange,
        EventKind::FrequencyOutOfRange,
        EventKind::BoostStarted,
        EventKind::BoostEnded,
        // The UPS reporting a fault in itself.
        EventKind::DeviceFault,
        EventKind::Overload,
        // Not about the power at all: the utility's own link to the device.
        EventKind::Disconnected,
        EventKind::Reconnected,
    ];

    /// The static description of this kind. Exhaustive with no wildcard: a new
    /// variant fails to compile until it is described here.
    pub(crate) fn info(self) -> EventInfo {
        use crate::strings::Key as K;
        match self {
            EventKind::PowerFailure => EventInfo {
                title_key: K::NotifyPowerFailureTitle,
                body_key: K::NotifyPowerFailureBody,
                severity: Severity::Warning,
                switch_key: K::SettingsOnPowerFailure,
                log_text: "mains lost, UPS switched to battery",
            },
            EventKind::PowerRestored => EventInfo {
                title_key: K::NotifyPowerRestoredTitle,
                body_key: K::NotifyPowerRestoredBody,
                severity: Severity::Info,
                switch_key: K::SettingsOnPowerRestored,
                log_text: "mains restored, UPS back on line",
            },
            EventKind::LowBattery => EventInfo {
                title_key: K::NotifyLowBatteryTitle,
                body_key: K::NotifyLowBatteryBody,
                severity: Severity::Critical,
                switch_key: K::SettingsOnLowBattery,
                log_text: "battery low, shutdown imminent",
            },
            EventKind::DeviceFault => EventInfo {
                title_key: K::NotifyDeviceFaultTitle,
                body_key: K::NotifyDeviceFaultBody,
                severity: Severity::Critical,
                switch_key: K::SettingsOnDeviceFault,
                log_text: "UPS reported an internal fault",
            },
            EventKind::Overload => EventInfo {
                title_key: K::NotifyOverloadTitle,
                body_key: K::NotifyOverloadBody,
                severity: Severity::Critical,
                switch_key: K::SettingsOnOverload,
                log_text: "UPS reported an output overload",
            },
            EventKind::Disconnected => EventInfo {
                title_key: K::NotifyDisconnectedTitle,
                body_key: K::NotifyDisconnectedBody,
                severity: Severity::Info,
                switch_key: K::SettingsOnConnectionLost,
                log_text: "lost contact with the UPS",
            },
            EventKind::Reconnected => EventInfo {
                title_key: K::NotifyReconnectedTitle,
                body_key: K::NotifyReconnectedBody,
                severity: Severity::Info,
                switch_key: K::SettingsOnConnectionRestored,
                log_text: "contact with the UPS re-established",
            },
            EventKind::VoltageOutOfRange => EventInfo {
                title_key: K::NotifyVoltageOutOfRangeTitle,
                body_key: K::NotifyVoltageOutOfRangeBody,
                severity: Severity::Warning,
                switch_key: K::SettingsOnVoltageOutOfRange,
                log_text: "mains voltage outside the transfer window",
            },
            EventKind::FrequencyOutOfRange => EventInfo {
                title_key: K::NotifyFrequencyOutOfRangeTitle,
                body_key: K::NotifyFrequencyOutOfRangeBody,
                severity: Severity::Warning,
                switch_key: K::SettingsOnFrequencyOutOfRange,
                log_text: "mains frequency outside tolerance",
            },
            EventKind::RuntimeLimitExpired => EventInfo {
                title_key: K::NotifyRuntimeLimitTitle,
                body_key: K::NotifyRuntimeLimitBody,
                severity: Severity::Critical,
                switch_key: K::SettingsOnRuntimeLimit,
                log_text: "remaining runtime below the configured limit",
            },
            EventKind::BoostStarted => EventInfo {
                title_key: K::NotifyBoostTitle,
                body_key: K::NotifyBoostBody,
                severity: Severity::Info,
                switch_key: K::SettingsOnBoostStarted,
                log_text: "AVR engaged, correcting mains voltage",
            },
            EventKind::BoostEnded => EventInfo {
                title_key: K::NotifyBoostEndedTitle,
                body_key: K::NotifyBoostEndedBody,
                severity: Severity::Info,
                switch_key: K::SettingsOnBoostEnded,
                log_text: "AVR disengaged, mains voltage back to normal",
            },
        }
    }
}

impl Event {
    /// Builds the event for a kind from its descriptor, so the metadata is
    /// stated once (in [`EventKind::info`]) rather than at every `diff` edge.
    fn of(kind: EventKind) -> Self {
        let info = kind.info();
        Event {
            title_key: info.title_key,
            body_key: info.body_key,
            severity: info.severity,
            kind,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Event {
    pub title_key: crate::strings::Key,
    pub body_key: crate::strings::Key,
    pub severity: Severity,
    pub kind: EventKind,
}

/// A rising edge between two *measurements*: the device said clear, and now
/// says raised.
///
/// `None` on either side is not an edge and never becomes one. An unknown
/// value is not a measurement of `false`, so promoting `None -> Some(true)` to
/// an edge would report a transition nobody observed — the utility would be
/// announcing when it started looking, not when the device changed. The flag is
/// still visible on the panel throughout, and it fires normally the moment a
/// real clear-to-raised change is seen.
fn raised(prev: Option<bool>, next: Option<bool>) -> bool {
    prev == Some(false) && next == Some(true)
}

/// A falling edge between two measurements. The mirror of [`raised`], and
/// unknown is not an edge here either.
fn cleared(prev: Option<bool>, next: Option<bool>) -> bool {
    prev == Some(true) && next == Some(false)
}

/// Explicit comparison of previous and current state. An event is emitted only
/// on an edge; nothing is suppressed for any other reason. In particular a
/// switch to battery is always reported, whatever the cause.
pub(crate) fn diff(prev: &UpsState, next: &UpsState) -> Vec<Event> {
    let mut events = Vec::new();

    // The link decides first, and exhaustively: every pair of link states has
    // to say what it is, so a new state cannot be added without answering for
    // the nine combinations it joins. The wildcard this replaces let
    // `(Never, Up)` fall through the `!prev.connected && next.connected` arm
    // and announce a reconnection on a first connection.
    match (prev.link, next.link) {
        // Nothing has been observed yet, so there is no transition to report —
        // neither the first contact (which is not a *re*connection) nor a
        // start with no device attached (which loses nothing). This subsumes
        // the old `initialised` guard, and does it without a second field.
        (Link::Never, _) => return events,

        (Link::Up, Link::Lost) => {
            events.push(Event::of(EventKind::Disconnected));
            // Power flags are meaningless while disconnected.
            return events;
        }

        (Link::Lost, Link::Up) => {
            events.push(Event::of(EventKind::Reconnected));
            // Nothing else is reported on the poll that regains contact.
            //
            // While disconnected the utility measures nothing, so every flag
            // in `prev` is unknown rather than observed. There is nothing to
            // compare against, and nothing to recover either — if the mains
            // genuinely failed and returned while the cable was out, the
            // device never told us, and reporting either transition would be
            // a guess. The next poll compares two real readings and reports
            // anything still standing.
            //
            // The early return is belt to the braces of `raised`/`cleared`:
            // with every flag unknown in `prev`, no edge would fire from this
            // comparison anyway. It stays because the reconnection is
            // deliberately the only thing announced on this poll, and that
            // decision should be readable here rather than deduced from the
            // edge rules.
            //
            // The flags themselves are not lost: `next` is a full reading, so
            // a fault raised while disconnected is already in the state and
            // will be reported the moment it changes, or seen in the panel
            // immediately.
            return events;
        }

        // Still out of contact: nothing is measured, so nothing can have
        // changed.
        (Link::Lost, Link::Lost) => return events,

        // Unreachable by construction — `contact_lost` only ever returns
        // `Never` from `Never`, and `from_reading_with` always returns `Up` —
        // and stated rather than left to a wildcard. If it ever happens, the
        // honest reading is that contact has not been observed, which is the
        // one thing that cannot invent an event.
        (Link::Lost | Link::Up, Link::Never) => return events,

        // Two readings from a device that answered both times: the only case
        // where the flags below mean anything.
        (Link::Up, Link::Up) => {}
    }

    if cleared(prev.ac_present, next.ac_present) {
        events.push(Event::of(EventKind::PowerFailure));
    } else if raised(prev.ac_present, next.ac_present) {
        events.push(Event::of(EventKind::PowerRestored));
    }

    if raised(prev.low_battery, next.low_battery) {
        events.push(Event::of(EventKind::LowBattery));
    }

    if raised(prev.internal_failure, next.internal_failure) {
        events.push(Event::of(EventKind::DeviceFault));
    }

    if raised(prev.overload, next.overload) {
        events.push(Event::of(EventKind::Overload));
    }

    // Mains quality faults. Rising edge only, like the faults above: the
    // device raising the flag is the event, and its clearing is implied by
    // the mains readings resuming normal values.
    if raised(prev.voltage_out_of_range, next.voltage_out_of_range) {
        events.push(Event::of(EventKind::VoltageOutOfRange));
    }

    if raised(prev.frequency_out_of_range, next.frequency_out_of_range) {
        events.push(Event::of(EventKind::FrequencyOutOfRange));
    }

    // Critical: the battery is nearly spent by the device's own reckoning,
    // which is a shorter warning than the charge percentage gives.
    if raised(prev.runtime_limit_expired, next.runtime_limit_expired) {
        events.push(Event::of(EventKind::RuntimeLimitExpired));
    }

    // AVR is the one flag reported on *both* edges, and the only one that is
    // not a fault. It is a period rather than a moment — the UPS correcting
    // mains without touching the battery — and a start with no matching end
    // would leave the log saying the correction is still running long after
    // it stopped. Severity is Info: nothing here asks the user to act.
    if raised(prev.boost, next.boost) {
        events.push(Event::of(EventKind::BoostStarted));
    } else if cleared(prev.boost, next.boost) {
        events.push(Event::of(EventKind::BoostEnded));
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading from a healthy UPS with every flag actually read.
    ///
    /// `Reading::default()` leaves each flag `None`, and `None` now means "not
    /// measured" rather than "clear" — a state built from it describes a device
    /// that reported nothing, and there is no edge to compute against nothing.
    /// A real poll of this device fills all eight of these from reports 11 and
    /// 23, so the fixture that stands in for a poll fills them too.
    fn healthy() -> Reading {
        Reading {
            ac_present: Some(true),
            below_capacity_limit: Some(false),
            internal_failure: Some(false),
            overload: Some(false),
            voltage_out_of_range: Some(false),
            frequency_out_of_range: Some(false),
            boost: Some(false),
            runtime_limit_expired: Some(false),
            ..Default::default()
        }
    }

    /// A UPS that has been polled once and is on mains with nothing wrong.
    fn online() -> UpsState {
        UpsState::from_reading(&healthy())
    }

    /// Why a failed *first* connect cannot be logged from this module.
    ///
    /// `diff` is edge-triggered, and at startup there is no edge: the state
    /// begins at `Link::Never` and the first `Disconnected` message leaves it
    /// there, so no `Up -> Lost` boundary is crossed.
    ///
    /// No event means no log line, which is exactly how a utility that never
    /// found the device came to write `SESSION started` and then nothing at
    /// all for hours.
    ///
    /// That behaviour is right for notifications — a balloon announcing that
    /// an absent UPS is still absent would be noise — so the fix belongs in
    /// `poller::run`, at the point the attempt actually fails. This test
    /// pins the reason so the failure is not "fixed" here later by making
    /// `diff` fire on an edge that does not exist.
    #[test]
    fn no_event_exists_for_a_failed_first_connect() {
        let fresh = UpsState::default();
        let events = diff(&fresh, &fresh.contact_lost());
        assert!(
            events.is_empty(),
            "startup has no transition to report; the poller must log the failure itself"
        );
    }

    /// A first connection is not a reconnection.
    ///
    /// The bug this pins: starting the utility before plugging in the UPS gave
    /// `Message::Disconnected` first, which moved the state to "was connected,
    /// now lost". Plugging the device in then crossed `false -> true` and
    /// produced the balloon "contact with the UPS re-established" — for a
    /// contact that had never been established. The same class as the phantom
    /// power events: an announcement about a transition nobody observed.
    ///
    /// Two steps, because one is not enough to catch it: the state has to go
    /// through the failed start before the connection is offered.
    #[test]
    fn connecting_after_a_start_with_no_device_is_not_a_reconnection() {
        let start = UpsState::default();
        let no_device = start.contact_lost();
        assert!(
            diff(&start, &no_device).is_empty(),
            "an absent device at startup is not a lost connection"
        );

        let first_contact = UpsState::from_reading_with(&no_device, &healthy());
        assert!(
            diff(&no_device, &first_contact).is_empty(),
            "the first connection of a session is not a restoration of anything"
        );
    }

    /// And the genuine article still fires: contact that was up, went down and
    /// came back is exactly what `Reconnected` is for. Kept beside the test
    /// above so that suppressing the phantom cannot be mistaken for suppressing
    /// the event.
    #[test]
    fn a_real_reconnection_still_fires() {
        let up = online();
        let lost = up.contact_lost();
        assert_eq!(
            diff(&up, &lost).iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![EventKind::Disconnected]
        );
        let back = UpsState::from_reading_with(&lost, &healthy());
        assert_eq!(
            diff(&lost, &back)
                .iter()
                .map(|e| e.kind)
                .collect::<Vec<_>>(),
            vec![EventKind::Reconnected]
        );
    }

    /// Every fault flag the panel can display must also produce an event.
    ///
    /// This is the bug this test exists for: four flags were added to the
    /// panel — voltage out of range, frequency out of range, AVR, runtime
    /// limit — and to nothing else. The device could raise and clear them all
    /// day while the log stayed silent and no balloon ever appeared, because
    /// `UpsState` did not track them and `diff` therefore had no edge to see.
    /// A flag that is only rendered is a flag that is only noticed by someone
    /// already looking at the window.
    #[test]
    fn every_displayed_flag_produces_an_event() {
        let base = healthy();
        let prev = UpsState::from_reading(&base);

        // Each flag on its own, so a shared cause cannot mask a missing one.
        let cases: [(&str, Reading); 4] = [
            (
                "voltage_out_of_range",
                Reading {
                    voltage_out_of_range: Some(true),
                    ..base.clone()
                },
            ),
            (
                "frequency_out_of_range",
                Reading {
                    frequency_out_of_range: Some(true),
                    ..base.clone()
                },
            ),
            (
                "runtime_limit_expired",
                Reading {
                    runtime_limit_expired: Some(true),
                    ..base.clone()
                },
            ),
            (
                "boost",
                Reading {
                    boost: Some(true),
                    ..base.clone()
                },
            ),
        ];

        for (name, reading) in cases {
            let next = UpsState::from_reading(&reading);
            let events = diff(&prev, &next);
            assert!(
                !events.is_empty(),
                "{name} was raised by the device and produced no event at all"
            );
        }
    }

    /// AVR is reported when it ends as well as when it starts.
    ///
    /// It describes a period, not a moment. Logging only the start leaves the
    /// file asserting the correction is still running long after it stopped.
    #[test]
    fn avr_reports_both_edges() {
        let off = online();
        let on = UpsState {
            boost: Some(true),
            ..off
        };

        assert!(
            diff(&off, &on)
                .iter()
                .any(|e| e.kind == EventKind::BoostStarted),
            "engaging AVR must be reported"
        );
        assert!(
            diff(&on, &off)
                .iter()
                .any(|e| e.kind == EventKind::BoostEnded),
            "AVR ending must be reported too, or the log never closes the period"
        );
    }

    /// Exhaustive audit: every alarm the device can raise reaches the user.
    ///
    /// This drives a real transition for each `EventKind` through `diff` and
    /// checks the result end to end — the event fires, and its title and body
    /// resolve to actual text in the embedded locale rather than falling
    /// through to the raw key.
    ///
    /// It exists because the failure mode here is silent. Four flags were
    /// added to the panel and to nothing else; nothing crashed, no test went
    /// red, and the only symptom was a log that stayed quiet during exactly
    /// the events it is kept for. The `match` below is exhaustive, so a new
    /// variant added to `EventKind` will not compile until someone decides
    /// how it is triggered — which is the point.
    #[test]
    fn every_event_kind_fires_and_has_text() {
        use EventKind as K;

        let online = online();
        let en = crate::lang::Locale::english();

        for kind in EventKind::ALL {
            // The (prev, next) pair that must produce this event. Exhaustive
            // by construction: a new variant forces a decision here.
            let (prev, next) = match kind {
                K::PowerFailure => (
                    online,
                    UpsState {
                        ac_present: Some(false),
                        ..online
                    },
                ),
                K::PowerRestored => (
                    UpsState {
                        ac_present: Some(false),
                        ..online
                    },
                    online,
                ),
                K::LowBattery => (
                    online,
                    UpsState {
                        low_battery: Some(true),
                        ..online
                    },
                ),
                K::DeviceFault => (
                    online,
                    UpsState {
                        internal_failure: Some(true),
                        ..online
                    },
                ),
                K::Overload => (
                    online,
                    UpsState {
                        overload: Some(true),
                        ..online
                    },
                ),
                K::Disconnected => (online, online.contact_lost()),
                K::Reconnected => (online.contact_lost(), online),
                K::VoltageOutOfRange => (
                    online,
                    UpsState {
                        voltage_out_of_range: Some(true),
                        ..online
                    },
                ),
                K::FrequencyOutOfRange => (
                    online,
                    UpsState {
                        frequency_out_of_range: Some(true),
                        ..online
                    },
                ),
                K::RuntimeLimitExpired => (
                    online,
                    UpsState {
                        runtime_limit_expired: Some(true),
                        ..online
                    },
                ),
                K::BoostStarted => (
                    online,
                    UpsState {
                        boost: Some(true),
                        ..online
                    },
                ),
                K::BoostEnded => (
                    UpsState {
                        boost: Some(true),
                        ..online
                    },
                    online,
                ),
            };

            let events = diff(&prev, &next);
            let ev = events
                .iter()
                .find(|e| e.kind == kind)
                .unwrap_or_else(|| panic!("{kind:?} never fires: the device can raise it and the utility would say nothing"));

            assert_ne!(
                en.t(ev.title_key),
                ev.title_key.label(),
                "{kind:?}: title key {} has no text; the balloon would show the key itself",
                ev.title_key.label()
            );
            assert_ne!(
                en.t(ev.body_key),
                ev.body_key.label(),
                "{kind:?}: body key {} has no text",
                ev.body_key.label()
            );
        }
    }

    /// Every event kind is reachable from exactly one switch in Settings.
    ///
    /// `EVENT_SWITCHES` now pairs each `EventKind` with a hotspot directly, so
    /// this checks the two lists agree: every kind in `EventKind::ALL` appears
    /// once in the switch table, and the table introduces no kind that `ALL`
    /// does not know. `ALL` is exhaustive by construction (a new variant must
    /// be added to it), so a kind with no switch fails here.
    ///
    /// The gap it guards: twelve kinds were once gated by four switches —
    /// overload shared the device-fault switch, both mains-quality flags the
    /// power-failure switch, the runtime limit the low-battery switch, the
    /// connection pair nothing at all — so muting one event silently muted
    /// others. The mapping is one-to-one now, which is the property to keep.
    #[test]
    fn every_event_kind_has_a_settings_switch() {
        use crate::ui::settings::SettingsDraft;

        let switched: Vec<EventKind> = SettingsDraft::EVENT_SWITCHES
            .iter()
            .map(|(kind, _)| *kind)
            .collect();

        for kind in EventKind::ALL {
            assert_eq!(
                switched.iter().filter(|&&k| k == kind).count(),
                1,
                "{kind:?} must have exactly one switch"
            );
        }
        assert_eq!(
            switched.len(),
            EventKind::ALL.len(),
            "the switch table lists a kind that EventKind::ALL does not"
        );
    }

    /// Reconnecting must not be reported as a power event.
    ///
    /// The bug this pins, observed on a real device: pulling and replugging
    /// the USB cable produced "mains restored" for a UPS that had been on
    /// mains the entire time. Nothing about the power changed — only the
    /// cable did.
    ///
    /// The cause is that the disconnected state used to be built from
    /// `Default`, so `ac_present` was false while disconnected. That false is a
    /// placeholder for "not measured", not an observation of mains loss, and
    /// comparing the first real reading against it manufactures a rising
    /// edge. It is the same error as the phantom "switched to battery"
    /// balloons, arriving from the other direction: a value that was never
    /// measured being treated as a measurement.
    #[test]
    fn reconnecting_does_not_report_a_power_transition() {
        let on_mains = online();

        // Contact is lost, then regained with the UPS still on mains.
        let lost = diff(&on_mains, &online().contact_lost());
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].kind, EventKind::Disconnected);

        let regained = diff(&online().contact_lost(), &on_mains);
        let kinds: Vec<_> = regained.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![EventKind::Reconnected],
            "regaining contact is one event; the mains never changed"
        );
    }

    /// The same, for a UPS that really is on battery when contact returns.
    ///
    /// This is the case the old code got backwards in the more damaging
    /// direction: it would have announced "mains restored" while the device
    /// was still running on its battery, because `prev.ac_present` was the
    /// disconnect placeholder rather than a reading.
    #[test]
    fn reconnecting_on_battery_does_not_claim_mains_returned() {
        let on_battery = UpsState {
            ac_present: Some(false),
            ..online()
        };
        let events = diff(&online().contact_lost(), &on_battery);
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(
            !kinds.contains(&EventKind::PowerRestored),
            "a UPS on battery must never be reported as back on mains"
        );
        assert_eq!(kinds, vec![EventKind::Reconnected]);
    }

    /// A fault standing at reconnect is not announced, but is not lost.
    ///
    /// The poll that regains contact reports only the reconnect: every field
    /// in the previous state is a placeholder, so any "edge" against it is
    /// invented. The reading itself is still stored, so the flag is in the
    /// panel and the tray icon at once, and the next genuine change is
    /// reported normally. An unannounced flag that is visible everywhere is a
    /// far smaller fault than a fabricated event that is not true.
    #[test]
    fn a_fault_present_at_reconnect_is_reported_when_it_next_changes() {
        let faulted = UpsState {
            overload: Some(true),
            ..online()
        };

        // Arriving with the fault already up: only the reconnect is reported.
        let arrival = diff(&online().contact_lost(), &faulted);
        assert_eq!(
            arrival.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![EventKind::Reconnected]
        );

        // The state now holds the fault, so it does not re-fire while it
        // stands...
        assert!(diff(&faulted, &faulted).is_empty());

        // ...and clearing then re-raising it reports normally.
        let cleared = online();
        assert!(diff(&faulted, &cleared).is_empty(), "faults clear silently");
        let raised = diff(&cleared, &faulted);
        assert_eq!(
            raised.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![EventKind::Overload]
        );
    }

    /// Normal battery states are deliberately not alarms.
    ///
    /// `charging`, `discharging` and `fully_charged` describe what the
    /// battery is doing, not something wrong. They are excluded from
    /// `UpsState` on purpose: a UPS charging after an outage would otherwise
    /// notify twice for one event, and `ac_present` already reports the
    /// transition that matters.
    #[test]
    fn ordinary_battery_states_are_not_alarms() {
        let base = healthy();
        let prev = UpsState::from_reading(&base);

        for reading in [
            Reading {
                charging: Some(true),
                ..base.clone()
            },
            Reading {
                discharging: Some(true),
                ..base.clone()
            },
            Reading {
                fully_charged: Some(true),
                ..base.clone()
            },
        ] {
            let next = UpsState::from_reading(&reading);
            assert!(
                diff(&prev, &next).is_empty(),
                "an ordinary battery state must not raise an alarm"
            );
        }
    }

    /// A flag that stays raised must not re-fire on every poll.
    #[test]
    fn a_held_flag_fires_once_not_every_poll() {
        let raised = UpsState {
            voltage_out_of_range: Some(true),
            frequency_out_of_range: Some(true),
            runtime_limit_expired: Some(true),
            boost: Some(true),
            ..online()
        };
        assert!(
            diff(&raised, &raised).is_empty(),
            "an unchanged state is not an event"
        );
    }

    #[test]
    fn first_poll_is_silent() {
        let prev = UpsState::default();
        assert!(diff(&prev, &online()).is_empty());
    }

    #[test]
    fn steady_state_repeats_nothing() {
        let s = online();
        assert!(diff(&s, &s).is_empty());
        let on_batt = UpsState {
            ac_present: Some(false),
            ..online()
        };
        assert!(diff(&on_batt, &on_batt).is_empty());
    }

    #[test]
    fn power_transitions_fire_once_each_way() {
        let a = online();
        let b = UpsState {
            ac_present: Some(false),
            ..online()
        };
        let out = diff(&a, &b);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::PowerFailure);

        let back = diff(&b, &a);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].kind, EventKind::PowerRestored);
    }

    #[test]
    fn disconnect_suppresses_stale_power_flags() {
        let on_batt = UpsState {
            ac_present: Some(false),
            low_battery: Some(true),
            ..online()
        };
        let out = diff(&on_batt, &online().contact_lost());
        assert_eq!(out.len(), 1, "only the disconnect should be reported");
        assert_eq!(out[0].kind, EventKind::Disconnected);
    }

    /// A reading whose mains flag is either a confirmed value (`known == true`)
    /// or unread (`known == false`, giving `None`). The `known` parameter is the
    /// whole point of these tests: it is how "the flag did not read" is
    /// expressed now that the reading carries `Option<bool>` instead of a value
    /// plus a companion `ac_present_known`.
    fn reading(ac: bool, known: bool) -> Reading {
        Reading {
            ac_present: known.then_some(ac),
            ..Default::default()
        }
    }

    /// The phantom-notification bug, stated directly. A failed read must not
    /// be reported as a power failure.
    #[test]
    fn unreadable_ac_flag_does_not_fire_a_power_failure() {
        let prev = online();
        // The device is on mains; this poll could not read the flag at all.
        let next = UpsState::from_reading_with(&prev, &reading(false, false));
        assert_eq!(
            next.ac_present,
            Some(true),
            "unknown AC must hold the previous measurement"
        );
        assert!(
            diff(&prev, &next).is_empty(),
            "an unreadable flag must not be reported as a transition"
        );
    }

    /// The fix must not go too far: a real outage still fires.
    #[test]
    fn genuine_power_failure_still_fires() {
        let prev = online();
        let next = UpsState::from_reading_with(&prev, &reading(false, true));
        assert_eq!(next.ac_present, Some(false));
        let out = diff(&prev, &next);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::PowerFailure);
    }

    /// While genuinely on battery, an unreadable flag must not be reported as
    /// a restore either — the hold works in both directions.
    #[test]
    fn unreadable_ac_flag_does_not_fire_a_restore() {
        let prev = UpsState {
            ac_present: Some(false),
            ..online()
        };
        let next = UpsState::from_reading_with(&prev, &reading(true, false));
        assert_eq!(
            next.ac_present,
            Some(false),
            "unknown AC must hold on-battery too"
        );
        assert!(diff(&prev, &next).is_empty());
    }

    /// With no previous reading to hold, the flag stays unknown.
    ///
    /// It used to take a startup default of "on mains", which was the least
    /// alarming guess but a guess all the same. The default existed because the
    /// field could not say "unknown"; now that it can, nothing has to be
    /// invented. Nothing downstream regressed by removing it: `diff` is silent
    /// until contact has been observed either way, and the panel and tray read
    /// the raw `Reading`, not this state.
    #[test]
    fn unknown_ac_before_first_poll_stays_unknown() {
        let prev = UpsState::default();
        let next = UpsState::from_reading_with(&prev, &reading(false, false));
        assert_eq!(next.ac_present, None);
        assert!(
            diff(&prev, &next).is_empty(),
            "the first poll reports nothing in any case"
        );
    }

    /// A8 generalised: the hold-previous rule protects every flag, not just AC.
    /// An overload that was set and then fails to read must not be reported as
    /// clearing, and — the phantom case — must not fire a fresh notification
    /// when it reads again. This is the same bug the AC tests pin, for a fault
    /// flag, proving the fix is the type of every flag rather than a special
    /// case for mains.
    #[test]
    fn an_unreadable_fault_flag_holds_and_does_not_fire() {
        // Overload is active and known.
        let overloaded = Reading {
            overload: Some(true),
            ..Default::default()
        };
        let prev = UpsState::from_reading(&overloaded);
        assert_eq!(prev.overload, Some(true));

        // Next poll cannot read overload: it must hold true, not clear, so no
        // spurious "overload ended" and no edge to fire on when it returns.
        let unread = Reading {
            overload: None,
            ..Default::default()
        };
        let next = UpsState::from_reading_with(&prev, &unread);
        assert_eq!(
            next.overload,
            Some(true),
            "an unread fault must hold its previous measurement"
        );
        assert!(
            diff(&prev, &next).is_empty(),
            "an unread fault must not be reported as a transition"
        );
    }

    /// The three-poll sequence that produced a phantom "mains restored", and
    /// the reason the two tests either side of it did not catch it.
    ///
    /// 1. The cable is pulled. Every flag becomes unknown.
    /// 2. The cable goes back in and the first poll arrives, but the mains flag
    ///    does not read. There is no previous *measurement* to hold, so it
    ///    stays unknown — where it used to fall back on the `false` the
    ///    disconnected state left behind and show the UPS as on battery.
    /// 3. The next poll reads the flag: mains present, as it had been all
    ///    along. This is the poll that used to fire `PowerRestored` — a
    ///    balloon, a log line and a NOTIFY entry for an event that never
    ///    happened.
    ///
    /// `reconnecting_does_not_report_a_power_transition` misses this because it
    /// reconnects straight into a successful read, and the early return covers
    /// that poll. `unreadable_ac_flag_does_not_fire_a_restore` misses it
    /// because it never disconnects. The defect lived in the combination.
    #[test]
    fn a_failed_read_after_reconnecting_does_not_invent_a_restore() {
        let on_mains = online();

        // 1. Contact lost.
        let lost = online().contact_lost();
        assert_eq!(
            diff(&on_mains, &lost)
                .iter()
                .map(|e| e.kind)
                .collect::<Vec<_>>(),
            vec![EventKind::Disconnected]
        );
        assert_eq!(
            lost.ac_present, None,
            "nothing is being measured while disconnected"
        );

        // 2. Contact regained, but the mains flag does not read.
        let regained = UpsState::from_reading_with(&lost, &reading(false, false));
        assert_eq!(
            regained.ac_present, None,
            "there is no previous measurement to hold, so it must stay unknown"
        );
        assert_eq!(
            diff(&lost, &regained)
                .iter()
                .map(|e| e.kind)
                .collect::<Vec<_>>(),
            vec![EventKind::Reconnected]
        );

        // 3. The flag reads: mains, exactly as before the cable was touched.
        let measured = UpsState::from_reading_with(&regained, &reading(true, true));
        assert_eq!(measured.ac_present, Some(true));
        assert!(
            diff(&regained, &measured).is_empty(),
            "the mains never changed; learning its value is not an event"
        );
    }

    /// Unknown is not an edge in either direction, for any flag.
    ///
    /// This is the rule that makes the sequence above impossible rather than
    /// merely untriggered: an event requires two measurements, and `None` is
    /// not one. Stated on `raised`/`cleared` directly so it is pinned at the
    /// rule rather than at one of the eleven places that apply it.
    #[test]
    fn an_unknown_value_is_never_an_edge() {
        for known in [Some(true), Some(false)] {
            assert!(!raised(None, known), "unknown -> {known:?} is not a rise");
            assert!(!raised(known, None), "{known:?} -> unknown is not a rise");
            assert!(!cleared(None, known), "unknown -> {known:?} is not a fall");
            assert!(!cleared(known, None), "{known:?} -> unknown is not a fall");
        }
        assert!(!raised(None, None));
        assert!(!cleared(None, None));

        // ...and a real change between two measurements still is one.
        assert!(raised(Some(false), Some(true)));
        assert!(cleared(Some(true), Some(false)));
    }

    #[test]
    fn simultaneous_faults_all_reported() {
        let a = online();
        let b = UpsState {
            ac_present: Some(false),
            low_battery: Some(true),
            overload: Some(true),
            ..online()
        };
        let kinds: Vec<_> = diff(&a, &b).into_iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::PowerFailure));
        assert!(kinds.contains(&EventKind::LowBattery));
        assert!(kinds.contains(&EventKind::Overload));
    }
}
