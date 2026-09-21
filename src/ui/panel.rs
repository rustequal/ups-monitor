//! Builds the panel's contents as data.
//!
//! Absent fields (frequency, temperature, `NeedReplacement`) are omitted rather
//! than rendered as zeros: this device does not report them, and a zero would
//! read as a real measurement.
//!
//! Producing rows as data rather than painting directly keeps every layout
//! decision — which rows appear, what colour a value takes, what the tray
//! tooltip says — testable without a window.

use super::format::format_runtime;
use super::row::{Gap, Row, HOT_BEEPER, HOT_SELFTEST, HOT_SETTINGS};
use crate::app::{BeeperView, Presence};
use crate::color::Color;
use crate::hid::{Identity, Reading, TestResult, MIN_CHARGE_PERCENT};
use crate::lang::Locale;
use crate::strings;
use crate::ui::theme::Theme;
/// A snapshot of everything the panel draws from, borrowed for one build.
#[derive(Clone)]
pub(crate) struct PanelData<'a> {
    pub reading: Option<&'a Reading>,
    pub identity: Option<&'a Identity>,
    pub presence: Presence,
    /// False until the first poll has reported. Drives the startup skeleton.
    /// What is known about the buzzer mode: current, stale, or nothing at all.
    ///
    /// One value rather than a mode beside a freshness flag, so the row has
    /// exactly the three shapes the type has. See [`push_beeper_row`].
    pub beeper: BeeperView,
    /// Standing complaints, already rendered in the interface language and in
    /// display order — the device first, then the configuration file.
    ///
    /// A list rather than one message. They come from independent sources and
    /// can stand at the same time: an unwritable INI does not stop a second
    /// UPS from being attached. Held as a single `Option<String>`, whichever
    /// arrived last hid the other, and answering either one cleared both.
    pub warnings: Vec<String>,
    /// True while the panel is drawn as the startup skeleton, before the
    /// device has answered anything.
    ///
    /// Needed because two rows are derived from booleans rather than from
    /// `Option`s — charge state and power state — and a `bool` has no "not
    /// read yet" value. On a default `Reading` they resolved to concrete,
    /// confident, wrong answers: `Idle` and `On battery`, shown next to a
    /// column of dashes and indistinguishable from a real measurement. That is
    /// precisely the failure the dash exists to prevent, and it appeared on
    /// every start of the utility.
    ///
    /// A flag rather than making those fields `Option`: the device always
    /// reports them together with the report they share, so absence is a
    /// property of the whole poll rather than of any one field, and more
    /// `Option`s would put a decision at every use site that only this one
    /// place needs to make.
    ///
    /// The buzzer was a third such row and is not one any more. Its mode is
    /// now an `Option` on the reading, and the row is drawn from that and from
    /// the last mode seen — so the skeleton needs no flag to blank it, because
    /// the skeleton holds neither. The self-test row still consults this flag,
    /// for the reason given where it does: it judges safety from a placeholder
    /// reading whose flags default to plausible answers.
    pub probing: bool,
    /// True while a self-test *this utility asked for* is under way. Greys the
    /// Self-test button.
    ///
    /// Not "a test is running" — the device's `Test` register answers that, and
    /// answers it for tests started from the front panel of the UPS too. This
    /// covers the window the register cannot: between queueing a request and
    /// the device reporting it. The row joins the two; see [`push_self_test_row`].
    pub self_test_running: bool,
}

/// Which of the panel's three shapes the data calls for.
///
/// The combination that used to need guarding — open, probed, and holding no
/// reading — is not a value of this type, so it cannot be passed on to the
/// code that would have to cope with it. That is the whole reason the enum
/// exists: `PanelData` can represent it, because `presence` and `reading` are
/// independent fields, and the classification below is where the two stop
/// being independent.
enum Shown<'a> {
    /// Nothing has come back from the poll thread yet, or the device is open
    /// and has not reported a reading. One variant for both because they draw
    /// the same thing — the skeleton — and telling them apart here would be a
    /// distinction the panel does not act on.
    Connecting,
    /// The poll thread answered and there is no device to talk to.
    Absent,
    /// The device is open and this is its latest reading.
    Live { reading: &'a Reading },
}

impl<'a> PanelData<'a> {
    /// Which shape this data calls for.
    ///
    /// Exhaustive over the pair, with no fallback arm: every combination of
    /// `presence` and `reading` names its shape here, so adding a `Presence`
    /// variant is a compile error in this function rather than a silent
    /// default somewhere below it.
    fn shown(&self) -> Shown<'a> {
        match (self.presence, self.reading) {
            (Presence::Unprobed, _) | (Presence::Open, None) => Shown::Connecting,
            (Presence::Absent, _) => Shown::Absent,
            (Presence::Open, Some(reading)) => Shown::Live { reading },
        }
    }
}

/// Colour for a load percentage. Thresholds match the tray icon so the panel
/// and the icon never disagree about severity.
pub(crate) fn load_color(percent: u32, theme: &Theme) -> Color {
    if percent >= 90 {
        theme.colors.critical
    } else if percent >= 75 {
        theme.colors.warning
    } else {
        theme.colors.text_primary
    }
}

/// Colour for a charge percentage.
pub(crate) fn charge_color(percent: u32, theme: &Theme) -> Color {
    if percent <= 20 {
        theme.colors.critical
    } else if percent <= 50 {
        theme.colors.warning
    } else {
        theme.colors.ok
    }
}

/// Em dash used for a value that is not known yet.
const EM_DASH: &str = "\u{2014}";

/// The fault lines, in the order the Status block shows them.
///
/// A named list because it is read twice: once to decide which faults are
/// showing, and once by [`width_samples`] to reserve room for the widest of
/// them. Two copies of the order would let a flag be added to one and not the
/// other, and the symptom — a window that steps outward the first time that
/// one fault appears — is exactly what the samples exist to prevent.
const FAULT_KEYS: [strings::Key; 6] = [
    strings::Key::FlagLowBattery,
    strings::Key::FlagRuntimeLimitExpired,
    strings::Key::FlagInternalFailure,
    strings::Key::FlagOverload,
    strings::Key::FlagVoltageOutOfRange,
    strings::Key::FlagFrequencyOutOfRange,
];

/// The title line, with Settings right-aligned. Exactly one per panel.
fn title_row(locale: Locale) -> [Row; 2] {
    [
        Row::TitleButton {
            title: locale.t(strings::Key::PanelTitle).to_owned(),
            button: locale.t(strings::Key::MenuSettings).to_owned(),
            id: HOT_SETTINGS,
        },
        Row::Space(Gap::Half),
    ]
}

/// Builds the rows for the current state.
pub(crate) fn build(locale: Locale, theme: &Theme, data: &PanelData) -> Vec<Row> {
    let mut rows = Vec::new();

    // Title line with Settings in the top-right corner.
    //
    // Reachable from the panel as well as the tray: a user with the panel
    // already open should not have to go back to the tray icon to reach the
    // options for the window in front of them. It sits at the top rather than
    // at the bottom because the panel is a column of readings of varying
    // length — a button after the last one lands at a different height
    // depending on how many rows the device reported, which is both untidy
    // and a moving target for the pointer.
    rows.extend(title_row(locale));

    // Shown above everything else: a warning the user never sees is the same
    // as no warning at all. All of them, because two can stand at once and
    // showing only one leaves the other silently unreported.
    if !data.warnings.is_empty() {
        for warning in &data.warnings {
            rows.push(Row::Wrapped {
                text: warning.clone(),
                color: theme.colors.warning,
            });
        }
        rows.push(Row::Space(Gap::Single));
    }

    // Before the first poll has reported, the layout is drawn in full with
    // placeholder values rather than showing either the error view or a bare
    // "reading" line.
    //
    // Both alternatives were visibly wrong at startup: `connected` is false
    // until the first message arrives, so opening the panel produced the
    // device-not-found view — a small window full of error text — which then
    // jumped to full size a poll interval later. The window must appear at
    // its final size immediately, with the controls already in place; only
    // the values arrive late, because only the values are actually late.
    // The skeleton covers two distinct gaps, both of which used to resize the
    // window: before any poll result has arrived, and after `Connected` but
    // before the first `Update` — because `Connected` carries the identity and
    // no reading, and the branch below would have drawn a bare "reading" line
    // in a much shorter window.
    // The reading, or the whole panel, depending on what there is to draw.
    //
    // The three shapes the panel takes are decided once, by [`PanelData::shown`],
    // and arrive here as one value. Written as three conditions in a row it was
    // possible to fall past all of them holding no reading, and the only thing
    // that said otherwise was a `debug_assert!(false)` under a `let ... else` —
    // a claim about a state the type admitted, checked in a build this project
    // does not produce. `Shown::Live` carries a `&Reading`, not an `Option`, so
    // there is nothing left here to unwrap and nothing to assert about.
    let r = match data.shown() {
        Shown::Connecting => {
            rows.extend(skeleton(locale, theme));
            return rows;
        }
        Shown::Absent => {
            rows.push(Row::Space(Gap::Single));
            rows.push(Row::Wrapped {
                text: locale.t(strings::Key::StateDisconnected).to_owned(),
                color: theme.colors.critical,
            });
            rows.push(Row::Space(Gap::Half));
            rows.push(Row::Wrapped {
                text: locale.t(strings::Key::ErrorDeviceNotFound).to_owned(),
                color: theme.colors.text_primary,
            });
            rows.push(Row::Space(Gap::Half));
            rows.push(Row::Wrapped {
                text: locale.t(strings::Key::ErrorDeviceNotFoundHint).to_owned(),
                color: theme.colors.text_secondary,
            });
            return rows;
        }
        Shown::Live { reading } => reading,
    };

    // A measurement row that is always present.
    //
    // An unreadable field renders as an em dash, not as an absent row and not
    // as the previous value. Both alternatives were tried and both mislead:
    //
    // * Omitting the row makes it vanish and pulls every row below it upward,
    //   so the panel appears to lose a metric the device is still reporting.
    //   Fields are grouped into feature reports, so one failed transfer takes
    //   out the handful of readings that share a report while its neighbours
    //   survive.
    // * Holding the last good value is worse, because it is indistinguishable
    //   from a live reading. Someone watching a frozen "Load 11%" during an
    //   outage has no way to tell it is stale, and a stale number presented as
    //   current is the one failure mode a monitoring tool must not have.
    //
    // The dash says exactly what is true: the utility has no value for this
    // right now. It is the same glyph the startup skeleton uses, for the same
    // reason.

    push_input_rows(&mut rows, locale, theme, r);

    push_output_rows(&mut rows, locale, theme, r);

    push_battery_rows(&mut rows, locale, theme, data, r);

    // Status.
    rows.push(Row::Header(
        locale.t(strings::Key::PanelGroupStatus).to_owned(),
    ));
    // Mains drives the row a user checks first during an outage, so an unread
    // flag must not be rendered as "on battery": `None` gives a dash, not a
    // false claim that the mains are gone. This is the same statement the dash
    // makes everywhere else, and the reason the flag is an `Option`.
    let (state_text, state_color) = match (data.probing, r.ac_present) {
        (false, Some(true)) => (
            Some(locale.t(strings::Key::StateOnline).to_owned()),
            theme.colors.ok,
        ),
        (false, Some(false)) => (
            Some(locale.t(strings::Key::StateOnBattery).to_owned()),
            theme.colors.warning,
        ),
        _ => (None, theme.colors.text_primary),
    };
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelState),
        state_text,
        state_color,
    ));

    push_fault_rows(&mut rows, locale, theme, r);

    push_device_rows(&mut rows, locale, theme, data, r);

    rows
}

/// The device group: identity strings, the nameplate figure, the buzzer
/// control and the self-test row.
///
/// One function because the order inside it is the point — it is argued
/// for at length below and mirrored by the panel exactly — and an order
/// that matters is easier to keep right when the whole of it is in view.
fn push_device_rows(
    rows: &mut Vec<Row>,
    locale: Locale,
    theme: &Theme,
    data: &PanelData,
    r: &Reading,
) {
    // The nameplate is the only place both units appear together, so they
    // are read here rather than carried in from the caller.
    let watt = locale.t(strings::Key::UnitWatt);
    let volt_amp = locale.t(strings::Key::UnitVoltAmp);

    // Device identity, the nameplate figure, the buzzer control, and the
    // self-test row.
    //
    // Order here is deliberate and mirrored by the panel exactly. Rated power
    // sits up between the serial number and the manufacturer, not down by the
    // last test: at the bottom its value ran up against the Self-test button on
    // the line below and the two read as one field. The two clickable controls —
    // the buzzer and the self-test — are still kept apart, the buzzer among the
    // identity strings and the self-test alone on the Last test line at the very
    // bottom, so a pair of buttons never stacks into one cluster.
    //
    // Identity strings are read once at connect and never re-read, so a missing
    // one means the device does not report it — genuine absence, not a failed
    // poll. It is still shown, as a dash: see `push_identity` for why absence
    // reserves the row rather than removing it.
    rows.push(Row::Header(
        locale.t(strings::Key::PanelGroupDevice).to_owned(),
    ));
    if let Some(id) = data.identity {
        for (key, value) in [
            (strings::Key::PanelModel, &id.model),
            (strings::Key::PanelFirmware, &id.firmware),
            (strings::Key::PanelSerial, &id.serial),
        ] {
            push_identity(rows, locale, theme, key, value);
        }
    }

    // Both nameplate figures come from the device and from nowhere else.
    //
    // This used to fall back to a value in the INI, which was wrong in the way
    // that is hardest to notice: the file said 810 W, and it kept saying 810 W
    // after the user swapped in a different UPS. A device fact cached in a
    // user config outlives the device it describes and then quietly
    // contradicts it. If the device does not report the figure, a dash is the
    // honest answer.
    //
    // VA first, then watts, matching how the unit is labelled and sold — a
    // CP1350EPFCLCD is a 1350 VA / 810 W model, and printing it the other way
    // round reads as a different, much larger UPS.
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelNominalPower),
        match (r.nominal_va, r.nominal_power_w) {
            (Some(va), Some(w)) => Some(format!("{va} {volt_amp} / {w} {watt}")),
            (Some(va), None) => Some(format!("{va} {volt_amp}")),
            (None, Some(w)) => Some(format!("{w} {watt}")),
            (None, None) => None,
        },
        theme.colors.text_primary,
    ));

    if let Some(id) = data.identity {
        push_identity(
            rows,
            locale,
            theme,
            strings::Key::PanelManufacturer,
            &id.manufacturer,
        );
    }

    // The buzzer control, lifted up here so its button and the self-test button
    // do not sit close together. Absent on models with no AudibleAlarmControl.
    push_beeper_row(rows, locale, theme, data);

    // The rest of the identity block, after the buzzer.
    if let Some(id) = data.identity {
        push_identity(
            rows,
            locale,
            theme,
            strings::Key::PanelChemistry,
            &id.chemistry,
        );
    }

    // Last test, with the Self-test button, at the very bottom of the panel.
    push_self_test_row(rows, locale, theme, data, r);
}

/// A measurement row: the value in `color`, or an em dash in the secondary
/// colour when the field did not read.
///
/// The dash is why this is a function rather than a `Row::Pair` literal at each
/// call. An unread field keeps its row and says so, and the two halves of that
/// rule — the dash and the muted colour — must not drift apart; written out
/// eighteen times they could. It was a closure inside `build`, capturing
/// `theme`, which is what kept the device group from being lifted out of that
/// function at all.
fn measured(theme: &Theme, label: &str, value: Option<String>, color: Color) -> Row {
    Row::Pair {
        label: label.to_owned(),
        color: if value.is_some() {
            color
        } else {
            theme.colors.text_secondary
        },
        value: value.unwrap_or_else(|| EM_DASH.to_owned()),
    }
}

/// A row for a field that is absent rather than unread.
///
/// A reserved row with a dash for its value, drawn in the secondary colour.
/// The same shape `measured(theme, .., None, ..)` produces inside `build`,
/// extracted so the row helpers outside `build` can reserve rows the identical
/// way.
fn dash_row(label: &str, theme: &Theme) -> Row {
    measured(theme, label, None, theme.colors.text_secondary)
}

/// The input group: mains voltage and the transfer window.
///
/// One function per group, matching the headings the panel draws. The
/// order inside each is the order on screen, and keeping the two in one
/// place is the whole point of the split.
fn push_input_rows(rows: &mut Vec<Row>, locale: Locale, theme: &Theme, r: &Reading) {
    let volt = locale.t(strings::Key::UnitVolt);
    // Input. No frequency row: this device does not expose Frequency at all.
    rows.push(Row::Header(
        locale.t(strings::Key::PanelGroupInput).to_owned(),
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelInputVoltage),
        r.input_voltage.map(|v| format!("{v:.0} {volt}")),
        theme.colors.text_primary,
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelInputNominal),
        r.input_nominal_voltage.map(|v| format!("{v:.0} {volt}")),
        theme.colors.text_primary,
    ));
    // The transfer window, as one row rather than two. The pair is only ever
    // read together — the question is "what range does it tolerate", not what
    // either bound is on its own — and two rows would spend twice the height
    // on one fact. Both bounds must be present for the row to mean anything,
    // so a half-read renders as a dash like any other unknown.
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelTransferWindow),
        match (r.low_transfer_voltage, r.high_transfer_voltage) {
            (Some(lo), Some(hi)) => Some(format!("{lo:.0}\u{2013}{hi:.0} {volt}")),
            _ => None,
        },
        theme.colors.text_primary,
    ));
}

/// The output group: what the UPS is delivering.
fn push_output_rows(rows: &mut Vec<Row>, locale: Locale, theme: &Theme, r: &Reading) {
    let volt = locale.t(strings::Key::UnitVolt);
    let watt = locale.t(strings::Key::UnitWatt);
    let volt_amp = locale.t(strings::Key::UnitVoltAmp);
    let pct = locale.t(strings::Key::UnitPercent);
    // Output.
    rows.push(Row::Header(
        locale.t(strings::Key::PanelGroupOutput).to_owned(),
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelOutputVoltage),
        r.output_voltage.map(|v| format!("{v:.0} {volt}")),
        theme.colors.text_primary,
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelLoadPercent),
        r.load_percent.map(|p| format!("{p} {pct}")),
        r.load_percent
            .map_or(theme.colors.text_primary, |p| load_color(p, theme)),
    ));
    // ActivePower is reported directly in watts; no derivation from percent.
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelLoadWatts),
        r.load_watts.map(|w| format!("{w} {watt}")),
        theme.colors.text_primary,
    ));
    // Apparent power beside active power. The gap between the two is the
    // power factor of the connected load, which is why both are worth
    // showing: a figure in watts alone cannot tell a resistive load from a
    // reactive one drawing the same current.
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelLoadVa),
        r.load_va.map(|va| format!("{va} {volt_amp}")),
        theme.colors.text_primary,
    ));
}

/// The battery group: charge, runtime and the limits set on them.
fn push_battery_rows(
    rows: &mut Vec<Row>,
    locale: Locale,
    theme: &Theme,
    data: &PanelData,
    r: &Reading,
) {
    let volt = locale.t(strings::Key::UnitVolt);
    let pct = locale.t(strings::Key::UnitPercent);
    let min_u = locale.t(strings::Key::UnitMinutes);
    // Battery.
    rows.push(Row::Header(
        locale.t(strings::Key::PanelGroupBattery).to_owned(),
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelCharge),
        r.charge_percent.map(|c| format!("{c} {pct}")),
        r.charge_percent
            .map_or(theme.colors.text_primary, |c| charge_color(c, theme)),
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelBatteryVoltage),
        r.battery_voltage.map(|v| format!("{v:.1} {volt}")),
        theme.colors.text_primary,
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelBatteryNominal),
        r.battery_nominal_voltage.map(|v| format!("{v:.1} {volt}")),
        theme.colors.text_primary,
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelRuntime),
        r.runtime_seconds.map(|s| format_runtime(s, min_u)),
        theme.colors.text_primary,
    ));
    // Derived from flags, so it has no `Option` of its own — but "not read
    // yet" still has to be sayable, or the startup panel asserts `Idle` about
    // a battery it has never asked. The row is shown only once at least one of
    // the three battery flags has read; while all three are unknown it stays a
    // dash, like every other unread measurement. A confirmed-clear set of flags
    // (`Some(false)` throughout) is genuine Idle and is shown as such.
    let battery_flags_known =
        r.fully_charged.is_some() || r.charging.is_some() || r.discharging.is_some();
    let charge_state = (!data.probing && battery_flags_known).then(|| {
        if r.fully_charged == Some(true) {
            locale.t(strings::Key::StateFullyCharged)
        } else if r.charging == Some(true) {
            locale.t(strings::Key::StateCharging)
        } else if r.discharging == Some(true) {
            locale.t(strings::Key::StateDischarging)
        } else {
            locale.t(strings::Key::StateIdle)
        }
        .to_owned()
    });
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelChargeState),
        charge_state,
        theme.colors.text_primary,
    ));
    // Thresholds. These are what the device's own warning flags fire against,
    // so they turn "Low battery" from an opaque verdict into a number the
    // user can check the charge against.
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelCapacityLimit),
        r.capacity_limit_percent.map(|p| format!("{p} {pct}")),
        theme.colors.text_secondary,
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelWarningCapacity),
        r.warning_capacity_percent.map(|p| format!("{p} {pct}")),
        theme.colors.text_secondary,
    ));
    rows.push(measured(
        theme,
        locale.t(strings::Key::PanelRuntimeLimit),
        r.runtime_limit_seconds.map(|s| format_runtime(s, min_u)),
        theme.colors.text_secondary,
    ));
}

/// The fault flags that are set, one `Notice` row each.
///
/// Its own function because it is a rule applied to a table, not a
/// sequence of decisions: the table names every flag the device reports,
/// the rule is "only a confirmed-set flag earns a row", and both belong
/// together rather than in the middle of the panel's layout.
fn push_fault_rows(rows: &mut Vec<Row>, locale: Locale, theme: &Theme, r: &Reading) {
    // Flags appear only when set: a list of "no" rows is noise. This is also
    // how "Hardware status: Normal" is expressed — no fault row shown means
    // no fault reported, which is the same statement without a row spent on
    // saying nothing is wrong.
    //
    // Each flag is one line and must stay one line: they are `Notice`, not
    // `Wrapped`. A `Notice` is one line by contract, whatever the window width;
    // a `Wrapped` row is as tall as its text measures, so a flag whose
    // translation happened to exceed the width would silently take a second
    // line and, with several faults active at once, pull the Status block
    // apart. Every translation of every flag fits the panel's width on one line
    // — the metric labels above set the window wider than the longest flag —
    // and `every_notice_fits_one_line_in_every_language` checks that rather
    // than assuming it.
    for (active, key) in [
        r.below_capacity_limit,
        r.runtime_limit_expired,
        r.internal_failure,
        r.overload,
        r.voltage_out_of_range,
        r.frequency_out_of_range,
    ]
    .into_iter()
    .zip(FAULT_KEYS)
    {
        // Only a confirmed-set flag earns a row. `Some(false)` is the device
        // reporting no fault, `None` is a flag that did not read this cycle;
        // neither is a fault, and inventing a red row for an unread flag is the
        // phantom-notification bug in visual form.
        if active == Some(true) {
            rows.push(Row::Notice {
                text: locale.t(key).to_owned(),
                color: theme.colors.critical,
            });
        }
    }
    // Boost is not a fault and must not be coloured like one. The UPS
    // correcting low mains by transformer tap is it doing its job without
    // touching the battery; red would tell the user to act on something that
    // needs no action.
    if r.boost == Some(true) {
        rows.push(Row::Notice {
            text: locale.t(strings::Key::FlagBoost).to_owned(),
            color: theme.colors.warning,
        });
    }
}

/// Pushes one identity row: the value when the device reports it, a dash when
/// it does not. Shared so the identity strings keep identical behaviour
/// whether they sit before or after the buzzer.
///
/// The dash covers both reasons a value can be missing, and that is the point.
/// It used to cover only one: while probing the row was reserved, and on the
/// live panel an absent string dropped the row entirely. The skeleton is built
/// with an empty `Identity`, so it always reserved all five — and a device
/// that does not report, say, a chemistry string then produced a panel one row
/// *shorter* than the skeleton, and a window that shrank the moment it
/// connected. That is the same resize the skeleton exists to prevent, only in
/// the other direction.
///
/// So the rule is one rule for both states: the row is always there, and a
/// dash means "not reported". `probing` is no longer a parameter because it
/// cannot change the answer — the skeleton's identity is empty, so every value
/// it passes is already `None`.
fn push_identity(
    rows: &mut Vec<Row>,
    locale: Locale,
    theme: &Theme,
    key: strings::Key,
    value: &Option<String>,
) {
    match value {
        Some(v) => rows.push(Row::Pair {
            label: locale.t(key).to_owned(),
            value: v.clone(),
            color: theme.colors.text_primary,
        }),
        None => rows.push(dash_row(locale.t(key), theme)),
    }
}

/// Pushes the buzzer row: a dash when nothing has ever been observed, a plain
/// pair when the mode is one this code does not model, and a labelled button
/// otherwise. The button acts on the state on its own line, so its caption
/// names the action ("Enable"/"Disable").
///
/// **The button does not come and go with the reading.** [`BeeperView::Stale`]
/// carries the mode through an observation that failed, so the caption always
/// has something to say while the value has not. A value is a measurement:
/// nobody could take it, so it shows a dash, exactly like the mains voltage
/// beside it. A button is a control: one that disappears for a cycle takes the
/// pointer's target with it, and the click already on its way lands on
/// whatever slid into its place. So it stays, greyed, wearing the caption its
/// mode gives it — the same treatment a self-test already gives it, and for a
/// reason of the same kind: the action cannot be performed right now.
///
/// Greyed and not merely stale, because the caption is only half the question.
/// The click sends an intent, and the poll thread reads the mode afresh before
/// acting on it — but offering the control while the utility cannot see the
/// state would invite a press whose outcome nobody can predict from the
/// screen. The row says what it knows: no current value, and the action that
/// applied when there last was one.
///
/// [`BeeperView::Unknown`] is a different case and keeps its dash and its
/// missing button. It covers a model with no `AudibleAlarmControl` at all, and
/// the startup skeleton before anything has answered. There is no mode to put
/// on a caption, and inventing one is what this row used to do: the
/// skeleton guessed `Enabled` and offered "Disable" beside it, a control
/// acting on a state the utility had not read, whose click would write the
/// opposite of whatever the device held. That case used to drop the row
/// entirely, so a device without a buzzer produced a panel one row shorter
/// than the skeleton and a window that shrank on connect. Reserving it with a
/// dash puts the buzzer under the same rule as the identity strings: the row
/// is always there, and a dash means the device does not report it.
fn push_beeper_row(rows: &mut Vec<Row>, locale: Locale, theme: &Theme, data: &PanelData) {
    // No `probing` guard, unlike the self-test row below. That row judges
    // safety from a placeholder reading whose flags default to plausible
    // answers, so it has to be told the reading is not real. Nothing has been
    // observed while probing, so the view is `Unknown` and this row needs no
    // flag to say so — one that consulted it would be a second statement of
    // what the data already carries, free to drift from it.
    let (mode, current) = match data.beeper {
        BeeperView::Unknown => {
            rows.push(dash_row(locale.t(strings::Key::PanelBeeper), theme));
            return;
        }
        BeeperView::Current(mode) => (mode, true),
        BeeperView::Stale(mode) => (mode, false),
    };
    // The value reports an observation and nothing else, so a failed one is a
    // dash here even though the caption below still knows what to say.
    let value = if current {
        locale.t(mode.lang_key()).to_owned()
    } else {
        EM_DASH.to_owned()
    };
    if let Some(action) = mode.action_key() {
        rows.push(Row::LabeledButton {
            label: locale.t(strings::Key::PanelBeeper).to_owned(),
            value,
            color: theme.colors.text_primary,
            button: locale.t(action).to_owned(),
            id: HOT_BEEPER,
            // Two reasons to grey one control, both of them "the action cannot
            // be performed now".
            //
            // A stale mode is the first: the row is showing a dash, and a
            // control offered over a dash asks the user to act on something the
            // utility cannot see.
            //
            // A self-test running is the second, on the same flag as the row
            // below. The poll thread is inside the test for a dozen seconds
            // and does not take commands off its channel until it returns, so
            // a click during that window is not refused — it waits, and the
            // panel cannot show its result because the state it would toggle
            // from cannot move either. Six clicks during one test reached the
            // device as six identical writes the moment it ended. The button
            // is dead for that window whatever this flag says; the flag is
            // what makes it look dead.
            enabled: current && !data.self_test_running,
        });
    } else {
        // No action to offer — `Beeper::action_key` is the one place that
        // knows which modes have none, so the row shape follows its answer
        // instead of testing the variant a second time here.
        rows.push(Row::Pair {
            label: locale.t(strings::Key::PanelBeeper).to_owned(),
            value,
            color: theme.colors.text_primary,
        });
    }
}

/// Pushes the Last test row with the Self-test button.
///
/// The value shows this session's outcome once a test has run, the in-progress
/// caption while one runs, and otherwise the device's own read-only `Test`
/// result. The button is greyed while a test runs and whenever the pre-test
/// checks would refuse — a full battery on mains, not already discharging —
/// so a click that could only fail is not offered.
fn push_self_test_row(
    rows: &mut Vec<Row>,
    locale: Locale,
    theme: &Theme,
    data: &PanelData,
    r: &Reading,
) {
    // While probing there is no reading to judge safety from, so the row is a
    // plain reserved dash with no button, like the buzzer.
    //
    // This stands *before* the safety verdict rather than after it: the probing
    // state then cannot reach `safe_now` at all, so the verdict does not have to
    // carry `!data.probing` to defend itself against a caller it can never have.
    // Ordering is the guarantee; a duplicate flag in the conjunction would only
    // be a second statement of the same rule, free to drift from the first.
    if data.probing {
        rows.push(dash_row(locale.t(strings::Key::PanelLastTest), theme));
        return;
    }

    // The device's own register, and nothing else. It is the only thing that
    // knows about *every* test: the utility can start one, but so can the
    // button on the front of the UPS, and a line fed from the utility's record
    // of its own runs cannot see those at all. It used to be fed from that
    // record in preference to the register, which also froze it — after the
    // first test of a session the register was shadowed for good.
    //
    // `TestResult::InProgress` is among the codes, so a test running on the
    // device reads as one whoever asked for it.
    let value = r.test_result.map(|t| locale.t(t.lang_key()).to_owned());

    // A test is under way, from either of the two things that can know.
    //
    // The register is the truth and covers both kinds of test, but it lags: a
    // request this utility has just queued has not reached the device yet, and
    // for that window `self_test_running` is the only thing that knows. Two
    // sources for two moments, joined here rather than folded into one flag
    // that would have to mean both.
    let testing = data.self_test_running || r.test_result == Some(TestResult::InProgress);

    // The button is available only when the device is connected, no test is
    // running, and the pre-test conditions hold. The same conditions the
    // session re-checks before sending — checked here only to grey the button,
    // never to authorise the test, which the session gates on its own. Each
    // flag must be *confirmed* safe: unread mains or an unread discharge state
    // greys the button, matching the session, which refuses on unknown.
    let safe_now = data.presence == Presence::Open
        && r.ac_present == Some(true)
        && r.discharging == Some(false)
        && matches!(r.charge_percent, Some(c) if c >= MIN_CHARGE_PERCENT);
    let enabled = safe_now && !testing;

    rows.push(Row::LabeledButton {
        label: locale.t(strings::Key::PanelLastTest).to_owned(),
        value: value.unwrap_or_else(|| EM_DASH.to_owned()),
        color: theme.colors.text_primary,
        button: locale.t(strings::Key::PanelSelfTest).to_owned(),
        id: HOT_SELFTEST,
        enabled,
    });
}

/// The widest number any panel row can show, in digits.
///
/// Four covers every quantity the panel draws: mains and battery volts (three
/// digits), load percent (three), watts and volt-amps (four on the largest
/// unit CyberPower ships), and runtime in minutes (four is a week). The figure
/// is a bound on the *rendering*, not on the type — the device reports `u32`,
/// and a reading wider than this still widens the window, because the real
/// rows are measured alongside these samples rather than replaced by them.
const MAX_VALUE_DIGITS: usize = 4;

/// Rows the window measures but never draws: the widest value each row of the
/// panel can take.
///
/// Why they exist is in [`PanelContent::measure`](crate::ui::layout::PanelContent);
/// what they contain is the panel's business, because only this module knows
/// which strings each row can hold.
///
/// **Every candidate is emitted as its own row rather than reduced to a widest
/// one here.** Picking the widest would need a font — which of "Passed" and
/// "Aborted" is wider is a question about glyphs, not characters — and this
/// function has none. The measuring pass already takes a maximum over rows, so
/// handing it the candidates is both simpler and exact, digits included: no
/// assumption that the UI fonts set figures to a common width has to be made
/// or checked.
///
/// The labels are empty. Every label the panel draws is on a row that is
/// always present, so the label column is already measured from the real rows;
/// a sample repeating them would be a second copy to keep in step.
pub(crate) fn width_samples(locale: Locale, theme: &Theme) -> Vec<Row> {
    let mut rows = Vec::new();
    let text = theme.colors.text_primary;

    // Numbers. One row per digit per unit: the widest digit is a property of
    // the font, so all ten are offered and the scan settles it.
    let units = [
        strings::Key::UnitVolt,
        strings::Key::UnitWatt,
        strings::Key::UnitVoltAmp,
        strings::Key::UnitPercent,
        strings::Key::UnitMinutes,
    ];
    for unit in units {
        let unit = locale.t(unit);
        for digit in '0'..='9' {
            let n: String = std::iter::repeat_n(digit, MAX_VALUE_DIGITS).collect();
            rows.push(sample(&format!("{n} {unit}"), text));
            // The transfer window is the one row that shows two numbers, and
            // it is the widest thing in the value column that is not a word.
            rows.push(sample(&format!("{n}\u{2013}{n} {unit}"), text));
        }
    }

    // Words. Power state, charge state and the buzzer mode.
    for key in [
        strings::Key::StateOnline,
        strings::Key::StateOnBattery,
        strings::Key::StateDisconnected,
        strings::Key::StateCharging,
        strings::Key::StateDischarging,
        strings::Key::StateFullyCharged,
        strings::Key::StateIdle,
        strings::Key::BeeperUnsupported,
    ] {
        rows.push(sample(locale.t(key), text));
    }

    // The two rows that carry a button: their value and the button sit on one
    // line, so the pair has to be measured together.
    for key in [
        strings::Key::BeeperDisabled,
        strings::Key::BeeperEnabled,
        strings::Key::BeeperMuted,
        strings::Key::BeeperUnsupported,
    ] {
        for action in [strings::Key::BeeperEnable, strings::Key::BeeperDisable] {
            rows.push(sample_button(locale, theme, key, action, HOT_BEEPER));
        }
    }
    for key in [
        strings::Key::TestTestPassed,
        strings::Key::TestTestPassedWarning,
        strings::Key::TestTestError,
        strings::Key::TestTestAborted,
        strings::Key::TestTestInProgress,
        strings::Key::TestTestNotRun,
        strings::Key::TestTestScheduled,
        strings::Key::TestTestUnknown,
    ] {
        rows.push(sample_button(
            locale,
            theme,
            key,
            strings::Key::PanelSelfTest,
            HOT_SELFTEST,
        ));
    }

    // Fault lines. They are full-width rows that come and go with the device's
    // status flags, so the window would step outward the first time one
    // appeared.
    for key in FAULT_KEYS {
        rows.push(Row::Notice {
            text: locale.t(key).to_owned(),
            color: theme.colors.critical,
        });
    }

    rows
}

/// One value-column sample: a `Pair` with nothing in the label column.
fn sample(value: &str, color: Color) -> Row {
    Row::Pair {
        label: String::new(),
        value: value.to_owned(),
        color,
    }
}

/// One sample for a row whose value shares its line with a button.
fn sample_button(
    locale: Locale,
    theme: &Theme,
    value: strings::Key,
    action: strings::Key,
    id: crate::ui::row::HotspotId,
) -> Row {
    Row::LabeledButton {
        label: String::new(),
        value: locale.t(value).to_owned(),
        color: theme.colors.text_primary,
        button: locale.t(action).to_owned(),
        id,
        enabled: false,
    }
}

/// The panel's shape with every value still unknown.
///
/// Now trivial: `build` emits every measurement row unconditionally and
/// renders an unknown value as a dash, so "no data yet" and "this poll failed"
/// already look the same by construction. The skeleton is just the ordinary
/// panel over an empty reading.
///
/// This used to be an elaborate placeholder — every optional field forced to
/// `Some(0)` and then blanked afterwards — because rows appeared only when
/// their value did, so a missing field meant a missing row and a window that
/// grew when the first reading landed. Fixing the disappearing rows removed
/// the reason for all of it.
///
/// It reserves nothing by hand any more. Every row the live panel can show is
/// shown here too, because the live panel now reserves an absent identity
/// string and an absent buzzer with a dash rather than dropping them — so
/// matching the skeleton to it is a matter of passing an empty `Identity` and
/// letting `build` do the same thing it does when connected.
fn skeleton(locale: Locale, theme: &Theme) -> Vec<Row> {
    let placeholder = Reading::default();
    // `Some(&empty)` rather than `None`: `build` skips the identity block
    // entirely when there is no `Identity` at all, and the skeleton must show
    // that block. Its fields are all `None`, which `push_identity` reserves
    // with a dash — exactly what the live panel does for a string the device
    // does not report.
    let identity = Identity::default();

    let probe = PanelData {
        reading: Some(&placeholder),
        identity: Some(&identity),
        presence: Presence::Open,
        probing: true,
        // Nothing has been observed, and it is this `Unknown`, not the
        // `probing` flag above, that reserves the buzzer row with a dash. The
        // fabricated `Some(Beeper::Enabled)` it replaces existed only because
        // an absent buzzer used to drop the row.
        beeper: BeeperView::Unknown,
        // Empty, not `data.warnings`: the outer `build` already rendered them
        // before delegating here, and passing them through rendered them a
        // second time — a config problem announced twice on the skeleton and
        // once on the live panel, which also made the two shapes differ by two
        // rows, the very mismatch this function exists to prevent.
        warnings: Vec::new(),
        self_test_running: false,
    };

    // `build` has already pushed the title row before delegating here, and
    // this call pushes another. Dropping the inner copy is what keeps the two
    // shapes equal — a duplicated title made the skeleton exactly one row
    // taller than the panel it was supposed to match, which is the very
    // resize this function exists to prevent.
    let mut inner = build(locale, theme, &probe);
    inner.drain(..title_row(locale).len());
    inner
}

#[cfg(test)]
mod tests {
    use super::*;
    // The device, the nameplate and the two panel shapes come from
    // `testsupport`: `panel.rs` and `layout.rs` each used to carry a copy, and
    // copies of one fixture are places it can drift.
    use crate::hid::Beeper;
    use crate::testsupport::{live_identity, live_panel, live_reading, panel_data};
    use crate::ui::row::HotspotId;
    use crate::ui::Dpi;

    fn theme() -> Theme {
        Theme::default()
    }

    fn locale() -> Locale {
        Locale::english()
    }

    fn count_pairs(rows: &[Row]) -> usize {
        rows.iter()
            .filter(|r| matches!(r, Row::Pair { .. }))
            .count()
    }

    /// The window's width, and the button inside it, do not move when the
    /// reading does.
    ///
    /// Stated as the thing the user sees rather than as a fact about the
    /// samples: measure the panel in every state it can be put into and the
    /// answer is one number. Where it is not, the window grows and shrinks
    /// under the pointer — most visibly during a self-test, when the value
    /// column had to take "In progress…" and gave it back a dozen seconds
    /// later.
    ///
    /// The action-button width is asserted beside it because the same fault
    /// had a second home one level in, and the window being still is what hid
    /// it: the buzzer's caption is a verb that swaps with the mode it toggles,
    /// so a width measured over the rows being drawn was the width of the word
    /// currently on the button. The buzzer mode is therefore part of the
    /// cross-product below — `Unknown` too, which offers no button at all and
    /// must not be allowed to shrink the one on the row beneath.
    ///
    /// A cross-product, because the rows that vary do so independently: a
    /// fault appears, a test runs, the mains come and go, a reading gains a
    /// digit. `StubMetrics` stands in for GDI — the claim is that a set of
    /// measurements agree, which holds under any consistent set of widths.
    ///
    /// Every shipped language, and that is not thoroughness for its own sake.
    /// Which column is the wider one depends on the language: where the labels
    /// are long the value column never binds, and a fixture in one language
    /// would pass while proving nothing about the twenty-three where it does.
    ///
    /// Identity strings are deliberately not varied. A serial number is not
    /// enumerable, the samples do not pretend to cover it, and the real rows
    /// are measured alongside them so that it still fits.
    #[test]
    fn no_reading_changes_the_width_of_the_panel_or_its_button() {
        use crate::ui::layout::content_geometry;
        let t = theme();
        let scaled = t.scaled(Dpi::new(96));
        let identity = live_identity();
        // A wider glyph than the default stub's, and the reason is the button
        // rather than the window. `button_min_width` is 96 and the padding
        // around a caption is 32, so a caption has to exceed 64 pixels before
        // it decides anything; at the default 7 pixels per character every
        // shipped buzzer verb is flattened to that floor, and a button width
        // measured from the wrong rows would come out equal anyway. That is a
        // property of the stub, not of the real font — the fault reported was
        // a buzzer button visibly changing size — so the stub is given a glyph
        // wide enough for the captions to speak. The panel-width claim is
        // unaffected: it holds under any consistent set of widths, which is
        // what this whole fixture rests on.
        let mut m = crate::testsupport::StubMetrics {
            char_width: 14,
            ..crate::testsupport::StubMetrics::default()
        };

        for language in &strings::LANGUAGES {
            let l = Locale::by_code(language.code)
                .or_log(|| panic!("every shipped language must resolve by its own code"));
            let samples = width_samples(l, &t);
            let mut seen: Vec<(i32, i32, String)> = Vec::new();
            // `None` among them: a mode that could not be read this cycle is
            // one of the states the panel has to hold its size in, and it is
            // the one that used to remove the control altogether.
            for beeper in [
                Some(crate::hid::Beeper::Enabled),
                Some(crate::hid::Beeper::Disabled),
                Some(crate::hid::Beeper::Muted),
                Some(crate::hid::Beeper::Unknown(0)),
                None,
            ] {
                for running in [false, true] {
                    // The register's own codes, which is what the row now
                    // shows. `InProgress` among them: a test running on the
                    // device is one of the states the panel must hold its size
                    // in, and it is no longer reachable only through the flag.
                    for outcome in [
                        None,
                        Some(TestResult::PassedWithWarning),
                        Some(TestResult::InProgress),
                    ] {
                        for fault in [false, true] {
                            for ac in [None, Some(true), Some(false)] {
                                for volts in [0.0_f32, 9999.0] {
                                    let mut reading = live_reading();
                                    reading.ac_present = ac;
                                    reading.discharging = ac.map(|on| !on);
                                    reading.overload = Some(fault);
                                    reading.internal_failure = Some(fault);
                                    reading.input_voltage = Some(volts);
                                    reading.output_voltage = Some(volts);
                                    reading.load_watts = Some(9999);
                                    reading.beeper = beeper;
                                    reading.test_result = outcome;
                                    let mut data = live_panel(&reading, &identity);
                                    data.self_test_running = running;

                                    let rows = build(l, &t, &data);
                                    let (width, columns) =
                                        content_geometry(&rows, &samples, &scaled, &mut m);
                                    seen.push((
                                        width,
                                        columns.action_button,
                                        format!(
                                            "beeper={beeper:?} running={running} \
                                     outcome={outcome:?} fault={fault} ac={ac:?} volts={volts}"
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            let mut measured = seen.iter();
            let (first, first_btn, how) = measured
                .next()
                .expect("the cross-product above measures at least one panel");
            for (width, button, state) in measured {
                assert_eq!(
                    width, first,
                    "in {}, the panel is {width} wide with {state} and {first} \
                     wide with {how}",
                    language.code
                );
                assert_eq!(
                    button, first_btn,
                    "in {}, the action button is {button} wide with {state} and \
                     {first_btn} wide with {how}",
                    language.code
                );
            }
        }
    }

    /// Settings must be the first thing in the panel, on the title line, not
    /// a button trailing the last reading.
    #[test]
    fn settings_sits_on_the_title_row_at_the_top() {
        let (t, l) = (theme(), locale());
        let reading = live_reading();
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        assert!(
            matches!(
                rows.first(),
                Some(Row::TitleButton { id, .. }) if *id == crate::ui::row::HOT_SETTINGS
            ),
            "the first row must be the title carrying Settings, got {:?}",
            rows.first().map(std::mem::discriminant)
        );
        // And nowhere else, or two Settings controls answer the same click.
        let count = rows
            .iter()
            .filter(|r| match r {
                Row::TitleButton { id, .. } | Row::LabeledButton { id, .. } => {
                    *id == crate::ui::row::HOT_SETTINGS
                }
                _ => false,
            })
            .count();
        assert_eq!(count, 1, "Settings must appear exactly once");
    }

    /// The buzzer control shares a line with the state it acts on, and its
    /// caption is a verb rather than a repeat of that state.
    #[test]
    fn buzzer_button_is_inline_and_names_an_action() {
        let (t, l) = (theme(), locale());
        let reading = live_reading();
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        let row = rows
            .iter()
            .find_map(|r| match r {
                Row::LabeledButton {
                    value, button, id, ..
                } if *id == HOT_BEEPER => Some((value.clone(), button.clone())),
                _ => None,
            })
            .expect("buzzer must be a labelled row with an inline button");

        let (state, action) = row;
        assert_eq!(state, "Enabled", "the value shows the current state");
        assert_eq!(action, "Disable", "the button shows what clicking it does");
        assert_ne!(
            state, action,
            "a button captioned with the state reads as a second status field"
        );
    }

    /// True if any row carries a clickable control with this id, whichever
    /// row type hosts it. Matching only `Row::Button` would have quietly gone
    /// false when the buzzer and Settings controls moved onto shared lines,
    /// reporting the buttons as missing when they had only changed shape.
    fn has_button(rows: &[Row], id: HotspotId) -> bool {
        rows.iter().any(|r| match r {
            Row::LabeledButton { id: i, .. } | Row::TitleButton { id: i, .. } => *i == id,
            Row::Buttons {
                left_id, right_id, ..
            } => *left_id == id || *right_id == id,
            _ => false,
        })
    }

    /// Index of the row carrying a control with this id, if any.
    fn button_index(rows: &[Row], id: HotspotId) -> Option<usize> {
        rows.iter().position(|r| {
            matches!(r, Row::LabeledButton { id: i, .. } | Row::TitleButton { id: i, .. } if *i == id)
        })
    }

    /// The self-test control is a labelled button on the Last test line, at the
    /// very bottom of the panel, and its caption is the settled English term.
    #[test]
    fn self_test_button_is_on_the_last_test_line_at_the_bottom() {
        let (t, l) = (theme(), locale());
        let reading = live_reading();
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);

        let row = rows
            .iter()
            .find_map(|r| match r {
                Row::LabeledButton {
                    label, button, id, ..
                } if *id == HOT_SELFTEST => Some((label.clone(), button.clone())),
                _ => None,
            })
            .expect("self-test must be a labelled row with an inline button");
        assert_eq!(row.0, "Last test", "the label is the Last test line");
        assert_eq!(row.1, "Self-test", "the button keeps the settled term");

        // The last labelled/titled button in the panel is the self-test, so it
        // sits below every other control including the buzzer.
        let last_control = rows
            .iter()
            .rposition(|r| matches!(r, Row::LabeledButton { .. } | Row::TitleButton { .. }));
        assert_eq!(
            last_control,
            button_index(&rows, HOT_SELFTEST),
            "the self-test button is the last control in the panel"
        );
    }

    /// The buzzer and self-test buttons must not sit on adjacent lines: two
    /// buttons in a row read as one cluster. The buzzer was lifted up among the
    /// identity strings for exactly this reason.
    #[test]
    fn the_two_buttons_are_not_adjacent() {
        let (t, l) = (theme(), locale());
        let reading = live_reading();
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        let beeper = button_index(&rows, HOT_BEEPER).expect("buzzer present");
        let selftest = button_index(&rows, HOT_SELFTEST).expect("self-test present");
        assert!(
            selftest > beeper + 1,
            "buzzer at {beeper} and self-test at {selftest} must have rows between them"
        );
    }

    /// A test greys the button and shows progress, from either of the two
    /// things that can know one is running.
    ///
    /// The register is the truth and is the only source that sees a test
    /// started from the front panel of the UPS. The flag covers the window the
    /// register cannot: between this utility queueing a request and the device
    /// reporting it. Both are asserted, because a row that consulted only the
    /// flag was blind to the front panel — the case that prompted this — and
    /// one that consulted only the register would leave the button live for a
    /// second after a click, inviting a second one.
    ///
    /// The front-panel case is built with `discharging: Some(false)`
    /// deliberately. The button did grey during such a test before this
    /// change, but by accident: a test puts the load on battery, and the
    /// pre-test check refuses while discharging. That is an invariant held by
    /// a side effect, with a window at the start of the test before the
    /// transfer shows up in the report. Pinning it here with the load still on
    /// mains is what tells the two apart.
    #[test]
    fn a_running_test_greys_the_button_and_shows_progress() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();

        let row_of = |data: &PanelData| {
            build(l, &t, data)
                .into_iter()
                .find_map(|r| match r {
                    Row::LabeledButton {
                        value, id, enabled, ..
                    } if id == HOT_SELFTEST => Some((value, enabled)),
                    _ => None,
                })
                .expect("self-test row present")
        };

        // Ours, queued but not yet visible to the device.
        let reading = live_reading();
        let (_, enabled) = row_of(&PanelData {
            self_test_running: true,
            ..live_panel(&reading, &identity)
        });
        assert!(
            !enabled,
            "a request already queued must not be offered twice"
        );

        // The device's own, with the load still reported on mains.
        let elsewhere = Reading {
            test_result: Some(TestResult::InProgress),
            discharging: Some(false),
            ..live_reading()
        };
        let (value, enabled) = row_of(&live_panel(&elsewhere, &identity));
        assert!(
            !enabled,
            "a test running on the device must grey the button, whoever started it"
        );
        assert_eq!(
            value,
            l.t(strings::Key::TestTestInProgress),
            "the value follows the device's register"
        );
    }

    /// The button is greyed when the pre-test conditions do not hold — here a
    /// battery below the threshold — so a click that could only be refused is
    /// not offered.
    #[test]
    fn an_unsafe_state_greys_the_self_test_button() {
        let (t, l) = (theme(), locale());
        let mut reading = live_reading();
        reading.charge_percent = Some(50); // below the 90% floor
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        let enabled = rows
            .iter()
            .find_map(|r| match r {
                Row::LabeledButton { id, enabled, .. } if *id == HOT_SELFTEST => Some(*enabled),
                _ => None,
            })
            .expect("self-test row present");
        assert!(!enabled, "a low battery greys the button");
    }

    /// A running test greys the buzzer button too.
    ///
    /// Not tidiness. The poll thread is inside the test for a dozen seconds
    /// and takes nothing off its command channel until it returns, so a click
    /// here is queued rather than refused, and the state it toggles from
    /// cannot move while it waits. A real session produced six clicks during
    /// one test and six identical writes to the device the instant it ended.
    /// The button is inert for that window whatever this flag says; the flag
    /// is what stops it looking live.
    #[test]
    fn a_running_test_greys_the_buzzer_button() {
        let (t, l) = (theme(), locale());
        let reading = live_reading();
        let identity = live_identity();
        let live = live_panel(&reading, &identity);
        let enabled_of = |data: &PanelData| {
            build(l, &t, data)
                .iter()
                .find_map(|r| match r {
                    Row::LabeledButton { id, enabled, .. } if *id == HOT_BEEPER => Some(*enabled),
                    _ => None,
                })
                .expect("buzzer row present")
        };

        assert!(
            enabled_of(&live),
            "the buzzer is clickable when no test is running"
        );
        assert!(
            !enabled_of(&PanelData {
                self_test_running: true,
                ..live
            }),
            "a click made here would only be queued until the test ends"
        );
    }

    #[test]
    fn disconnected_shows_hint_and_no_measurements() {
        let data = PanelData {
            presence: Presence::Absent,
            ..panel_data()
        };
        let rows = build(locale(), &theme(), &data);
        assert_eq!(count_pairs(&rows), 0, "no measurement rows when offline");
        assert!(rows.iter().any(|r| matches!(r, Row::Wrapped { .. })));
    }

    #[test]
    fn absent_fields_are_omitted_not_zeroed() {
        // Every optional field empty: the panel must not invent zeros for
        // readings this device never reports.
        let reading = Reading::default();
        let data = PanelData {
            reading: Some(&reading),
            presence: Presence::Open,
            ..panel_data()
        };
        let rows = build(locale(), &theme(), &data);
        let values: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Pair { value, .. } => Some(value.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !values.iter().any(|v| v.starts_with("0 V")),
            "absent voltages must not render as zero: {values:?}"
        );
    }

    /// An unreadable field keeps its row and shows a dash.
    ///
    /// This is the whole requirement, stated three ways, because each of the
    /// three has been implemented wrongly at some point: the row must not
    /// disappear, the value must not be a stale number, and the dash must be
    /// the same glyph the startup skeleton uses.
    #[test]
    fn an_unreadable_field_shows_a_dash_and_keeps_its_row() {
        let full = live_reading();
        // The same reading with PercentLoad and ActivePower unreadable, as a
        // single failed HidD_GetFeature leaves them.
        let partial = Reading {
            load_percent: None,
            load_watts: None,
            ..live_reading()
        };
        let identity = live_identity();
        let mk = |r: &Reading| {
            let data = live_panel(r, &identity);
            build(locale(), &theme(), &data)
        };

        let rows_full = mk(&full);
        let rows_partial = mk(&partial);

        // 1. The row count is unchanged: nothing vanished, nothing shifted up.
        assert_eq!(
            rows_full.len(),
            rows_partial.len(),
            "a failed read must not remove the row"
        );

        // 2. The value is a dash, not the number it had a moment ago.
        let load = rows_partial
            .iter()
            .find_map(|r| match r {
                Row::Pair { label, value, .. } if label == "Load" => Some(value.as_str()),
                _ => None,
            })
            .expect("the Load row must still be present");
        assert_eq!(load, EM_DASH, "an unknown value must read as a dash");
        assert!(
            !load.contains("11"),
            "a stale value presented as current is worse than no value"
        );

        // 3. The same glyph the startup skeleton uses, so "not yet" and
        //    "could not read" are indistinguishable to the reader — which is
        //    correct, because both mean the utility has no value.
        let connecting = PanelData {
            beeper: BeeperView::Current(Beeper::Enabled),
            ..panel_data()
        };
        let skeleton_rows = build(locale(), &theme(), &connecting);
        let skeleton_load = skeleton_rows
            .iter()
            .find_map(|r| match r {
                Row::Pair { label, value, .. } if label == "Load" => Some(value.as_str()),
                _ => None,
            })
            .expect("the skeleton must have a Load row too");
        assert_eq!(skeleton_load, load);
    }

    /// Every newly added flag must actually reach the panel when raised.
    ///
    /// These are all zero on a healthy unit, so nothing in normal operation
    /// would reveal a flag that was read but never rendered.
    #[test]
    fn every_status_flag_reaches_the_panel_when_raised() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let reading = Reading {
            below_capacity_limit: Some(true),
            runtime_limit_expired: Some(true),
            internal_failure: Some(true),
            overload: Some(true),
            voltage_out_of_range: Some(true),
            frequency_out_of_range: Some(true),
            boost: Some(true),
            ..live_reading()
        };
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        let texts: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        for key in [
            strings::Key::FlagLowBattery,
            strings::Key::FlagRuntimeLimitExpired,
            strings::Key::FlagInternalFailure,
            strings::Key::FlagOverload,
            strings::Key::FlagVoltageOutOfRange,
            strings::Key::FlagFrequencyOutOfRange,
            strings::Key::FlagBoost,
        ] {
            assert!(
                texts.contains(&l.t(key)),
                "{} was read from the device but never rendered",
                key.label()
            );
        }
    }

    /// Status flags are single-line `Notice` rows, never `Wrapped`.
    ///
    /// A `Wrapped` row is floored at two lines by the width estimate, so a
    /// one-line flag rendered that way would reserve a blank line beneath it,
    /// and several active faults would stack those blanks and pull the Status
    /// block apart. Every flag translation fits the panel width on one line, so
    /// `Notice` clips nothing. This pins the choice: a flag pushed as `Wrapped`
    /// again would bring the phantom second line back.
    #[test]
    fn status_flags_are_single_line_notices() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let reading = Reading {
            below_capacity_limit: Some(true),
            runtime_limit_expired: Some(true),
            internal_failure: Some(true),
            overload: Some(true),
            voltage_out_of_range: Some(true),
            frequency_out_of_range: Some(true),
            boost: Some(true),
            ..live_reading()
        };
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        for key in [
            strings::Key::FlagLowBattery,
            strings::Key::FlagRuntimeLimitExpired,
            strings::Key::FlagInternalFailure,
            strings::Key::FlagOverload,
            strings::Key::FlagVoltageOutOfRange,
            strings::Key::FlagFrequencyOutOfRange,
            strings::Key::FlagBoost,
        ] {
            let text = l.t(key);
            assert!(
                rows.iter()
                    .any(|r| matches!(r, Row::Notice { text: t, .. } if t == text)),
                "{} must render as a single-line Notice",
                key.label()
            );
            assert!(
                !rows
                    .iter()
                    .any(|r| matches!(r, Row::Wrapped { text: t, .. } if t == text)),
                "{} must not be a Wrapped row",
                key.label()
            );
        }
    }

    /// Boost is the UPS working, not failing, and must not be dressed as a
    /// fault. Red here would tell the user to act on something needing no
    /// action.
    #[test]
    fn boost_is_not_coloured_as_a_fault() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let reading = Reading {
            boost: Some(true),
            ..live_reading()
        };
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        let color = rows
            .iter()
            .find_map(|r| match r {
                Row::Notice { text, color } if text == l.t(strings::Key::FlagBoost) => Some(*color),
                _ => None,
            })
            .expect("the AVR row must be present when boost is active");
        assert_ne!(color, t.colors.critical, "AVR activity is not a fault");
        assert_eq!(color, t.colors.warning);
    }

    /// A healthy device shows no flag rows at all. This is how "Hardware
    /// status: Normal" is expressed: the absence of fault rows, rather than a
    /// row spent stating that nothing is wrong.
    #[test]
    fn a_healthy_device_shows_no_flag_rows() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let reading = live_reading();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        for key in [
            strings::Key::FlagLowBattery,
            strings::Key::FlagRuntimeLimitExpired,
            strings::Key::FlagInternalFailure,
            strings::Key::FlagOverload,
            strings::Key::FlagVoltageOutOfRange,
            strings::Key::FlagFrequencyOutOfRange,
            strings::Key::FlagBoost,
        ] {
            assert!(
                !rows.iter().any(|r| matches!(
                    r, Row::Notice { text, .. } if text == l.t(key)
                )),
                "{} must stay hidden while the device is healthy",
                key.label()
            );
        }
    }

    /// The nameplate must read `1350 VA / 810 W`, never the reverse.
    ///
    /// Two things can flip this and both have: the firmware reports the two
    /// Config*Power fields the wrong way round, so the code deliberately
    /// crosses them, and a crossed mapping is easy to "correct" back into
    /// being wrong. The failure is silent and plausible — 1350 W / 810 VA is
    /// a coherent-looking spec for a much larger UPS than this one.
    #[test]
    fn nameplate_reads_va_then_watts() {
        let (t, l) = (theme(), locale());
        let reading = live_reading();
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(l, &t, &data);
        let value = rows
            .iter()
            .find_map(|r| match r {
                Row::Pair { label, value, .. } if label == l.t(strings::Key::PanelNominalPower) => {
                    Some(value.as_str())
                }
                _ => None,
            })
            .expect("the nameplate row must be present");

        assert_eq!(
            value, "1350 VA / 810 W",
            "the CP1350EPFCLCD is a 1350 VA / 810 W unit"
        );
        // Stated separately, because the assertion above could be satisfied
        // by a locale change while the numbers stayed crossed.
        let va_at = value.find("1350").expect("VA figure missing");
        let w_at = value.find("810").expect("watt figure missing");
        assert!(va_at < w_at, "VA is the headline figure and comes first");
        assert!(
            !value.contains("1350 W") && !value.contains("810 VA"),
            "the two figures are crossed: {value}"
        );
    }

    /// A connected device with no reading yet is the skeleton, never a
    /// placeholder line.
    ///
    /// This is the state between `Connected` and the first `Update`: the
    /// identity is known, nothing that comes from a poll is. It must draw the
    /// full-size layout with dashes, because anything shorter resizes the
    /// window a poll later. A `state.reading` placeholder branch used to sit
    /// below the skeleton guard where nothing could reach it.
    #[test]
    fn a_connected_device_awaiting_its_first_reading_draws_the_skeleton() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let data = PanelData {
            identity: Some(&identity),
            presence: Presence::Open,
            beeper: BeeperView::Current(Beeper::Enabled),
            ..panel_data()
        };
        let rows = build(l, &t, &data);

        // Full layout, not a stub: the group headings are present.
        for key in [
            strings::Key::PanelGroupInput,
            strings::Key::PanelGroupOutput,
            strings::Key::PanelGroupBattery,
        ] {
            assert!(
                rows.iter()
                    .any(|r| matches!(r, Row::Header(h) if h == l.t(key))),
                "{} must be laid out before the first reading arrives",
                key.label()
            );
        }
        // A raw key can no longer reach the panel: `Row` text is built only
        // through `Locale::t`, which takes a `Key` and returns real text, so
        // there is no `&str` path by which an unlocalized key could leak. What
        // was a runtime check here is now enforced by the type system.
    }

    #[test]
    fn load_thresholds_escalate() {
        let t = theme();
        assert_eq!(load_color(50, &t), t.colors.text_primary);
        assert_eq!(load_color(80, &t), t.colors.warning);
        assert_eq!(load_color(95, &t), t.colors.critical);
        // Boundaries belong to the more severe band.
        assert_eq!(load_color(75, &t), t.colors.warning);
        assert_eq!(load_color(90, &t), t.colors.critical);
    }

    #[test]
    fn charge_thresholds_escalate_downward() {
        let t = theme();
        assert_eq!(charge_color(100, &t), t.colors.ok);
        assert_eq!(charge_color(40, &t), t.colors.warning);
        assert_eq!(charge_color(10, &t), t.colors.critical);
        assert_eq!(charge_color(50, &t), t.colors.warning);
        assert_eq!(charge_color(20, &t), t.colors.critical);
    }

    /// The startup skeleton must measure exactly as tall as the populated
    /// panel, or the window still resizes when data arrives — which is the
    /// flicker the skeleton exists to remove.
    #[test]
    fn skeleton_matches_the_populated_height() {
        use crate::testsupport::StubMetrics;
        use crate::ui::layout::measure_with;
        let t = theme();
        let l = locale();

        // A reading with every field this device actually reports present.
        let reading = live_reading();
        let identity = live_identity();

        let connecting = PanelData {
            beeper: BeeperView::Current(Beeper::Enabled),
            ..panel_data()
        };
        let live = live_panel(&reading, &identity);

        let scaled = t.scaled(Dpi::new(96));
        let h_skeleton = measure_with(
            &build(l, &t, &connecting),
            &scaled,
            460,
            &mut StubMetrics::default(),
        );
        let h_live = measure_with(
            &build(l, &t, &live),
            &scaled,
            460,
            &mut StubMetrics::default(),
        );
        assert_eq!(
            h_skeleton, h_live,
            "startup skeleton must not resize when the first reading lands"
        );
    }

    /// `Connected` arrives before the first reading. That window must show
    /// the skeleton too, not the short "reading" view, or the panel visibly
    /// shrinks and grows again between connect and first poll.
    #[test]
    fn connected_without_a_reading_uses_the_skeleton() {
        use crate::testsupport::StubMetrics;
        use crate::ui::layout::measure_with;
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let just_connected = PanelData {
            identity: Some(&identity),
            presence: Presence::Open,
            beeper: BeeperView::Current(Beeper::Enabled),
            ..panel_data()
        };
        let rows = build(l, &t, &just_connected);
        assert!(
            has_button(&rows, crate::ui::row::HOT_SETTINGS),
            "the settings control must be present while connecting"
        );
        // Same height as the fully populated panel. The same fixture the
        // sibling test uses: these two readings used to be written out
        // separately here and differed by `charging`, with nothing saying
        // whether that mattered — which is exactly the question a duplicated
        // fixture leaves unanswerable.
        let reading = live_reading();
        let live = PanelData {
            reading: Some(&reading),
            ..just_connected.clone()
        };
        let scaled = t.scaled(Dpi::new(96));
        assert_eq!(
            measure_with(
                &build(l, &t, &just_connected),
                &scaled,
                460,
                &mut StubMetrics::default()
            ),
            measure_with(
                &build(l, &t, &live),
                &scaled,
                460,
                &mut StubMetrics::default()
            )
        );
    }

    /// Nothing on the startup panel states a value the device has not sent.
    ///
    /// The skeleton is built over `Reading::default()`, and three rows are
    /// derived from booleans rather than from `Option`s. A `bool` has no
    /// "unread" value, so those rows resolved a default `false` into
    /// confident prose: `Idle` for the charge state, `On battery` for the
    /// power state, `Disabled` plus an `Enable` button for the buzzer — three
    /// assertions about a device that had answered nothing, sitting in a
    /// column of dashes where they were indistinguishable from measurements.
    /// The identity rows were worse in a quieter way: reserved with empty
    /// strings, they drew as blank space, which reads as a field the device
    /// answered with nothing rather than one still being waited on.
    ///
    /// Every value cell on the skeleton must therefore be the dash, and no
    /// button may be offered against a state that has not been read.
    #[test]
    fn the_startup_panel_states_nothing_it_has_not_read() {
        let data = panel_data();
        let rows = build(locale(), &theme(), &data);

        for row in &rows {
            match row {
                Row::Pair { label, value, .. } => assert_eq!(
                    value, EM_DASH,
                    "{label:?} states {value:?} before the device has answered"
                ),
                Row::LabeledButton { label, .. } => {
                    panic!("{label:?} offers a control against a state that has not been read")
                }
                _ => {}
            }
        }

        // And the rows are genuinely there — a panel that asserted nothing
        // because it drew nothing would pass the loop above and fail the user.
        assert!(
            count_pairs(&rows) > 20,
            "the skeleton must still reserve every row, got {}",
            count_pairs(&rows)
        );
    }

    /// The window must not shrink when the device connects.
    ///
    /// The skeleton exists so the panel opens at its final size, and the
    /// reasoning behind it was one-sided: it made sure the live panel could
    /// never be *taller*. It could be shorter. The skeleton reserves all five
    /// identity strings and a buzzer row, while the live panel used to draw an
    /// identity string only when the device reported it and a buzzer row only
    /// when the model had `AudibleAlarmControl` — so a device missing either
    /// produced a panel with fewer rows than the skeleton it replaced, and the
    /// window snapped smaller a moment after opening.
    ///
    /// The fixture is that device: connected, answering with measurements, and
    /// reporting no identity strings and no buzzer at all.
    #[test]
    fn a_device_that_reports_no_strings_does_not_shrink_the_panel() {
        let reading = live_reading();
        let empty = Identity::default();
        let live = PanelData {
            beeper: BeeperView::Unknown,
            ..live_panel(&reading, &empty)
        };
        let startup = panel_data();

        let before = build(locale(), &theme(), &startup);
        let after = build(locale(), &theme(), &live);
        assert_eq!(
            before.len(),
            after.len(),
            "the skeleton has {} rows and the connected panel {}; the window \
             would resize on connect",
            before.len(),
            after.len()
        );
    }

    /// A mains flag that failed to read is not a mains failure.
    ///
    /// `ac_present_known` distinguishes the two, and the notifier has always
    /// consulted it; the panel did not, so a dropped read of report 11 drew
    /// "On battery" in warning colour on a UPS sitting comfortably on mains.
    #[test]
    fn an_unread_mains_flag_is_not_drawn_as_an_outage() {
        let reading = Reading {
            ac_present: None,
            ..live_reading()
        };
        let identity = live_identity();
        let data = live_panel(&reading, &identity);
        let rows = build(locale(), &theme(), &data);
        let state = rows
            .iter()
            .find_map(|r| match r {
                Row::Pair { label, value, .. } if label == locale().t(strings::Key::PanelState) => {
                    Some(value.clone())
                }
                _ => None,
            })
            .expect("the status row must exist");
        assert_eq!(
            state, EM_DASH,
            "an unread AC flag must show a dash, not an outage"
        );
    }

    /// The skeleton carries the Settings button, so the control is usable
    /// before the device has answered.
    #[test]
    fn skeleton_has_the_settings_button() {
        let data = panel_data();
        let rows = build(locale(), &theme(), &data);
        assert!(has_button(&rows, crate::ui::row::HOT_SETTINGS));
    }

    #[test]
    fn warning_is_rendered_when_present() {
        let t = Theme::default();
        let locale = Locale::english();
        let mut data = PanelData {
            presence: Presence::Absent,
            ..panel_data()
        };

        let without = build(locale, &t, &data);
        data.warnings = vec!["cannot write config".to_owned()];
        let with = build(locale, &t, &data);

        assert!(
            with.len() > without.len(),
            "a warning must add rows, not replace them"
        );
        assert!(
            with.iter().any(|r| matches!(
                r,
                Row::Wrapped { text, .. } if text == "cannot write config"
            )),
            "the warning text must actually reach the panel"
        );
    }

    /// A warning appears exactly once, including on the skeleton. The
    /// skeleton is built by re-invoking `build` on a probe, and passing the
    /// warning into that probe rendered it twice: once by the outer call,
    /// once by the inner — two identical red rows for one config problem,
    /// and a skeleton two rows taller than the live panel.
    #[test]
    fn warning_appears_exactly_once_on_the_skeleton() {
        let (t, l) = (theme(), locale());
        let data = PanelData {
            warnings: vec!["cannot write config".to_owned()],
            ..panel_data()
        };
        let rows = build(l, &t, &data);
        let hits = rows
            .iter()
            .filter(|r| matches!(r, Row::Wrapped { text, .. } if text == "cannot write config"))
            .count();
        assert_eq!(hits, 1, "the skeleton probe must not duplicate the warning");
    }

    #[test]
    fn beeper_button_hidden_when_the_mode_is_one_this_build_cannot_toggle() {
        let reading = Reading {
            beeper: Some(Beeper::Unknown(7)),
            ..Reading::default()
        };
        let data = PanelData {
            reading: Some(&reading),
            presence: Presence::Open,
            beeper: BeeperView::Current(Beeper::Unknown(7)),
            ..panel_data()
        };
        let rows = build(locale(), &theme(), &data);
        assert!(
            !has_button(&rows, HOT_BEEPER),
            "a mode with no wire value for its opposite has no action to offer"
        );

        let known = Reading {
            beeper: Some(Beeper::Enabled),
            ..Reading::default()
        };
        let rows = build(
            locale(),
            &theme(),
            &PanelData {
                reading: Some(&known),
                beeper: BeeperView::Current(Beeper::Enabled),
                ..data
            },
        );
        assert!(has_button(&rows, HOT_BEEPER));
    }

    /// An unread mode empties the value and greys the button. It does not take
    /// the button away.
    ///
    /// This is the defect that prompted the change, seen from the panel. A
    /// buzzer write whose confirming read was refused — one transfer, a thing
    /// this device does several times a minute — left the row with a dash and
    /// no control, and the only thing that would have re-read the mode was
    /// that control. The mode is polled now, so the gap lasts one cycle; and a
    /// control that vanishes even for one cycle takes the pointer's target
    /// with it, so it stays put and says it cannot be used.
    ///
    /// The caption is asserted to be the one the carried mode gives, not merely
    /// "some caption": a greyed button wearing the wrong verb is a worse answer
    /// than a missing one, because it reads as information.
    #[test]
    fn an_unread_mode_greys_the_button_and_keeps_its_caption() {
        let (t, l) = (theme(), locale());
        let identity = live_identity();
        let seen = Reading {
            beeper: Some(Beeper::Disabled),
            ..live_reading()
        };
        let unread = Reading {
            beeper: None,
            ..seen
        };

        let live = live_panel(&seen, &identity);
        let lost = PanelData {
            reading: Some(&unread),
            // What `App` holds after an observation that came back with
            // nothing: the view demotes and keeps the mode.
            beeper: BeeperView::Stale(Beeper::Disabled),
            ..live_panel(&seen, &identity)
        };

        let row_of = |data: &PanelData| {
            build(l, &t, data)
                .into_iter()
                .find_map(|r| match r {
                    Row::LabeledButton {
                        id,
                        value,
                        button,
                        enabled,
                        ..
                    } if id == HOT_BEEPER => Some((value, button, enabled)),
                    _ => None,
                })
                .expect("the buzzer row carries a button in both states")
        };

        let (live_value, live_caption, live_enabled) = row_of(&live);
        let (lost_value, lost_caption, lost_enabled) = row_of(&lost);

        assert!(live_enabled, "a mode that was read is actionable");
        assert!(!lost_enabled, "a mode that was not read is not actionable");
        assert_eq!(
            lost_caption, live_caption,
            "the button keeps the caption its carried mode gives it"
        );
        assert_eq!(live_value, l.t(strings::Key::BeeperDisabled));
        assert_eq!(
            lost_value, EM_DASH,
            "the value reports the reading, and this cycle has none"
        );
    }
}
