//! Device model: turns raw descriptor fields into typed readings.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::descriptor::{
    read_flag, read_value, write_value, Descriptor, FlagField, Scope, ValueField, PAGE_BATTERY,
    PAGE_POWER, PAGE_VENDOR,
};
use super::device::RawDevice;
use super::selftest::{Safety, SelfTestOutcome, SelfTestSession, Tick};
use crate::error::{Error, Result};
// Power page usages
const U_VOLTAGE: u16 = 0x30;

/// The collections `Voltage` is read from, in the order the panel shows them.
///
/// One list, two decisions that must agree: `connect` refuses a firmware that
/// declares none of them, and `poll` bases its liveness probe on exactly the
/// ones that are declared. Written apart from either so neither can grow a
/// fourth collection the other does not know about.
const VOLTAGE_SCOPES: [Scope; 3] = [Scope::Input, Scope::Output, Scope::PowerSummary];
const U_CONFIG_VOLTAGE: u16 = 0x40;
const U_PERCENT_LOAD: u16 = 0x35;
const U_ACTIVE_POWER: u16 = 0x34;
const U_APPARENT_POWER: u16 = 0x33;
const U_CONFIG_APPARENT_POWER: u16 = 0x43;
const U_CONFIG_ACTIVE_POWER: u16 = 0x44;
const U_LOW_VOLTAGE_TRANSFER: u16 = 0x53;
const U_HIGH_VOLTAGE_TRANSFER: u16 = 0x54;
const U_TEST: u16 = 0x58;
const U_AUDIBLE_ALARM: u16 = 0x5A;
const U_INTERNAL_FAILURE: u16 = 0x62;
const U_VOLTAGE_OUT_OF_RANGE: u16 = 0x63;
const U_FREQUENCY_OUT_OF_RANGE: u16 = 0x64;
const U_OVERLOAD: u16 = 0x65;
const U_BOOST: u16 = 0x6E;

// Battery page usages
const U_REMAINING_CAPACITY: u16 = 0x66;
const U_RUNTIME_TO_EMPTY: u16 = 0x68;
const U_REMAINING_CAPACITY_LIMIT: u16 = 0x29;
const U_WARNING_CAPACITY_LIMIT: u16 = 0x8C;
const U_REMAINING_TIME_LIMIT: u16 = 0x2A;
const U_AC_PRESENT: u16 = 0xD0;
const U_DISCHARGING: u16 = 0x45;
const U_CHARGING: u16 = 0x44;
const U_BELOW_CAPACITY_LIMIT: u16 = 0x42;
const U_REMAINING_TIME_LIMIT_EXPIRED: u16 = 0x43;
const U_FULLY_CHARGED: u16 = 0x46;

/// Volts per raw unit, decided by the collection the field sits under.
///
/// Never from the declared `UnitExponent`: this firmware reports exponent 6 and
/// 7 where the real scale is 0 and −1, so the caps cannot be believed and the
/// scale is fixed per collection instead. Mains-side voltages are whole volts
/// (`0x00e0` = 224 V); the battery side is tenths (`0x010f` = 27.1 V, not 271).
///
/// Keyed on scope rather than on the call site. The decision was made twice
/// before — once in `read_config`, once in `poll` — and the two copies were
/// kept equal by nothing but a pair of comments saying they were.
const fn volts_per_unit(scope: Scope) -> f32 {
    match scope {
        // Battery: ConfigVoltage and Voltage both sit under PowerSummary.
        Scope::PowerSummary => 0.1,
        // Mains: input, output, and the transfer thresholds, which the
        // descriptor leaves unscoped because they appear once.
        Scope::Input | Scope::Output | Scope::Unscoped => 1.0,
    }
}

/// Result of the last self-test, read-only. Writing to Test is deliberately
/// unimplemented: this firmware ignores quick-test and the only value that
/// does anything starts an unbounded discharge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestResult {
    Passed,
    PassedWithWarning,
    Error,
    Aborted,
    InProgress,
    NotRun,
    Scheduled,
    Unknown(u32),
}

impl TestResult {
    fn from_raw(v: u32) -> Self {
        match v {
            1 => Self::Passed,
            2 => Self::PassedWithWarning,
            3 => Self::Error,
            4 => Self::Aborted,
            5 => Self::InProgress,
            6 => Self::NotRun,
            7 => Self::Scheduled,
            other => Self::Unknown(other),
        }
    }

    /// The localization key for the last-test line.
    pub(crate) fn lang_key(self) -> crate::strings::Key {
        use crate::strings::Key;
        match self {
            Self::Passed => Key::TestTestPassed,
            Self::PassedWithWarning => Key::TestTestPassedWarning,
            Self::Error => Key::TestTestError,
            Self::Aborted => Key::TestTestAborted,
            Self::InProgress => Key::TestTestInProgress,
            Self::NotRun => Key::TestTestNotRun,
            Self::Scheduled => Key::TestTestScheduled,
            Self::Unknown(_) => Key::TestTestUnknown,
        }
    }
}

/// One complete poll of the device. Fields the device does not expose are
/// `None` and must be omitted from the UI entirely rather than shown as zero.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reading {
    pub input_voltage: Option<f32>,
    pub output_voltage: Option<f32>,
    pub battery_voltage: Option<f32>,
    pub battery_nominal_voltage: Option<f32>,
    pub input_nominal_voltage: Option<f32>,
    pub load_percent: Option<u32>,
    pub load_watts: Option<u32>,
    /// Apparent power in VA, alongside the active power in watts. The two
    /// differ by the power factor of whatever is plugged in, so showing both
    /// is what makes a reactive load visible.
    pub load_va: Option<u32>,
    pub charge_percent: Option<u32>,
    pub runtime_seconds: Option<u32>,
    pub test_result: Option<TestResult>,
    /// Buzzer mode, read every poll like every other piece of device state.
    ///
    /// `None` means the mode could not be read *this cycle*, exactly as for
    /// the fields around it — not that the device has no buzzer, and not that
    /// the mode is unknown from here on. It used to live outside this struct,
    /// read only at connect and inside the toggle handler, and a single failed
    /// read therefore left the utility with no way to learn the mode again:
    /// the one thing that would re-read it was the button, and the button is
    /// drawn from the value. Observed in the field as a buzzer row that lost
    /// its value and its control until the utility was restarted, while the
    /// device held the mode that had just been written to it.
    ///
    /// It belongs in the poll and not in the connect-time `DeviceConfig`
    /// because it is state, not configuration: it changes while the device
    /// stays plugged in — this utility writes it, and so does the front panel
    /// of the UPS.
    pub beeper: Option<Beeper>,

    /// Mains voltage below which the UPS transfers to battery.
    pub low_transfer_voltage: Option<f32>,
    /// Mains voltage above which the UPS transfers to battery.
    pub high_transfer_voltage: Option<f32>,
    /// Charge percentage at which `below_capacity_limit` is raised.
    pub capacity_limit_percent: Option<u32>,
    /// Charge percentage at which the device warns.
    pub warning_capacity_percent: Option<u32>,
    /// Remaining runtime, in seconds, at which the time limit is considered
    /// expired.
    pub runtime_limit_seconds: Option<u32>,
    /// Nameplate VA, from `ConfigApparentPower` (0x43) — the field the HID
    /// Power Device spec defines for apparent power. See `DeviceConfig` for
    /// why neither nameplate figure is crossed.
    pub nominal_va: Option<u32>,
    /// Nameplate watts, from `ConfigActivePower` (0x44) — the spec's field
    /// for real power. Beside `nominal_va` rather than off in `Identity`,
    /// where it used to live: the two are one nameplate read the same way at
    /// the same moment, and splitting the pair across two structs gave the
    /// panel two unrelated paths to one figure.
    pub nominal_power_w: Option<u32>,

    /// Status flags. Every one is `Option<bool>`: `None` means the flag could
    /// not be read this cycle, which is distinct from the device reporting it
    /// clear. Collapsing the two into `false` was the origin of the phantom
    /// notifications — a failed read looked identical to a real transition, so
    /// `diff` fired on the recovery edge. `None` is resolved against the
    /// previous state in `notify`, and rendered as absent (not false) in the
    /// panel. Previously only `ac_present` was protected, by a separate
    /// `ac_present_known` companion; that pattern is now the type of every flag.
    pub ac_present: Option<bool>,
    pub discharging: Option<bool>,
    pub charging: Option<bool>,
    pub below_capacity_limit: Option<bool>,
    pub fully_charged: Option<bool>,
    pub internal_failure: Option<bool>,
    pub overload: Option<bool>,
    /// Mains voltage outside the transfer window.
    pub voltage_out_of_range: Option<bool>,
    /// Mains frequency outside tolerance. The only trace of frequency in this
    /// device: the measured value itself is not reported at all.
    pub frequency_out_of_range: Option<bool>,
    /// AVR active — the UPS is correcting mains voltage by transformer tap
    /// rather than transferring to battery.
    pub boost: Option<bool>,
    /// The remaining-runtime threshold has been crossed.
    pub runtime_limit_expired: Option<bool>,
}

impl Reading {
    /// Whether the device is reporting a critical fault.
    ///
    /// One definition, used by both the tray (to choose the red icon) and the
    /// panel (to colour the row). They disagreed before: the tray counted three
    /// flags, the panel coloured six, so an expired runtime limit or an
    /// out-of-range mains showed red in the window while the tray stayed green.
    /// A flag that could not be read (`None`) is not a fault — only a confirmed
    /// `Some(true)` is.
    pub(crate) fn is_critical(&self) -> bool {
        [
            self.internal_failure,
            self.overload,
            self.below_capacity_limit,
            self.runtime_limit_expired,
            self.voltage_out_of_range,
            self.frequency_out_of_range,
        ]
        .contains(&Some(true))
    }

    /// Whether the mains are confirmed absent. `None` (unread) is not absence —
    /// the on-battery state is asserted only when the device actually says so.
    pub(crate) fn on_battery(&self) -> bool {
        self.ac_present == Some(false)
    }
}

/// Static identity strings, read once at connect.
#[derive(Debug, Clone, Default)]
pub(crate) struct Identity {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub chemistry: Option<String>,
    pub firmware: Option<String>,
}

/// Device configuration: values that describe how the UPS is set up rather
/// than what it is currently doing.
///
/// These are read **once, at connect**, and never polled again. Nothing here
/// can change while the device stays plugged in — a UPS does not revise its
/// own nameplate rating or move its transfer thresholds mid-session — so
/// re-reading them every three seconds spent five USB control transfers per
/// poll, forever, to re-learn constants.
///
/// That is worth removing for its own sake, but the real reason is the
/// device: these units are known to stop responding under heavy polling until
/// physically reconnected, so every transfer removed from the repeating path
/// buys reliability. Together with per-poll report caching this takes a poll
/// from 28 transfers to 10.
///
/// If the device is unplugged and replaced, `Ups::connect` runs again and
/// these are re-read. A changed device is a new connection, never a new
/// value on an existing one.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DeviceConfig {
    pub input_nominal_voltage: Option<f32>,
    pub battery_nominal_voltage: Option<f32>,
    /// Nameplate VA and watts, mapped straight to the spec usages with no
    /// crossing: 0x43 `ConfigApparentPower` is VA, 0x44 `ConfigActivePower`
    /// is watts. An earlier version crossed them on the theory that the
    /// firmware had the pair swapped, which was a misreading of the dump:
    /// the two fields are declared at adjacent *bit* offsets (40 and 41)
    /// while both are 16 bits wide, so the offsets in the listing cannot be
    /// taken as byte positions and the manual decode based on them was
    /// meaningless. Nothing here decodes by offset anyway —
    /// `HidP_GetUsageValue` resolves by usage — so the crossing simply
    /// reported each figure under the other's name, and the panel showed
    /// `810 VA / 1350 W` for a unit that is 1350 VA / 810 W. Pinned by
    /// `nameplate_usages_are_not_crossed`.
    pub nominal_va: Option<u32>,
    pub nominal_power_w: Option<u32>,
    pub low_transfer_voltage: Option<f32>,
    pub high_transfer_voltage: Option<f32>,
    pub capacity_limit_percent: Option<u32>,
    pub warning_capacity_percent: Option<u32>,
    pub runtime_limit_seconds: Option<u32>,
}

pub(crate) struct Ups {
    dev: RawDevice,
    desc: Descriptor,
    pub identity: Identity,
    beeper_field: Option<ValueField>,
    /// Read once at connect. See `DeviceConfig`.
    config: DeviceConfig,
    /// The poll thread's stop flag, consulted between control transfers.
    ///
    /// Held by the device rather than passed to each call, because the value
    /// it guards is the device: every series of transfers this type performs —
    /// the connect sequence, a poll, a self-test — belongs to the one thread
    /// that owns it, and threading the flag through nine signatures would put
    /// the same argument in nine places for one fact. See [`Error::Stopped`].
    stop: Arc<AtomicBool>,
    /// Feature reports already fetched during the current poll, keyed by
    /// report ID.
    ///
    /// Fields are addressed individually but arrive in groups: a single
    /// report carries every flag in `PresentStatus`, another carries both
    /// transfer thresholds, and so on. Fetching per field meant report 11 was
    /// pulled six times and report 23 five times in one pass — 28 USB control
    /// transfers to read 12 distinct reports, each repeat returning bytes
    /// identical to the ones just discarded.
    ///
    /// That is not merely wasted work on a device deliberately held to one
    /// read per second. The units this targets are known to stop
    /// responding under aggressive polling until physically reconnected, so
    /// more than halving the traffic is a reliability fix, not an
    /// optimisation.
    ///
    /// Cleared at the start of every poll, so a cached report never outlives
    /// the pass that read it and no value can be a cycle stale.
    cache: RefCell<HashMap<u8, Vec<u8>>>,
}

/// A field read, together with whether the descriptor declares the field at all.
///
/// `None` on its own cannot say why a value is missing, and the two reasons
/// call for opposite conclusions: a usage this firmware never published is a
/// settled fact about the device, while a declared field that did not answer is
/// evidence the device has stopped answering. The liveness probe in `poll`
/// turns three failed reads into a disconnect, so it must count only the fields
/// that could have answered.
#[derive(Clone, Copy)]
struct Measured<T> {
    /// The value, or `None` when the field is absent or the read failed.
    value: Option<T>,
    /// Whether the descriptor declares this field.
    declared: bool,
}

impl<T> Measured<T> {
    /// A field the descriptor does not declare: nothing was read, and nothing
    /// could have been.
    const fn undeclared() -> Self {
        Self {
            value: None,
            declared: false,
        }
    }

    /// The outcome of reading a declared field, whether or not it answered.
    const fn read(value: Option<T>) -> Self {
        Self {
            value,
            declared: true,
        }
    }

    /// Rescales the value while keeping what is known about the field.
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Measured<U> {
        Measured {
            value: self.value.map(f),
            declared: self.declared,
        }
    }
}

impl Ups {
    /// Opens the UPS and reads its descriptor, reporting whether more than one
    /// interface matched.
    ///
    /// # Errors
    ///
    /// [`Error::UsageAbsent`] when the descriptor exposes no mains-voltage
    /// field: a device that answers but publishes nothing this utility can
    /// monitor is not one to keep polling. Propagates
    /// [`Error::DeviceNotFound`] and [`Error::Enumeration`] from
    /// [`RawDevice::open_matching`], and [`Error::Parse`] from
    /// [`Descriptor::parse`].
    ///
    /// The caller maps all of these to a [`ConnectFailure`](crate::error::ConnectFailure)
    /// for the panel; the full text goes to the log.
    pub(crate) fn connect(vid: u16, pid: u16, stop: Arc<AtomicBool>) -> Result<(Self, bool)> {
        // Multiple matches: the opened interface comes back separately from
        // the ones passed over, so taking one is not an indexing operation and
        // has no empty case to be wrong about. Failing on ambiguity would be
        // worse than picking one, so the alternatives are logged instead.
        let (dev, alternatives) = RawDevice::open_matching(vid, pid)?;
        let ambiguous = !alternatives.is_empty();

        // Named here, and only here, because this is the one moment the
        // choice is arbitrary and the answer matters. A UPS exposing several
        // HID interfaces, or two UPSs of the same model on one machine, both
        // land in this branch, and "using the first one found" is unactionable
        // without saying which. The remaining paths are listed too: whoever
        // reads this is deciding whether the right one was taken, and that
        // needs the alternatives.
        if ambiguous {
            let others: Vec<&str> = alternatives.iter().map(|d| d.path.as_str()).collect();
            crate::evlog::event(
                crate::evlog::Cat::Device,
                &format!(
                    "several matching HID interfaces; opened {} (also present: {})",
                    dev.path,
                    others.join(", ")
                ),
            );
        }

        // A new connection re-asks every question, so the repeat filter must
        // not suppress an answer merely because the previous connection gave
        // the same one. Otherwise a device reconnected after a cable fault
        // would leave the file implying nothing had been re-checked.
        crate::evlog::debug_reset();

        // Between steps, never inside a transfer. Everything from here on is
        // device traffic — a descriptor parse, five indexed strings, five
        // configuration reports and the beeper — and on a device that has
        // stopped answering each of those costs a driver timeout. See
        // [`Error::Stopped`].
        Self::keep_going(&stop)?;
        let desc = Descriptor::parse(&dev)?;

        // A firmware that publishes no `Voltage` under any collection is
        // incompatible with this utility, and that is decided once, here, on
        // the connection.
        //
        // It used to be rediscovered by every poll and misread every time.
        // The liveness probe cannot tell "the usage is not in the descriptor"
        // from "the read failed" — `opt_voltage` returns `None` for both — so
        // such a device was declared unresponsive, disconnected, reconnected
        // and declared unresponsive again, forever, with the log saying only
        // "read failed 3 times" and never that the field does not exist.
        // `UsageAbsent` says exactly that, once, and classifies as
        // `ConnectFailure::Incompatible` rather than `Unresponsive`.
        if !VOLTAGE_SCOPES
            .iter()
            .any(|scope| desc.find_value(PAGE_POWER, U_VOLTAGE, *scope).is_some())
        {
            return Err(Error::UsageAbsent {
                page: PAGE_POWER,
                usage: U_VOLTAGE,
            });
        }

        Self::keep_going(&stop)?;
        let identity = Identity {
            model: dev.indexed_string(1),
            serial: dev.indexed_string(2),
            manufacturer: dev.indexed_string(3),
            chemistry: dev.indexed_string(4),
            firmware: dev.indexed_string(5),
        };

        let beeper_field = desc.find_value(PAGE_POWER, U_AUDIBLE_ALARM, Scope::Unscoped);

        let mut ups = Self {
            dev,
            desc,
            identity,
            beeper_field,
            config: DeviceConfig::default(),
            cache: RefCell::new(HashMap::new()),
            stop,
        };
        // Everything static, in one pass, while the connection is fresh. The
        // cache makes this cost 5 transfers rather than 8, and the poll loop
        // then never touches these reports again.
        Self::keep_going(&ups.stop)?;
        ups.config = ups.read_config();
        ups.invalidate_cache();
        Ok((ups, ambiguous))
    }

    /// Reads everything static in one pass, at connect time.
    ///
    /// Voltages go through `opt_voltage`, so the scale is the same one `poll`
    /// uses and comes from the same place.
    fn read_config(&mut self) -> DeviceConfig {
        DeviceConfig {
            input_nominal_voltage: self.opt_voltage(PAGE_POWER, U_CONFIG_VOLTAGE, Scope::Input),
            battery_nominal_voltage: self.opt_voltage(
                PAGE_POWER,
                U_CONFIG_VOLTAGE,
                Scope::PowerSummary,
            ),
            nominal_va: self.opt_value(PAGE_POWER, U_CONFIG_APPARENT_POWER, Scope::Unscoped),
            nominal_power_w: self.opt_value(PAGE_POWER, U_CONFIG_ACTIVE_POWER, Scope::Unscoped),
            low_transfer_voltage: self.opt_voltage(
                PAGE_POWER,
                U_LOW_VOLTAGE_TRANSFER,
                Scope::Unscoped,
            ),
            high_transfer_voltage: self.opt_voltage(
                PAGE_POWER,
                U_HIGH_VOLTAGE_TRANSFER,
                Scope::Unscoped,
            ),
            capacity_limit_percent: self.opt_value(
                PAGE_BATTERY,
                U_REMAINING_CAPACITY_LIMIT,
                Scope::Unscoped,
            ),
            warning_capacity_percent: self.opt_value(
                PAGE_BATTERY,
                U_WARNING_CAPACITY_LIMIT,
                Scope::Unscoped,
            ),
            runtime_limit_seconds: self.opt_value(
                PAGE_BATTERY,
                U_REMAINING_TIME_LIMIT,
                Scope::Unscoped,
            ),
        }
    }

    fn feature_buf(&self, rid: u8) -> Vec<u8> {
        // The id is the first byte because it is pushed first, not because a
        // subscript says so; `resize` supplies the rest of the report.
        let len = self.desc.feature_len.max(2);
        let mut buf = Vec::with_capacity(len);
        buf.push(rid);
        buf.resize(len, 0);
        buf
    }

    /// Runs `read` against a feature report, fetching it unless this poll
    /// already has it.
    ///
    /// The report is lent, not handed over. Returning an owned `Vec` meant a
    /// 64-byte allocation per field read — twenty-one of them a poll, forever,
    /// for buffers that were dropped the moment the field was parsed. The
    /// cached bytes are what every reader wants, and they already live
    /// somewhere; a borrow says so.
    ///
    /// `&mut [u8]` because that is what the `HidP_Get*` bindings take. They do
    /// not write to it, but the buffer they are given is the cached one either
    /// way, so a report that did come back altered would be altered in the only
    /// copy there is — and would then be discarded with the rest of the cache
    /// at the end of the poll, which is the same lifetime the private copy had.
    /// # Errors
    ///
    /// Whatever `read` returns, and [`Error::FeatureRead`] from
    /// [`RawDevice::get_feature`] when the report is not cached and the
    /// transfer fails. Nothing is cached on a failed transfer, so the next
    /// caller retries rather than inheriting a gap.
    fn with_report<T>(&self, rid: u8, read: impl FnOnce(&mut [u8]) -> Result<T>) -> Result<T> {
        // Taken out of the cache rather than lent from inside it, so no borrow
        // of the `RefCell` is outstanding while `read` runs.
        //
        // The cached branch used to return `read(cached)` from inside a live
        // `borrow_mut`. Today's two closures — `read_value` and `read_flag` —
        // never touch the cache, so nothing borrowed it twice; but this is the
        // one place every field read passes through, it takes an arbitrary
        // closure, and the first closure that needed a second report would have
        // panicked on `already borrowed` in the poll thread, where the release
        // profile turns a panic into `abort`. The borrow now ends at the end of
        // its own statement, which is a property of this function rather than
        // of every closure that will ever be passed to it.
        let cached = self.cache.borrow_mut().remove(&rid);
        let mut buf = if let Some(buf) = cached {
            buf
        } else {
            // Every control transfer in this type is issued below this line, so
            // this is the one place a series of them can be cut short — and it
            // is cut short between transfers, which is the only point at which
            // stopping is safe. A cached report is answered either way: it
            // costs nothing, and returning `Stopped` for it would make the
            // outcome depend on the cache.
            Self::keep_going(&self.stop)?;
            // The transfer happens outside any borrow of the cache: it takes
            // milliseconds, and nothing else on this thread should be waiting
            // on a `RefCell` for that long. Nothing is put back on failure, so
            // the next caller retries rather than inheriting a gap.
            let mut buf = self.feature_buf(rid);
            self.dev.get_feature(&mut buf)?;
            buf
        };
        let out = read(&mut buf);
        self.cache.borrow_mut().insert(rid, buf);
        out
    }

    /// `Ok` while the poll thread is still meant to be working.
    ///
    /// The counterpart of the check `run_protocol` already makes between the
    /// steps of a self-test, applied to the other two series of transfers this
    /// type performs.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] once the flag is raised.
    fn keep_going(stop: &AtomicBool) -> Result<()> {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Stopped);
        }
        Ok(())
    }

    /// Drops everything cached, so the next read goes to the device.
    ///
    /// Called at the start of each poll and after any write: a report kept
    /// past the pass that fetched it would serve stale measurements, and one
    /// kept past a beeper write would report the state from before it.
    fn invalidate_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    /// One scalar field, through the report cache.
    ///
    /// # Errors
    ///
    /// [`Error::UsageMissing`] from [`read_value`], or [`Error::FeatureRead`]
    /// when the report it lives in could not be fetched.
    fn value_of(&self, field: ValueField) -> Result<u32> {
        self.with_report(field.report_id, |buf| read_value(&self.dev, field, buf))
    }

    /// One flag field, through the report cache.
    ///
    /// # Errors
    ///
    /// [`Error::UsageMissing`] from [`read_flag`], or [`Error::FeatureRead`]
    /// when the report it lives in could not be fetched. A flag that is clear
    /// is `Ok(false)`, not an error.
    fn flag_of(&self, field: FlagField) -> Result<bool> {
        self.with_report(field.report_id, |buf| read_flag(&self.dev, field, buf))
    }

    /// Reads a voltage field and scales it, choosing the scale by collection.
    ///
    /// Every voltage in the utility goes through here, so the scale is decided
    /// once, by the field's own scope, instead of at each call site. It used to
    /// be decided twice — `read_config` and `poll` each held a private copy of
    /// both constants — and nothing but agreement between two comments kept
    /// them equal: a divergence would have compiled, passed every test, and
    /// shown the same reading as two different numbers on two panel rows.
    fn opt_voltage(&self, page: u16, usage: u16, scope: Scope) -> Option<f32> {
        self.measure_voltage(page, usage, scope).value
    }

    /// The same reading, carrying whether the descriptor declares the field.
    ///
    /// Only the liveness probe in `poll` needs the second half, and it used to
    /// get it by asking `find_value` again for each of the three scopes — six
    /// linear searches where three would do, and, more to the point, the
    /// question "is this field declared" answered in two places that could
    /// come to differ. The lookup the read itself performed is the answer.
    fn measure_voltage(&self, page: u16, usage: u16, scope: Scope) -> Measured<f32> {
        self.measure_value(page, usage, scope)
            .map(|v| v as f32 * volts_per_unit(scope))
    }

    fn opt_value(&self, page: u16, usage: u16, scope: Scope) -> Option<u32> {
        self.measure_value(page, usage, scope).value
    }

    /// Reads a scalar, and says why when it cannot.
    ///
    /// `None` here is what the panel renders as a dash. That is the right
    /// thing to show — a value that is not available must not be faked or
    /// held over — but it collapses three distinct causes into one character:
    /// the usage is absent from this device's descriptor, or the control
    /// transfer that fetches its report failed, or the report came back but
    /// `HidP_GetUsageValue` could not resolve the field inside it. Nothing
    /// downstream can tell them apart, and they call for different responses:
    /// the first is permanent and means the field will never appear on this
    /// firmware, the second is usually a cable or a device that has stopped
    /// answering, the third is a descriptor that does not describe its own
    /// reports.
    ///
    /// At the Normal level nothing is written: a dropped field is not an
    /// event about the UPS, and the log stays a record of what happened
    /// rather than of what was attempted. At Debug the cause is named, which
    /// is the entire reason that level exists.
    ///
    /// All three throttle keys carry the scope, because `page:usage` does not
    /// name a field on this device. Voltage is read three times a poll —
    /// `Input`, `Output` and `PowerSummary` — and with the scope left out of
    /// the key those three shared one throttle counter. Two things followed, both visible in
    /// a log from a real test: a sibling scope succeeding in the same poll
    /// cleared the counter of a scope that had just failed, so the file
    /// announced "readable again" one line below the failure and a second
    /// before the report actually answered; and a scope failing on every poll
    /// never accumulated a count, so it was written out in full every poll
    /// instead of being throttled. The absent-usage key above was already
    /// scoped, which is what made the omission in the other two a slip rather
    /// than a decision.
    fn measure_value(&self, page: u16, usage: u16, scope: Scope) -> Measured<u32> {
        // The early return is not shortened to `?`: the absent-usage case is
        // one of the causes that has to be distinguished, and `?` would make
        // it indistinguishable from a failed read at the call site.
        let Some(f) = self.desc.find_value(page, usage, scope) else {
            // `format_args!`, not `format!`: nothing is joined unless the
            // Debug level is on. This path runs on every poll for a field the
            // firmware does not have, so an eagerly built string would be
            // allocated ten times a cycle, forever, only to be dropped.
            crate::evlog::debug_repeating(
                format_args!("absent:{page:#04x}:{usage:#04x}:{scope:?}"),
                format_args!(
                    "no value for {}: usage {usage:#04x} on page {page:#04x} \
                     ({scope:?}) is not in the descriptor",
                    field_name(page, usage)
                ),
            );
            return Measured::undeclared();
        };
        // The failure and the recovery must be counted under the *same* key —
        // that identity is what lets a success clear the counter its own
        // failures raised — so the key is built once here rather than spelled
        // out in both arms. Two copies of one format string is how the scope
        // came to be present in one place and missing in another.
        //
        // A `Copy` value rather than a `String`: `format_args!` renders it only
        // if the line is actually written, which is what keeps this path free
        // while the Debug level is off. Ten fields a poll pass through here.
        let key = ValueKey { page, usage, scope };
        match self.value_of(f) {
            Ok(v) => {
                // The success path, and therefore the hottest one: ten fields
                // a poll arrive here. Deferred formatting is what keeps it
                // free while the level is off.
                crate::evlog::debug_recovered(
                    format_args!("{key}"),
                    format_args!("{} ({scope:?}) readable again", field_name(page, usage)),
                );
                Measured::read(Some(v))
            }
            Err(e) => {
                crate::evlog::debug_repeating(
                    format_args!("{key}"),
                    format_args!(
                        "no value for {} ({scope:?}): usage {usage:#04x} on page {page:#04x}, \
                         feature report {}: {e}",
                        field_name(page, usage),
                        f.report_id
                    ),
                );
                Measured::read(None)
            }
        }
    }

    /// Reads a flag, distinguishing "device says false" from "the read
    /// failed". The difference matters: `ACPresent` defaulting to false on a
    /// failed read is indistinguishable from a real mains failure, and the
    /// notification state machine fires on the edge either way. This is the
    /// origin of the phantom "switched to battery" balloons.
    fn opt_flag_checked(&self, page: u16, usage: u16) -> Option<bool> {
        // Every flag read here is single-occurrence, so `Unscoped` selects it
        // regardless of collection — the same result the old first-match lookup
        // gave, now stated rather than assumed. A flag that ever appears in
        // more than one collection would pass its scope here instead.
        let Some(f) = self.desc.find_flag(page, usage, Scope::Unscoped) else {
            // Same reasoning as `opt_value`: a flag the firmware lacks fails
            // every poll, so the message must not be built to be thrown away.
            crate::evlog::debug_repeating(
                format_args!("absent:{page:#04x}:{usage:#04x}"),
                format_args!(
                    "no flag for {}: usage {usage:#04x} on page {page:#04x} \
                     is not in the descriptor",
                    field_name(page, usage)
                ),
            );
            return None;
        };
        match self.flag_of(f) {
            Ok(v) => {
                crate::evlog::debug_recovered(
                    format_args!("flag:{page:#04x}:{usage:#04x}"),
                    format_args!("{} readable again", field_name(page, usage)),
                );
                Some(v)
            }
            Err(e) => {
                crate::evlog::debug_repeating(
                    format_args!("flag:{page:#04x}:{usage:#04x}"),
                    format_args!(
                        "no flag for {}: usage {usage:#04x} on page {page:#04x}, \
                         feature report {}: {e}",
                        field_name(page, usage),
                        f.report_id
                    ),
                );
                None
            }
        }
    }

    /// Poll every parameter. A failure to read any single field degrades that
    /// field to `None` rather than failing the whole poll; only a total loss of
    /// the mains-voltage report is treated as a disconnect by the caller.
    ///
    /// # Errors
    ///
    /// [`Error::DeviceUnresponsive`], and only that. Every per-field failure
    /// is logged and folded into a `None`, so this returns `Err` on exactly
    /// one condition: every voltage the descriptor *declares* failed to read
    /// in this pass. "Declares" is load-bearing — a collection the firmware
    /// never published also reads as `None`, and counting those would make a
    /// firmware limitation indistinguishable from a dead device.
    pub(crate) fn poll(&self) -> Result<Reading> {
        // Each poll starts from the device, never from the previous pass.
        self.invalidate_cache();

        let input_voltage = self.measure_voltage(PAGE_POWER, U_VOLTAGE, Scope::Input);
        let output_voltage = self.measure_voltage(PAGE_POWER, U_VOLTAGE, Scope::Output);
        let battery_voltage = self.measure_voltage(PAGE_POWER, U_VOLTAGE, Scope::PowerSummary);
        // Nominal voltages come from the configuration read at connect. They
        // are properties of the installation, not measurements, and polling
        // them cost two USB transfers every cycle to re-learn constants.
        let battery_nominal_voltage = self.config.battery_nominal_voltage;
        let input_nominal_voltage = self.config.input_nominal_voltage;

        // A total failure to reach the device shows up as every read failing.
        //
        // The probe counts only the collections the descriptor declares. A
        // `None` from a collection this firmware never published is not
        // evidence about whether the device is answering — it is a fact about
        // the firmware, already settled at connect — and folding it in was
        // what let a missing usage masquerade as a dead device. `connect`
        // guarantees at least one is declared; the count is still checked so
        // this probe reads as "every declared voltage failed" rather than
        // being vacuously true on an empty set.
        //
        // Both counts come from the readings themselves. Asking the descriptor
        // again here was a second answer to a question the read had already
        // settled, in a loop that had to be kept in step with the three lines
        // above by hand.
        let voltages = [input_voltage, output_voltage, battery_voltage];
        let declared = voltages.iter().filter(|m| m.declared).count();
        let answered = voltages.iter().filter(|m| m.value.is_some()).count();
        // Asked before the verdict, because a stop is what makes every read
        // above return `None` once the flag is up: read as a liveness result
        // that is indistinguishable from a device that has gone, and it would
        // log a disconnect and grey the tray icon on the way out of a process
        // whose UPS was answering perfectly.
        Self::keep_going(&self.stop)?;
        if declared > 0 && answered == 0 {
            // Every declared voltage failed, so this is the device gone rather
            // than a dropped field. The individual causes were already named
            // above; this line says what the utility concluded from them,
            // which is the step that turns three failed reads into a
            // disconnect and eventually into a grey tray icon.
            crate::evlog::debug_repeating(
                format_args!("liveness"),
                format_args!(
                    "all {declared} declared voltage reads failed; \
                     treating this poll as a device read failure"
                ),
            );
            return Err(Error::DeviceUnresponsive);
        }

        Ok(Reading {
            input_voltage: input_voltage.value,
            output_voltage: output_voltage.value,
            battery_voltage: battery_voltage.value,
            battery_nominal_voltage,
            input_nominal_voltage,
            load_percent: self.opt_value(PAGE_POWER, U_PERCENT_LOAD, Scope::Unscoped),
            // ActivePower reports instantaneous watts directly; no need to
            // derive it from PercentLoad and the nameplate rating.
            load_watts: self.opt_value(PAGE_POWER, U_ACTIVE_POWER, Scope::Unscoped),
            load_va: self.opt_value(PAGE_POWER, U_APPARENT_POWER, Scope::Unscoped),
            charge_percent: self.opt_value(PAGE_BATTERY, U_REMAINING_CAPACITY, Scope::Unscoped),
            runtime_seconds: self.opt_value(PAGE_BATTERY, U_RUNTIME_TO_EMPTY, Scope::Unscoped),
            test_result: self
                .opt_value(PAGE_POWER, U_TEST, Scope::Unscoped)
                .map(TestResult::from_raw),
            // State, so polled; see the field's own documentation for why it
            // is not in the connect-time block below.
            beeper: self
                .opt_value(PAGE_POWER, U_AUDIBLE_ALARM, Scope::Unscoped)
                .map(Beeper::from_raw),

            // Thresholds and nameplate: all from the connect-time read. These
            // are settings of the device, and a setting that changed would
            // arrive with a new connection, not mid-session.
            low_transfer_voltage: self.config.low_transfer_voltage,
            high_transfer_voltage: self.config.high_transfer_voltage,
            capacity_limit_percent: self.config.capacity_limit_percent,
            warning_capacity_percent: self.config.warning_capacity_percent,
            runtime_limit_seconds: self.config.runtime_limit_seconds,
            nominal_va: self.config.nominal_va,
            nominal_power_w: self.config.nominal_power_w,

            // Every flag carries its read status: `Some(false)` is the device
            // reporting clear, `None` is a read that failed. The distinction is
            // resolved in `notify` and honoured by the panel. `opt_flag_checked`
            // is what draws it — `opt_flag`, which flattened to `false`, is gone.
            ac_present: self.opt_flag_checked(PAGE_BATTERY, U_AC_PRESENT),
            discharging: self.opt_flag_checked(PAGE_BATTERY, U_DISCHARGING),
            charging: self.opt_flag_checked(PAGE_BATTERY, U_CHARGING),
            below_capacity_limit: self.opt_flag_checked(PAGE_BATTERY, U_BELOW_CAPACITY_LIMIT),
            fully_charged: self.opt_flag_checked(PAGE_BATTERY, U_FULLY_CHARGED),
            internal_failure: self.opt_flag_checked(PAGE_POWER, U_INTERNAL_FAILURE),
            overload: self.opt_flag_checked(PAGE_POWER, U_OVERLOAD),
            voltage_out_of_range: self.opt_flag_checked(PAGE_POWER, U_VOLTAGE_OUT_OF_RANGE),
            frequency_out_of_range: self.opt_flag_checked(PAGE_POWER, U_FREQUENCY_OUT_OF_RANGE),
            boost: self.opt_flag_checked(PAGE_POWER, U_BOOST),
            runtime_limit_expired: self
                .opt_flag_checked(PAGE_BATTERY, U_REMAINING_TIME_LIMIT_EXPIRED),
        })
    }

    /// Reads the buzzer state from the device, for the write path only.
    ///
    /// Always a fresh read. This brackets a write — the mode to toggle from,
    /// and the confirmation afterwards — and both would be worthless taken
    /// from a cache or from the last poll: the first must be the mode the
    /// device holds at the instant of the write, and the second must be the
    /// device's answer rather than the cache agreeing with itself.
    ///
    /// What the panel shows does **not** come from here. It comes from
    /// [`Reading::beeper`], polled like every other piece of state, which is
    /// what makes a failed read here cost one log line instead of the row's
    /// control until the next restart.
    ///
    /// `None` covers two cases — the device exposes no `AudibleAlarmControl`
    /// at all, or the read of it failed. Both mean the toggle cannot proceed,
    /// and the caller says so; only the failed read is logged, because a
    /// device without a buzzer is not a fault.
    pub(crate) fn read_beeper(&self) -> Option<Beeper> {
        self.invalidate_cache();
        let field = self.beeper_field?;
        match self.value_of(field) {
            Ok(raw) => Some(Beeper::from_raw(raw)),
            Err(e) => {
                crate::evlog::debug_repeating(
                    format_args!("beeper_read"),
                    format_args!("beeper read failed: {e}"),
                );
                None
            }
        }
    }

    /// `AudibleAlarmControl`: 1 = disabled, 2 = enabled, 3 = muted.
    ///
    /// # Errors
    ///
    /// [`Error::UsageAbsent`] on firmware that exposes no beeper control at
    /// all — a permanent fact about the device rather than a transient
    /// failure, which is why the panel greys the control instead of retrying.
    /// Propagates [`Error::FeatureRead`] from the read half of the
    /// read-modify-write, [`Error::UsageMissing`] when the field will not
    /// place, and [`Error::FeatureWrite`] when the device refuses the write.
    pub(crate) fn set_beeper(&self, mode: u8) -> Result<()> {
        let Some(field) = self.beeper_field else {
            return Err(Error::UsageAbsent {
                page: PAGE_POWER,
                usage: U_AUDIBLE_ALARM,
            });
        };
        // Read fresh, not from the cache: this is a read-modify-write, and
        // modifying a copy taken earlier in the poll would write back stale
        // neighbouring bytes along with the new mode.
        self.invalidate_cache();
        let mut buf = self.feature_buf(field.report_id);
        self.dev.get_feature(&mut buf)?;
        // Placed by usage, not at a guessed byte. `AudibleAlarmControl` was
        // written to `buf[1]` on the assumption it is the first field after the
        // report id — true on this firmware, an off-by-one waiting to happen on
        // any other. `write_value` asks HidP to put the mode where the
        // descriptor says it goes, leaving every other field in the report as
        // it was read.
        write_value(&self.dev, field, &mut buf, u32::from(mode))?;
        let result = self.dev.set_feature(&buf);
        // The report on the device no longer matches anything cached. This
        // matters immediately: the caller reads the beeper back to confirm
        // the write, and a cached answer would report the state from before
        // it — the readback would confirm itself rather than the device.
        self.invalidate_cache();
        result
    }

    /// Runs a vendor self-test and returns its outcome.
    ///
    /// The safety conditions are read fresh here, immediately before the test,
    /// rather than taken from the last poll: a poll can be seconds old and the
    /// mains can fail or the battery discharge in that gap. The `Test` value
    /// polled during the run is read the same way every other field is, so it
    /// goes through the same cache invalidation — a stale cached `Test` would
    /// otherwise freeze the progress watch on the pre-test value.
    ///
    /// `abort` is the poll thread's stop flag. The test holds this thread for
    /// its whole duration, so without a way to interrupt it a shutdown would
    /// block on `stop_and_join` for up to the full 30 s window. The session
    /// checks the flag at every step that can block and returns `Cancelled`.
    ///
    /// The feature `Test` report is never written here or anywhere else; the
    /// test is started only by the vendor `T` command inside the session. That
    /// invariant is pinned by `test_feature_is_never_written`.
    ///
    /// `observe` receives a full reading once a second for as long as the test
    /// runs. It exists because the alternative is the one failure a monitoring
    /// tool must not have: the test holds this thread for up to half a minute,
    /// during which the panel went on repainting the reading taken *before* the
    /// device moved onto its inverter — 100 %, "on mains", 11 % load — while
    /// the battery discharged under load. A stale number presented as a current
    /// one is indistinguishable from a live one, and a self-test is the moment
    /// these readings matter most, so they are taken rather than suppressed.
    pub(crate) fn run_self_test(
        &self,
        abort: &AtomicBool,
        mut observe: impl FnMut(Reading),
    ) -> SelfTestOutcome {
        self.invalidate_cache();
        let safety = Safety {
            charge_percent: self.opt_value(PAGE_BATTERY, U_REMAINING_CAPACITY, Scope::Unscoped),
            ac_present: self.opt_flag_checked(PAGE_BATTERY, U_AC_PRESENT),
            discharging: self.opt_flag_checked(PAGE_BATTERY, U_DISCHARGING),
            // The raw value, kept whole. The session needs it twice: once to
            // refuse a start while a test is already running, and once as the
            // baseline that tells this test's result from the code the last one
            // left behind.
            test_before: self.opt_value(PAGE_POWER, U_TEST, Scope::Unscoped),
        };

        let mut session = match SelfTestSession::open(&self.dev, &self.desc) {
            Ok(s) => s,
            Err(e) => {
                crate::evlog::event(crate::evlog::Cat::Error, &format!("self-test: {e}"));
                return SelfTestOutcome::ChannelError;
            }
        };

        session.run(&safety, abort, || {
            // A full poll each tick, not a bare `Test` read. `watch` calls this
            // once a second, which is exactly the pacing floor this utility
            // holds to, so the device sees the same cadence it would have seen
            // had no test been running.
            //
            // `poll` invalidates the cache itself, which is what makes the
            // value fresh — the cache is per-poll and this loop spans many
            // seconds, so a cached `Test` would freeze the progress watch on
            // the pre-test value.
            let polled = self.poll();
            // Both read after the poll and therefore out of the reports it just
            // fetched: `Test` and `Discharging` share report ids with the fields
            // above and the cache is still warm, so the pair the session watches
            // costs no extra transfer and comes from the same instant as the
            // reading handed to `observe`.
            let state = Tick {
                test: self.opt_value(PAGE_POWER, U_TEST, Scope::Unscoped),
                discharging: self.opt_flag_checked(PAGE_BATTERY, U_DISCHARGING),
            };
            // A failed poll is reported to nobody: the panel keeps the last
            // good reading, exactly as it does between ordinary polls, and the
            // failure has already been logged field by field. Only the two
            // fields above matter to the session, and they are returned either
            // way.
            if let Ok(reading) = polled {
                observe(reading);
            }
            state
        })
    }
}

/// The key a value read is throttled and recovered under.
///
/// A type rather than a format string because the identity of the key is the
/// whole mechanism: [`crate::evlog::debug_repeating`] counts failures under it
/// and [`crate::evlog::debug_recovered`] clears that count under it, so a
/// failure and its recovery must produce the same key, and two fields that are
/// not the same field must not. Written out twice, those two requirements were
/// held only by the two copies happening to match.
///
/// The scope is part of the identity, not decoration. `page:usage` does not
/// name a field on this device: Voltage is `0x84:0x30` under `Input`, `Output`
/// and `PowerSummary` alike, three reads a poll from three different reports.
///
/// [`std::fmt::Display`] rather than an eager `String`: `format_args!` renders
/// this only if the line survives the level check, which is what keeps the
/// per-field diagnostics free while Debug is off.
#[derive(Clone, Copy)]
struct ValueKey {
    page: u16,
    usage: u16,
    scope: Scope,
}

impl std::fmt::Display for ValueKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { page, usage, scope } = self;
        write!(f, "value:{page:#04x}:{usage:#04x}:{scope:?}")
    }
}

/// The spec name of a usage, for diagnostics.
///
/// English and unlocalised, like the rest of the log: the reader is diagnosing
/// a machine that is often not theirs, and `Voltage` is searchable against the
/// HID Power Device spec and against `ups-dump` output in a way that its
/// translation into the reader's language is not. The panel is where the
/// localised label belongs.
///
/// An unknown usage prints as `usage` rather than being omitted: the numbers
/// follow in the same line either way, so an unnamed field still identifies
/// itself, and a device exposing something this build does not know about is
/// exactly the case where the line is worth having.
fn field_name(page: u16, usage: u16) -> &'static str {
    match (page, usage) {
        (PAGE_POWER, U_VOLTAGE) => "Voltage",
        (PAGE_POWER, U_CONFIG_VOLTAGE) => "ConfigVoltage",
        (PAGE_POWER, U_PERCENT_LOAD) => "PercentLoad",
        (PAGE_POWER, U_ACTIVE_POWER) => "ActivePower",
        (PAGE_POWER, U_APPARENT_POWER) => "ApparentPower",
        (PAGE_POWER, U_CONFIG_APPARENT_POWER) => "ConfigApparentPower",
        (PAGE_POWER, U_CONFIG_ACTIVE_POWER) => "ConfigActivePower",
        (PAGE_POWER, U_LOW_VOLTAGE_TRANSFER) => "LowVoltageTransfer",
        (PAGE_POWER, U_HIGH_VOLTAGE_TRANSFER) => "HighVoltageTransfer",
        (PAGE_POWER, U_TEST) => "Test",
        (PAGE_POWER, U_AUDIBLE_ALARM) => "AudibleAlarmControl",
        (PAGE_POWER, U_INTERNAL_FAILURE) => "InternalFailure",
        (PAGE_POWER, U_VOLTAGE_OUT_OF_RANGE) => "VoltageOutOfRange",
        (PAGE_POWER, U_FREQUENCY_OUT_OF_RANGE) => "FrequencyOutOfRange",
        (PAGE_POWER, U_OVERLOAD) => "Overload",
        (PAGE_POWER, U_BOOST) => "Boost",
        (PAGE_BATTERY, U_REMAINING_CAPACITY) => "RemainingCapacity",
        (PAGE_BATTERY, U_RUNTIME_TO_EMPTY) => "RunTimeToEmpty",
        (PAGE_BATTERY, U_REMAINING_CAPACITY_LIMIT) => "RemainingCapacityLimit",
        (PAGE_BATTERY, U_WARNING_CAPACITY_LIMIT) => "WarningCapacityLimit",
        (PAGE_BATTERY, U_REMAINING_TIME_LIMIT) => "RemainingTimeLimit",
        (PAGE_BATTERY, U_AC_PRESENT) => "ACPresent",
        (PAGE_BATTERY, U_DISCHARGING) => "Discharging",
        (PAGE_BATTERY, U_CHARGING) => "Charging",
        (PAGE_BATTERY, U_BELOW_CAPACITY_LIMIT) => "BelowRemainingCapacityLimit",
        (PAGE_BATTERY, U_REMAINING_TIME_LIMIT_EXPIRED) => "RemainingTimeLimitExpired",
        (PAGE_BATTERY, U_FULLY_CHARGED) => "FullyCharged",
        (PAGE_VENDOR, _) => "vendor usage",
        _ => "usage",
    }
}

/// `AudibleAlarmControl` state. The only writable field in the whole map, and
/// only on an explicit user action: it changes the UPS itself, not Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Beeper {
    Disabled,
    Enabled,
    Muted,
    Unknown(u32),
}

impl Beeper {
    pub(crate) fn from_raw(v: u32) -> Self {
        match v {
            1 => Self::Disabled,
            2 => Self::Enabled,
            3 => Self::Muted,
            other => Self::Unknown(other),
        }
    }

    /// The wire value `AudibleAlarmControl` expects, or `None` for `Unknown`.
    ///
    /// `Unknown(_)` is a mode the device reported that this code does not
    /// model; it has no defined command value, and inventing one (it silently
    /// returned "enabled" before) would send a real state change the caller
    /// never asked for. Returning `None` makes the absence explicit so the
    /// caller declines rather than guesses.
    ///
    /// That arm is reachable, and used to be documented as not being. The
    /// buzzer is commanded as `read_beeper().toggled().to_raw()`, and
    /// `Unknown` toggles to itself — so a device reporting a mode this build
    /// does not model reaches here the moment a toggle is asked for. The panel
    /// offers no button in that state (`action_key` is `None` on the same
    /// composition), but the poll thread does not take the panel's word for
    /// it, and the mode can change between the two.
    pub(crate) fn to_raw(self) -> Option<u8> {
        match self {
            Self::Disabled => Some(1),
            Self::Enabled => Some(2),
            Self::Muted => Some(3),
            Self::Unknown(_) => None,
        }
    }

    pub(crate) fn lang_key(self) -> crate::strings::Key {
        use crate::strings::Key;
        match self {
            Self::Disabled => Key::BeeperDisabled,
            Self::Enabled => Key::BeeperEnabled,
            Self::Muted => Key::BeeperMuted,
            Self::Unknown(_) => Key::BeeperUnsupported,
        }
    }

    /// Toggle target: muted counts as on, so one click silences it.
    ///
    /// `Unknown(_)` toggles to itself. An unmodelled mode has no defined
    /// opposite, and picking one would be a guess that `to_raw` then turns
    /// into a real state change on the wire; keeping the mode means `to_raw`
    /// yields `None` and the command is declined with a log line, which is the
    /// decision that module already made. Exhaustive rather than `_ =>` so a
    /// mode added later has to be answered here instead of silently inheriting
    /// "off".
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Disabled => Self::Enabled,
            Self::Enabled | Self::Muted => Self::Disabled,
            Self::Unknown(v) => Self::Unknown(v),
        }
    }

    /// Caption for the button that toggles the buzzer, or `None` when there is
    /// no action to offer.
    ///
    /// Names the *action*, not the resulting state. The button used to be
    /// captioned with `toggled().lang_key()`, which produced "Enabled" and
    /// "Disabled" — adjectives describing a state, sitting on a control, one
    /// line under a value showing the current state in the same words. It
    /// read as a second status field rather than something to press.
    ///
    /// `None` for `Unknown(_)`: there is no wire value to send, so there is no
    /// caption that would be true. This is the single place that knows a
    /// buzzer control cannot be offered — the panel branches on this answer
    /// rather than re-deriving it from the variant.
    pub(crate) fn action_key(self) -> Option<crate::strings::Key> {
        use crate::strings::Key;
        match self.toggled() {
            Self::Enabled => Some(Key::BeeperEnable),
            Self::Disabled | Self::Muted => Some(Key::BeeperDisable),
            Self::Unknown(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every usage this build knows names itself, and every name is distinct.
    ///
    /// The names are a diagnostic contract, not decoration: a reader is
    /// looking at a log from a machine that is often not theirs, and
    /// `RunTimeToEmpty` is what they will search for in the HID Power Device
    /// spec and in `ups-dump` output. A name that silently became `usage`
    /// would not break anything — the numbers still follow on the same line —
    /// which is exactly why nothing here was checked: twenty-seven arms of
    /// this table could be deleted one at a time without a single test
    /// noticing, and the mutation run said so.
    ///
    /// The table below is a second statement of the mapping, and that is what
    /// a test of a naming table is. What it is not is a second *implementation*
    /// — there is no logic here to drift, only the pairs and the strings the
    /// log is expected to contain.
    #[test]
    fn every_known_usage_names_itself() {
        let table = [
            (PAGE_POWER, U_VOLTAGE, "Voltage"),
            (PAGE_POWER, U_CONFIG_VOLTAGE, "ConfigVoltage"),
            (PAGE_POWER, U_PERCENT_LOAD, "PercentLoad"),
            (PAGE_POWER, U_ACTIVE_POWER, "ActivePower"),
            (PAGE_POWER, U_APPARENT_POWER, "ApparentPower"),
            (PAGE_POWER, U_CONFIG_APPARENT_POWER, "ConfigApparentPower"),
            (PAGE_POWER, U_CONFIG_ACTIVE_POWER, "ConfigActivePower"),
            (PAGE_POWER, U_LOW_VOLTAGE_TRANSFER, "LowVoltageTransfer"),
            (PAGE_POWER, U_HIGH_VOLTAGE_TRANSFER, "HighVoltageTransfer"),
            (PAGE_POWER, U_TEST, "Test"),
            (PAGE_POWER, U_AUDIBLE_ALARM, "AudibleAlarmControl"),
            (PAGE_POWER, U_INTERNAL_FAILURE, "InternalFailure"),
            (PAGE_POWER, U_VOLTAGE_OUT_OF_RANGE, "VoltageOutOfRange"),
            (PAGE_POWER, U_FREQUENCY_OUT_OF_RANGE, "FrequencyOutOfRange"),
            (PAGE_POWER, U_OVERLOAD, "Overload"),
            (PAGE_POWER, U_BOOST, "Boost"),
            (PAGE_BATTERY, U_REMAINING_CAPACITY, "RemainingCapacity"),
            (PAGE_BATTERY, U_RUNTIME_TO_EMPTY, "RunTimeToEmpty"),
            (
                PAGE_BATTERY,
                U_REMAINING_CAPACITY_LIMIT,
                "RemainingCapacityLimit",
            ),
            (
                PAGE_BATTERY,
                U_WARNING_CAPACITY_LIMIT,
                "WarningCapacityLimit",
            ),
            (PAGE_BATTERY, U_REMAINING_TIME_LIMIT, "RemainingTimeLimit"),
            (PAGE_BATTERY, U_AC_PRESENT, "ACPresent"),
            (PAGE_BATTERY, U_DISCHARGING, "Discharging"),
            (PAGE_BATTERY, U_CHARGING, "Charging"),
            (
                PAGE_BATTERY,
                U_BELOW_CAPACITY_LIMIT,
                "BelowRemainingCapacityLimit",
            ),
            (
                PAGE_BATTERY,
                U_REMAINING_TIME_LIMIT_EXPIRED,
                "RemainingTimeLimitExpired",
            ),
            (PAGE_BATTERY, U_FULLY_CHARGED, "FullyCharged"),
        ];
        for (page, usage, name) in table {
            assert_eq!(
                field_name(page, usage),
                name,
                "usage {usage:#06x} of page {page:#06x}"
            );
        }

        let mut names: Vec<&str> = table.iter().map(|&(_, _, name)| name).collect();
        names.sort_unstable();
        let distinct = names.len();
        names.dedup();
        assert_eq!(names.len(), distinct, "two usages share a name in the log");

        // The two fallbacks, which are what a deleted arm above would return.
        assert_eq!(field_name(PAGE_VENDOR, 0x1234), "vendor usage");
        assert_eq!(field_name(PAGE_POWER, 0xFFFF), "usage");
        assert_eq!(field_name(0xFFFF, U_VOLTAGE), "usage");
    }

    /// The wire value is the one the device defines, and every mode has its
    /// own.
    ///
    /// This byte goes into `AudibleAlarmControl` and takes effect on the
    /// hardware. A wrong one is not a display fault: it silences a UPS whose
    /// owner asked for it to be audible, or sounds one they had muted, and the
    /// panel then reads back whatever the device now is — so the utility and
    /// the device agree, and both are wrong about what was asked for.
    ///
    /// Checked as a round trip against `from_raw` as well as by value: the two
    /// are one mapping read in opposite directions, and a mode that survives
    /// the trip is one the utility can command back to where it found it.
    #[test]
    fn every_modelled_beeper_mode_has_its_own_wire_value() {
        assert_eq!(Beeper::Disabled.to_raw(), Some(1));
        assert_eq!(Beeper::Enabled.to_raw(), Some(2));
        assert_eq!(Beeper::Muted.to_raw(), Some(3));

        for raw in 1..=3u8 {
            assert_eq!(
                Beeper::from_raw(u32::from(raw)).to_raw(),
                Some(raw),
                "mode {raw} must command back to itself"
            );
        }

        // A mode the device reported and this code does not model has no
        // defined command value. Inventing one — it used to answer "enabled" —
        // sends a state change nobody asked for.
        assert_eq!(Beeper::Unknown(9).to_raw(), None);
    }

    /// The button says what pressing it does, never what the buzzer is.
    #[test]
    fn the_button_names_an_action_not_a_state() {
        use crate::strings::Key;
        assert_eq!(Beeper::Disabled.action_key(), Some(Key::BeeperEnable));
        assert_eq!(Beeper::Enabled.action_key(), Some(Key::BeeperDisable));
        // Muted counts as on, so the offered action is to silence it.
        assert_eq!(Beeper::Muted.action_key(), Some(Key::BeeperDisable));
        // The action key must never collide with a state key, or the caption
        // reverts to the adjective this exists to remove.
        for b in [Beeper::Disabled, Beeper::Enabled, Beeper::Muted] {
            assert_ne!(b.action_key(), Some(b.lang_key()));
            assert_ne!(b.action_key(), Some(b.toggled().lang_key()));
        }
        // An unmodelled mode offers nothing: no wire value, so no caption.
        assert_eq!(Beeper::Unknown(9).action_key(), None);
        assert_eq!(Beeper::Unknown(9).toggled(), Beeper::Unknown(9));
    }

    /// A button is offered exactly where a toggle can be commanded.
    ///
    /// Two places derive the same predicate from `toggled()`: the panel asks
    /// `action_key` whether to render a button, and the poll thread asks
    /// `toggled().to_raw()` whether it has anything to write. They are the
    /// same question and must give the same answer — a caption over a command
    /// that will be refused, or a refusal of a command the panel invited, are
    /// both the drift this pins shut.
    ///
    /// Asserted over every mode this type can hold, including the unmodelled
    /// ones, because the disagreement would live precisely in the arm nobody
    /// writes a literal for.
    #[test]
    fn a_button_is_offered_exactly_where_a_toggle_can_be_written() {
        let modes = [Beeper::Disabled, Beeper::Enabled, Beeper::Muted]
            .into_iter()
            .chain((0..=u8::MAX).map(|v| Beeper::from_raw(u32::from(v))));
        for mode in modes {
            assert_eq!(
                mode.action_key().is_some(),
                mode.toggled().to_raw().is_some(),
                "{mode:?}: the panel and the poll thread disagree on whether \\
                 this mode can be toggled"
            );
        }
    }

    /// The source of `poll`, for the two tests that check which reads live
    /// inside it. Extracted by brace matching from the real signature, which
    /// (with its parenthesis) appears only at the definition, so the earlier
    /// worry about the bare name colliding with a test literal no longer
    /// applies.
    ///
    /// **Structural, and temporarily so.** Both tests that use this read
    /// source text rather than running the code, because `poll` needs a real
    /// HID handle. A text search cannot tell a read that happens from one in
    /// an arm that is never reached, so they are weaker than the properties
    /// they stand for. The remedy is the one applied to `app::plan_emission`:
    /// lift the decision — here, *which* usages one cycle asks for — into a
    /// function that returns the list instead of performing the reads, then
    /// assert on the list. Until that is done, these are the only checks there
    /// are.
    fn poll_body() -> &'static str {
        crate::testsupport::fn_body(include_str!("ups.rs"), "pub(crate) fn poll(")
    }

    /// The poll loop must not re-read device configuration.
    ///
    /// Every field in `DeviceConfig` describes how the UPS is set up, not what
    /// it is doing: nameplate rating, transfer window, capacity thresholds,
    /// nominal voltages. None can change while the device stays plugged in,
    /// and a device that *did* change is a new connection, which re-runs
    /// `connect` and re-reads them.
    ///
    /// Polling them cost five USB control transfers per cycle to re-learn
    /// constants. On units that stop responding under heavy polling until
    /// physically reconnected, that is a reliability cost, not just waste:
    /// with per-poll report caching this took a poll from 28 transfers to 10.
    ///
    /// This test pins the intent. If a field is ever moved back into the poll
    /// path, the reasoning above is what has to be argued against.
    #[test]
    fn device_configuration_is_read_once_not_polled() {
        let poll = poll_body();

        // Each of these resolves to a report that carries only configuration.
        // Reading any of them inside `poll` puts that report back on the
        // per-cycle path.
        for usage in [
            "U_CONFIG_VOLTAGE",
            "U_CONFIG_APPARENT_POWER",
            "U_CONFIG_ACTIVE_POWER",
            "U_LOW_VOLTAGE_TRANSFER",
            "U_HIGH_VOLTAGE_TRANSFER",
            "U_REMAINING_CAPACITY_LIMIT",
            "U_WARNING_CAPACITY_LIMIT",
            "U_REMAINING_TIME_LIMIT",
        ] {
            // Matched with a trailing delimiter, so `U_REMAINING_TIME_LIMIT`
            // does not also match `U_REMAINING_TIME_LIMIT_EXPIRED` — which is
            // a status flag and belongs in the poll path.
            let named = poll.contains(&format!("{usage},")) || poll.contains(&format!("{usage})"));
            assert!(
                !named,
                "{usage} is static configuration and must be read at connect, not polled"
            );
        }
    }

    /// Conversely, real measurements must stay in the poll path.
    ///
    /// The guard above is a one-way ratchet if nothing checks the other
    /// direction: moving a live reading into `DeviceConfig` would also
    /// satisfy it, while freezing a measurement at its connect-time value.
    ///
    /// `U_AUDIBLE_ALARM` is in this list and not in the one above, and the
    /// line between them is worth stating because the buzzer mode looks like a
    /// setting. The criterion is the one the test above names: can it change
    /// while the device stays plugged in? A nameplate rating and a transfer
    /// window cannot. The buzzer mode can — this utility writes it, and so
    /// does the front panel of the UPS — so it is state, and state is polled.
    ///
    /// It was not, and that is the defect this list now guards. Read only at
    /// connect and inside the toggle handler, a single refused transfer during
    /// the write's confirming read left the mode unknown for the rest of the
    /// session: nothing else would re-read it, and the control that would have
    /// asked for a re-read was drawn from the value it had lost.
    #[test]
    fn measurements_are_still_polled() {
        let poll = poll_body();

        for usage in [
            "U_AUDIBLE_ALARM",
            "U_VOLTAGE",
            "U_PERCENT_LOAD",
            "U_ACTIVE_POWER",
            "U_APPARENT_POWER",
            "U_REMAINING_CAPACITY",
            "U_RUNTIME_TO_EMPTY",
            "U_AC_PRESENT",
            "U_INTERNAL_FAILURE",
            "U_OVERLOAD",
            "U_VOLTAGE_OUT_OF_RANGE",
            "U_FREQUENCY_OUT_OF_RANGE",
            "U_BOOST",
        ] {
            let named = poll.contains(&format!("{usage},")) || poll.contains(&format!("{usage})"));
            assert!(
                named,
                "{usage} is a live measurement and must be read every poll"
            );
        }
    }

    /// The device path is retained because the ambiguity log line uses it.
    ///
    /// It was previously a `#[allow(dead_code)]` field justified as being
    /// "for diagnostics" while no diagnostic read it — a decorated dead field
    /// that looked load-bearing. Either it feeds the one message where the
    /// choice of interface is arbitrary and the answer matters, or it should
    /// not be stored at all. This pins the first.
    ///
    /// **Structural, and temporarily so.** It reads source text rather than
    /// running the code, because `connect` needs a real HID handle. A text
    /// search cannot tell a correct log line from one in an arm that is never
    /// reached, so this is weaker than the property it stands for. The remedy
    /// is the one applied to `app::plan_emission`: lift the decision into a
    /// function that takes what it needs and returns what it decided, then
    /// assert on the value. Until that is done here, this is the only check
    /// there is.
    #[test]
    fn the_ambiguity_message_names_the_chosen_interface() {
        let connect = crate::testsupport::fn_body(include_str!("ups.rs"), "pub(crate) fn connect(");

        assert!(
            connect.contains("dev.path"),
            "the opened interface must be named, or 'using the first one found' is unactionable"
        );
        assert!(
            connect.contains("d.path.as_str()"),
            "the interfaces passed over must be listed too: the reader is judging the choice"
        );
    }

    /// The two nameplate fields must map to their own usages.
    ///
    /// This is the test that was missing when the panel showed
    /// `810 VA / 1350 W` for a 1350 VA / 810 W unit. The panel tests all
    /// passed, because their fixtures set `nominal_va` and `nominal_power_w`
    /// to the right numbers by hand — they proved the formatting, and the
    /// error was upstream in which usage each field was read from. Nothing
    /// checked the mapping itself.
    ///
    /// `HidP_GetUsageValue` resolves by usage, so the correct mapping is just
    /// the spec one: 0x43 is apparent power (VA), 0x44 is active power (W).
    /// The bit offsets printed by `ups-dump` are not byte positions and must
    /// not be used to second-guess this — 0x43 and 0x44 are listed at offsets
    /// 40 and 41 while both are 16 bits wide, which is only coherent as bit
    /// offsets into a packed report.
    #[test]
    fn nameplate_usages_are_not_crossed() {
        assert_eq!(
            U_CONFIG_APPARENT_POWER, 0x43,
            "apparent power (VA) is usage 0x43 in the HID Power Device spec"
        );
        assert_eq!(
            U_CONFIG_ACTIVE_POWER, 0x44,
            "active power (W) is usage 0x44 in the HID Power Device spec"
        );
        assert_ne!(
            U_CONFIG_APPARENT_POWER, U_CONFIG_ACTIVE_POWER,
            "the two must never resolve to the same usage"
        );
    }

    /// The physical scale is a property of the collection, not of the function
    /// that happens to read the field.
    ///
    /// Verified against a live capture from the device: raw `0x00e0` under
    /// Input is 224 V, raw `0x010f` under `PowerSummary` is 27.1 V.
    #[test]
    fn voltage_scale_follows_the_collection_not_the_call_site() {
        // Compared within a tolerance, not exactly. `271.0 * 0.1f32` and the
        // literal `27.1f32` agreeing bit for bit is a property of one rounding,
        // not of the scale being right, and a test that depends on it fails the
        // day the multiply is re-associated. A thousandth of a volt is three
        // orders below anything this program displays.
        const TOLERANCE: f32 = 0.001;
        let volts = |raw: u16, scope: Scope| f32::from(raw) * volts_per_unit(scope);

        assert!((volts(0x00e0, Scope::Input) - 224.0).abs() < TOLERANCE);
        assert!((volts(0x00e0, Scope::Output) - 224.0).abs() < TOLERANCE);
        assert!((volts(0x010f, Scope::PowerSummary) - 27.1).abs() < TOLERANCE);
        // The transfer thresholds are mains voltages the descriptor leaves
        // unscoped; reading them as tenths would put the transfer window an
        // order of magnitude below the supply it guards.
        assert_eq!(
            volts_per_unit(Scope::Unscoped),
            volts_per_unit(Scope::Input)
        );
    }

    /// Every scope Voltage is read from throttles under its own key.
    ///
    /// The three reads share `page:usage`, so with the scope left out of the
    /// key they shared one counter — and a real self-test log caught both
    /// consequences of that. A sibling scope succeeding in the same poll
    /// cleared the counter of the scope that had just failed, so the file said
    /// "readable again" one line under the failure and a second before the
    /// report answered; and a scope failing every poll never accumulated a
    /// count, so it was written out in full every poll instead of being
    /// throttled at 1, 2, 5, 10.
    ///
    /// Asserted against `VOLTAGE_SCOPES` rather than against three literals:
    /// the list is what `poll` reads, so a fourth collection added there is
    /// covered here without anyone remembering to come back.
    #[test]
    fn each_voltage_scope_has_its_own_throttle_key() {
        let keys: Vec<String> = VOLTAGE_SCOPES
            .iter()
            .map(|&scope| {
                ValueKey {
                    page: PAGE_POWER,
                    usage: U_VOLTAGE,
                    scope,
                }
                .to_string()
            })
            .collect();

        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            keys.len(),
            "two collections of one usage would share a throttle counter: {keys:?}"
        );
    }

    /// A key does not change with anything but the field it names.
    ///
    /// The failure line and the recovery line are counted under the same key,
    /// and that is what lets a success clear the count its own failures
    /// raised. Two calls for one field therefore have to agree exactly.
    #[test]
    fn a_key_is_the_same_string_every_time_it_is_built() {
        let of = |scope| {
            ValueKey {
                page: PAGE_POWER,
                usage: U_VOLTAGE,
                scope,
            }
            .to_string()
        };
        assert_eq!(of(Scope::Output), of(Scope::Output));
        assert_ne!(of(Scope::Output), of(Scope::Input));
    }
}
