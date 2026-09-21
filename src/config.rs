//! Portable configuration. Every path is derived from `current_exe()`, never
//! from the working directory, and nothing is written outside that directory.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::ini::Ini;
pub(crate) const POLL_MIN_MS: u32 = 1000;
pub(crate) const POLL_MAX_MS: u32 = 60_000;
pub(crate) const POLL_DEFAULT_MS: u32 = 3000;

/// Verbosity of the working log.
///
/// `Normal` is events only, nothing per poll: a UPS sitting on mains for a
/// week writes no lines at all.
///
/// `Debug` adds one category — the reason a value could not be read — and
/// nothing else. It exists for exactly one question: the panel shows a dash
/// where a number belongs, and in `Normal` the file says nothing about it,
/// because a failed field read is not an event about the UPS. The dash is
/// honest but silent about its cause, and the cause is what the reader needs:
/// a usage absent from the descriptor is a different fault from a control
/// transfer that failed, and neither is visible from the window.
///
/// It is not a general trace. Nothing is logged per poll while reads succeed,
/// so a working device in `Debug` is as quiet as in `Normal`, and turning it
/// on to catch an intermittent fault does not fill the file while waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LogLevel {
    #[default]
    Normal,
    Debug,
}

impl LogLevel {
    /// INI spelling. Lower-case and stable: this is written to a file that is
    /// read and edited by hand.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Debug => "debug",
        }
    }

    /// Reads the INI spelling back. Unknown text falls back to `Normal` rather
    /// than failing to load: a typo in the INI must not stop the utility, and
    /// the quieter setting is the safer default.
    ///
    /// Named `from_ini`, not `from_str`. An inherent `from_str` reads as
    /// `FromStr::from_str` — which returns a `Result` and lets the caller
    /// decide what a bad value means — while this one decides for them, and
    /// silently. A name that promises a fallible parse and performs a
    /// forgiving one is the sort of thing a caller only discovers when a typo
    /// in their INI turns out to have cost them their Debug level with no
    /// message anywhere.
    pub(crate) fn from_ini(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" => Self::Debug,
            _ => Self::Normal,
        }
    }

    /// Localisation key for the value shown in Settings.
    pub(crate) fn lang_key(self) -> crate::strings::Key {
        use crate::strings::Key;
        match self {
            Self::Normal => Key::SettingsLogNormal,
            Self::Debug => Key::SettingsLogDebug,
        }
    }

    /// The two levels, in the order the Settings control cycles them.
    pub(crate) const ALL: [LogLevel; 2] = [LogLevel::Normal, LogLevel::Debug];
}

/// "No position recorded yet."
///
/// Not `-1`: negative window coordinates are ordinary on a multi-monitor
/// desktop, where a display placed to the left of or above the primary one
/// occupies negative virtual-desktop space. Using a plausible coordinate as a
/// sentinel is what made the panel forget it had been moved to such a monitor.
/// `i32::MIN` cannot collide with a real position.
pub(crate) const POS_UNSET: i32 = i32::MIN;

/// The result of [`Config::load`]: the configuration, plus a warning if it
/// could not be written back.
///
/// The warning is deliberately not a load *error* — a missing file is normal
/// and simply yields defaults. It reports the one thing loading can fail at
/// that the user needs to know about: the config directory is not writable, so
/// a clamped value or a freshly created file did not reach disk and settings
/// will not stick. Naming it makes that meaning explicit at the call site,
/// where a bare `Option<Error>` alongside a `Config` read as "load failed".
pub(crate) struct Loaded {
    /// The configuration to use.
    pub config: Config,
    /// Set when the config could not be saved (unwritable directory); the
    /// caller logs it once the session log is open.
    pub warning: Option<Error>,
}

/// The settings as they stand, and the file they came from.
///
/// Everything the user can change: language, theme, poll interval, log
/// verbosity, twelve notification switches, the panel's last position, and
/// whether to start in the tray. Plain data with no behaviour beyond loading,
/// clamping and saving itself.
///
/// The two private fields are what make it a *file* rather than a struct that
/// happens to be persisted. `path` is where it lives; `raw` is the parsed INI
/// with every line the file contained, including keys this version does not
/// know and comments the user wrote. [`Config::save`] writes through `raw`, so
/// an unknown key survives a round trip instead of being deleted by the act of
/// changing an unrelated setting — which is what a save built from the typed
/// fields alone would do, silently, to a file the user had edited by hand.
///
/// Values are clamped on load rather than rejected: a poll interval outside
/// [`POLL_MIN_MS`]..=[`POLL_MAX_MS`] becomes the nearest allowed one, and the
/// clamped value is written back so the file agrees with what is running. A
/// configuration that cannot be parsed is not an error either — the defaults
/// are used and the reason goes to the log. Refusing to start over a bad
/// settings file would be the wrong trade for a utility whose job is to be
/// running when the power fails.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub language: String,
    pub poll_interval_ms: u32,
    pub theme: String,

    /// Verbosity of `ups-monitor.log`. See `LogLevel`.
    pub log_level: LogLevel,

    pub notifications_enabled: bool,
    pub on_power_failure: bool,
    pub on_power_restored: bool,
    pub on_low_battery: bool,
    pub on_device_fault: bool,
    /// Output overload. Its own switch rather than sharing `on_device_fault`:
    /// an overload is caused by what the user plugged in and is cleared by
    /// unplugging it, while an internal failure is the UPS itself breaking.
    /// One is actionable by the person reading the balloon, the other is not,
    /// and a single switch forced them to accept or mute both together.
    pub on_overload: bool,
    /// Mains voltage outside the transfer window.
    ///
    /// Its own switch, not shared with frequency. The two flags are different
    /// faults with different causes and different remedies: voltage outside
    /// the window is a sagging or surging supply, usually local and often
    /// fixable by moving the load off a shared circuit. Frequency outside
    /// tolerance is a generator running at the wrong speed or an inverter
    /// misbehaving, and no amount of rewiring the room addresses it. That
    /// this firmware reports no frequency *value* — only the flag — is a
    /// limit on how much detail each event carries, not a reason to merge two
    /// distinct conditions into one control.
    pub on_voltage_out_of_range: bool,
    /// Mains frequency outside tolerance.
    pub on_frequency_out_of_range: bool,
    /// Remaining runtime below the device's configured limit.
    pub on_runtime_limit: bool,
    /// Contact with the UPS lost.
    ///
    /// Separate from `on_connection_restored` because the two are not one
    /// event seen twice. Losing contact means the utility is no longer
    /// monitoring anything and is the one a user is least likely to mute;
    /// the restore is the reassurance, and someone who reconnects the device
    /// themselves already knows. Muting the pair together would force them
    /// to give up the alarm to be rid of the acknowledgement.
    pub on_connection_lost: bool,
    /// Contact with the UPS re-established.
    pub on_connection_restored: bool,
    /// AVR engaged.
    ///
    /// On by default, like every other event here. AVR is not a fault and
    /// asks nothing of the user — the UPS is correcting low mains by
    /// transformer tap without touching the battery — and an earlier revision
    /// shipped it muted on the reasoning that a popup per correction is noise
    /// that teaches people to dismiss notifications unread.
    ///
    /// That reasoning was overruled, and correctly: this is an engineering
    /// utility. Someone who installs it wants to see what the hardware is
    /// doing, and a tool that decides in advance which of its own
    /// observations are worth the user's attention is making a judgement
    /// about an installation it knows nothing about. Everything is reported;
    /// whoever finds a particular event noisy turns it off, which is what the
    /// switch is for.
    ///
    /// Two switches rather than one because the edges answer different
    /// questions: the start says the supply is sagging now, the end says it
    /// recovered. Someone watching a suspect circuit may want only the
    /// former.
    ///
    /// The setting never affects the log. Both edges reach
    /// `ups-monitor.log` whatever this is set to — see `app.rs::emit_events`.
    pub on_boost_started: bool,
    /// AVR disengaged. Separate from `on_boost_started`, on by default for
    /// the same reason.
    pub on_boost_ended: bool,

    pub vendor_id: u16,
    pub product_id: u16,

    pub panel_x: i32,
    pub panel_y: i32,
    /// Start in the tray rather than with the panel on screen.
    ///
    /// Default is true: a monitoring utility that opens a window on every
    /// login is intrusive, and the tray icon already carries the state. The
    /// device-missing case overrides this and still shows the panel, because
    /// an icon alone cannot explain a failure.
    pub start_minimized: bool,

    path: PathBuf,
    raw: Ini,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: "en".into(),
            poll_interval_ms: POLL_DEFAULT_MS,
            theme: "dark".into(),
            log_level: LogLevel::Normal,
            notifications_enabled: true,
            on_power_failure: true,
            on_power_restored: true,
            on_low_battery: true,
            on_device_fault: true,
            on_overload: true,
            on_voltage_out_of_range: true,
            on_frequency_out_of_range: true,
            on_runtime_limit: true,
            on_connection_lost: true,
            on_connection_restored: true,
            on_boost_started: true,
            on_boost_ended: true,
            vendor_id: 0x0764,
            product_id: 0x0601,
            start_minimized: true,
            panel_x: POS_UNSET,
            panel_y: POS_UNSET,
            path: PathBuf::new(),
            raw: Ini::default(),
        }
    }
}

/// The product's own name, and the last resort for every path derived from the
/// executable.
///
/// One literal. It stood in two files, spelled the same by hand, each the
/// fallback for a `current_exe()` that failed: a renamed copy would keep its
/// files together under the new name, but a copy whose own path could not be
/// read would have looked for `ups-monitor.ini` and written `ups-monitor.log`
/// only because the two spellings happened to match.
const DEFAULT_STEM: &str = "ups-monitor";

/// Directory containing the executable. All resources are relative to it.
pub(crate) fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The executable's file name without its extension: `ups-monitor.exe` ->
/// `ups-monitor`.
///
/// Derived rather than hardcoded so a renamed copy keeps its configuration and
/// its log beside it under the matching name — the portability rule, applied to
/// the name as well as to the directory.
pub(crate) fn exe_stem() -> String {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::file_stem)
        .map_or_else(
            || DEFAULT_STEM.to_owned(),
            |s| s.to_string_lossy().into_owned(),
        )
}

/// A file beside the executable, named after it: `sibling_file("ini")` is
/// `ups-monitor.ini` next to `ups-monitor.exe`.
///
/// `extension` carries no dot. Every file this utility owns is named this way,
/// so the rule lives here rather than once per file.
pub(crate) fn sibling_file(extension: &str) -> PathBuf {
    base_dir().join(format!("{}.{extension}", exe_stem()))
}
/// The twelve per-event switches, stated once.
///
/// Each row is a notifiable event kind and the field that gates its balloon.
/// The INI key is the field name, so there is no fourth list to keep in step.
/// These names used to be written out three times — in `notifies_for`, in the
/// reader and in the writer — and a switch missing from any one of them fails
/// differently: silently unmutable, silently unsaved, or silently reset on the
/// next load. Stating the rows once and emitting all three makes those
/// failures unrepresentable.
///
/// The declarations and the defaults stay written out: a macro cannot add
/// fields to a struct that has fifteen others, and those two lists are the
/// ones the compiler already checks — a missing field is an error at every
/// construction site.
macro_rules! event_switches {
    ($($kind:ident => $field:ident),+ $(,)?) => {
        /// Whether a balloon is shown for `kind`, per the per-event switches.
        ///
        /// The one place `EventKind` is mapped to the switch that gates it. It
        /// lives here, beside the fields it reads, rather than in `App`: the
        /// mapping is a fact about the configuration. Exhaustive with no wildcard,
        /// so a new event kind does not compile until it is given a switch here —
        /// the same guard the notification tests rely on. See the individual
        /// `on_*` fields for why each defaults on and why none shares a control.
        ///
        /// Every event has its own switch. There used to be four switches for
        /// twelve events, so muting one thing silenced another: overload rode on
        /// the device-fault switch, both mains-quality flags rode on the
        /// power-failure switch, the runtime limit rode on the low-battery switch,
        /// and the two connection events had no switch at all. The groupings were
        /// defensible one at a time and wrong in aggregate — a user who muted
        /// "switched to battery" during a storm also lost the brownout warnings,
        /// without being told and without any way to say otherwise. Voltage and
        /// frequency out of range were the last pair to be separated, on the
        /// grounds that this firmware reports no frequency *value*, only the flag;
        /// but that is a limit on the detail an event carries, not evidence that
        /// two events are one. A sagging supply is often local and fixable, a
        /// frequency fault is a generator or inverter problem and is not.
        ///
        /// This decides balloons and nothing else. Logging happens upstream, in
        /// `app::plan_emission`, and is unconditional: a user muting a
        /// notification does not unmake the event, and the log is the record of
        /// what the device did, not of what was shown.

        pub(crate) fn notifies_for(&self, kind: crate::notify::EventKind) -> bool {
            use crate::notify::EventKind as K;
            match kind {
                $(K::$kind => self.$field,)+
            }
        }

        /// Reads every switch, falling back to `d` for one the file omits.
        fn read_switches(&mut self, ini: &mut Fallbacks<'_>, d: &Config) {
            $(self.$field = ini.bool("notifications", stringify!($field), d.$field);)+
        }

        /// The INI key of every switch, in table order.
        ///
        /// Emitted so the test that checks the shipped template against the
        /// switches does not have to repeat their names — that list drifted
        /// exactly as easily as the three this macro replaced.
        #[cfg(test)]
        const SWITCH_KEYS: &'static [&'static str] = &[$(stringify!($field)),+];

        /// Writes every switch back, under the key it was read from.
        fn write_switches(&mut self) {
            $(self.raw.set("notifications", stringify!($field), bool_str(self.$field));)+
        }
    };
}

impl Config {
    /// Reads the configured language and nothing else, touching nothing.
    ///
    /// For the refused second instance, whose dialog is localized. `load`
    /// is unusable there: it creates the file when absent and rewrites it
    /// when a value needs clamping, and a second copy started from another
    /// directory has no file — so `load` would plant an INI in a directory
    /// where nothing is running. A launch that was refused must leave the
    /// disk exactly as it found it. A missing or unreadable file simply
    /// means the default language.
    pub(crate) fn peek_language() -> String {
        language_in(std::fs::read_to_string(sibling_file("ini")).ok().as_deref())
    }

    event_switches! {
        PowerFailure => on_power_failure,
        PowerRestored => on_power_restored,
        LowBattery => on_low_battery,
        DeviceFault => on_device_fault,
        Overload => on_overload,
        VoltageOutOfRange => on_voltage_out_of_range,
        FrequencyOutOfRange => on_frequency_out_of_range,
        RuntimeLimitExpired => on_runtime_limit,
        Disconnected => on_connection_lost,
        Reconnected => on_connection_restored,
        BoostStarted => on_boost_started,
        BoostEnded => on_boost_ended,
    }

    /// Loads the config, creating it with defaults when absent. A missing file
    /// is not an error; an unwritable directory is reported so the user learns
    /// why settings do not stick.
    pub(crate) fn load() -> Loaded {
        Self::load_from(sibling_file("ini"))
    }

    /// [`Config::load`], against a caller-chosen path.
    ///
    /// The path is a parameter for one reason: `save` is the only write this
    /// utility makes to disk, it has already had one defect fixed in it (a lost
    /// key spelling), and it was covered by no test at all — because the only
    /// way to reach it was to overwrite the real configuration file beside the
    /// running executable. `load` supplies the portable location; the round-trip
    /// test supplies a temporary directory and leaves the user's file alone.
    fn load_from(path: PathBuf) -> Loaded {
        let mut warning = None;

        let text = std::fs::read_to_string(&path).ok();
        let existed = text.is_some();
        let raw = Ini::parse(text.as_deref().unwrap_or(DEFAULT_CONFIG));

        let d = Config::default();
        // Read into locals before the struct is built, because `raw` is moved
        // into it and `Fallbacks` borrows `raw`. The borrow ends at the last
        // read, which is what lets the move follow.
        let mut ini = Fallbacks::new(&raw);

        // Text keys cannot fail to parse, so they do not go through
        // `Fallbacks`: every string is a value. Whether that string names a
        // language or a theme that exists is a separate question, asked and
        // logged where the answer is known — `app::resolve_locale` and
        // `app::resolve_theme` — because this module holds neither table.
        let language = raw
            .get("general", "language")
            .unwrap_or(&d.language)
            .to_owned();
        let theme = raw.get("general", "theme").unwrap_or(&d.theme).to_owned();
        // Infallible by construction: `from_ini` maps anything it does not
        // recognise onto `Normal`, so there is no `None` to report.
        let log_level = raw
            .get("general", "log_level")
            .map_or(d.log_level, LogLevel::from_ini);

        let poll_interval_ms = ini.u32("general", "poll_interval_ms", d.poll_interval_ms);

        let notifications_enabled = ini.bool("notifications", "enabled", d.notifications_enabled);

        let vendor_id = ini.u16("device", "vendor_id", d.vendor_id);
        let product_id = ini.u16("device", "product_id", d.product_id);
        let start_minimized = ini.bool("window", "start_minimized", d.start_minimized);
        let panel_x = ini.i32("window", "panel_x", d.panel_x);
        let panel_y = ini.i32("window", "panel_y", d.panel_y);

        // Every switch at once, from the table that also decides how they are
        // written. Read here, with the other fields, because the reader borrows
        // `raw` and `raw` moves into the struct below.
        let mut switches = d.clone();
        switches.read_switches(&mut ini, &d);

        let unreadable = ini.rewrite;

        let mut cfg = Config {
            language,
            poll_interval_ms,
            theme,
            log_level,

            notifications_enabled,

            vendor_id,
            product_id,
            panel_x,
            panel_y,
            start_minimized,

            path,
            raw,

            // The switches come from `switches` above, read as a group; naming
            // them here would be that list a second time.
            ..switches
        };

        // Out-of-range interval snaps to the nearest bound and is written back.
        let clamped = cfg.poll_interval_ms.clamp(POLL_MIN_MS, POLL_MAX_MS);
        // Three reasons to write, and they are one reason: what is on disk does
        // not match what the utility is running with. A file that does not
        // exist, a value outside its bounds, and a value that could not be read
        // at all all leave the two out of step, and `save` re-serialises from
        // the parsed config, so a single write settles any of them. Without the
        // third, the offending line stayed in the file for good — re-diagnosed
        // on every start and honoured on none.
        let needs_write = !existed || clamped != cfg.poll_interval_ms || unreadable;
        cfg.poll_interval_ms = clamped;

        if needs_write {
            if let Err(e) = cfg.save() {
                warning = Some(e);
            }
        }

        Loaded {
            config: cfg,
            warning,
        }
    }

    /// Writes the config back, preserving unknown keys and comments.
    ///
    /// # Errors
    ///
    /// [`Error::ConfigNotWritable`], carrying the path and the OS reason. The
    /// reason is kept because it separates causes that call for different
    /// answers — a read-only directory, a full disk, a file another process
    /// holds open, a removable drive pulled out — which a bare "not writable"
    /// renders identical to whoever is working out why their settings do not
    /// stick. Only the localised sentence with the path reaches the panel; the
    /// cause goes to the log.
    ///
    /// Not an error the caller must handle to stay correct: `load` records it
    /// as a warning and carries on with the in-memory config, so a launch from
    /// an unwritable directory still runs.
    pub(crate) fn save(&mut self) -> Result<()> {
        self.poll_interval_ms = self.poll_interval_ms.clamp(POLL_MIN_MS, POLL_MAX_MS);

        self.raw.set("general", "language", &self.language);
        self.raw.set(
            "general",
            "poll_interval_ms",
            &self.poll_interval_ms.to_string(),
        );
        self.raw.set("general", "theme", &self.theme);
        self.raw
            .set("general", "log_level", self.log_level.as_str());

        self.raw.set(
            "notifications",
            "enabled",
            bool_str(self.notifications_enabled),
        );
        self.write_switches();

        self.raw
            .set("device", "vendor_id", &format!("{:#06x}", self.vendor_id));
        self.raw
            .set("device", "product_id", &format!("{:#06x}", self.product_id));

        self.raw
            .set("window", "start_minimized", bool_str(self.start_minimized));
        // Written only once a real position exists. Serialising the sentinel
        // would put `-2147483648` in a file meant to be read and edited by
        // hand, and reading it back is only correct by accident.
        if self.panel_x != POS_UNSET && self.panel_y != POS_UNSET {
            self.raw.set("window", "panel_x", &self.panel_x.to_string());
            self.raw.set("window", "panel_y", &self.panel_y.to_string());
        }

        // Written to a sibling file and renamed over the target. A plain
        // overwrite truncates first and writes second, so power loss in
        // between — the one event this utility exists to monitor — leaves a
        // half-written config. `rename` replaces atomically on both NTFS and
        // the FAT variants a portable stick is likely to carry.
        //
        // The data is flushed to disk with `sync_all` *before* the rename, not
        // left to the OS to write back later. Without the flush the rename can
        // reach the disk before the tmp file's contents do: after power loss the
        // directory entry points at a file whose data never landed, which is an
        // empty or truncated config — precisely the corruption the atomic swap
        // was meant to prevent. The flush is what makes the guarantee real
        // rather than merely stated. The file is closed (dropped) before the
        // rename so no handle is open across it.
        let tmp = self.path.with_extension("ini.tmp");
        if let Err(e) = write_synced(&tmp, self.raw.to_text().as_bytes())
            .and_then(|()| std::fs::rename(&tmp, &self.path))
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::ConfigNotWritable {
                path: self.path.clone(),
                cause: e.to_string(),
            });
        }
        Ok(())
    }
}

/// Writes `bytes` to `path` and flushes them to the physical disk before
/// returning. The flush is the point: `std::fs::write` only hands the data to
/// the OS cache, which may reorder it after a later `rename`. `sync_all`
/// (fdatasync/FlushFileBuffers) forces the data down first, so the rename that
/// follows can never expose a file whose contents have not been committed.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// The language named by an INI text, or the default. Pure, so the no-write
/// guarantee of `peek_language` is testable without touching the filesystem.
fn language_in(text: Option<&str>) -> String {
    text.and_then(|t| Ini::parse(t).get("general", "language").map(str::to_owned))
        .unwrap_or_else(|| "en".into())
}

/// Reads typed values from the INI, making every fallback visible.
///
/// The rule was already stated in this project — "a typo in the INI is
/// ordinary input, but the fallback must be visible" — and already applied, to
/// two keys out of twenty. `theme` and `language` log the code they could not
/// place; everything else went through `get_*(..).unwrap_or(default)`, where a
/// `None` from an unparsable value is indistinguishable from a `None` from an
/// absent key. `poll_interval_ms = abc` therefore left no trace anywhere: the
/// utility ran at 3000 ms, the file kept `abc`, the clamp saw no change so
/// nothing was written back, and the next start repeated the whole thing.
///
/// Two facts are separated here that `unwrap_or` conflates:
///
/// * **absent** — the file does not mention the setting. Not a mistake, and
///   not logged: most keys are absent in a hand-written file.
/// * **present and unreadable** — somebody wrote something and it did not
///   parse. Logged with the offending text, and `rewrite` is raised so `load`
///   writes the normalised value back. The write-back is what stops the same
///   line being diagnosed afresh on every start.
///
/// The reporting lives here, in one place, rather than at twenty call sites,
/// which is what makes "every key behaves the same way" a property of the type
/// instead of twenty things to remember.
struct Fallbacks<'a> {
    ini: &'a Ini,
    /// True once any key was present and could not be read.
    rewrite: bool,
}

impl<'a> Fallbacks<'a> {
    fn new(ini: &'a Ini) -> Self {
        Self {
            ini,
            rewrite: false,
        }
    }

    fn bool(&mut self, section: &str, key: &str, default: bool) -> bool {
        let parsed = self.ini.get_bool(section, key);
        self.resolve(section, key, parsed, default)
    }

    fn u32(&mut self, section: &str, key: &str, default: u32) -> u32 {
        let parsed = self.ini.get_u32(section, key);
        self.resolve(section, key, parsed, default)
    }

    fn i32(&mut self, section: &str, key: &str, default: i32) -> i32 {
        let parsed = self.ini.get_i32(section, key);
        self.resolve(section, key, parsed, default)
    }

    /// A 16-bit id, refusing anything that does not fit.
    ///
    /// `try_from`, not `as`: a hand-edited id above 0xFFFF must fall back
    /// rather than silently truncate to whatever its low 16 bits happen to be
    /// — a wrong device searched for quietly is worse than a typo rejected.
    /// Out of range and unparsable are one case to the reader, and now to the
    /// log as well: both say the value was not usable and name it.
    fn u16(&mut self, section: &str, key: &str, default: u16) -> u16 {
        let parsed = self
            .ini
            .get_u32(section, key)
            .and_then(|v| u16::try_from(v).ok());
        self.resolve(section, key, parsed, default)
    }

    /// The shared half: report the discrepancy, then fall back.
    ///
    /// `default` is printed, so the line says what the utility is actually
    /// running with rather than only what it refused. The values are all
    /// `Display` in their INI spelling — `bool` prints `true`/`false`, which
    /// is exactly what `save` writes back.
    fn resolve<T: std::fmt::Display>(
        &mut self,
        section: &str,
        key: &str,
        parsed: Option<T>,
        default: T,
    ) -> T {
        if let Some(value) = parsed {
            return value;
        }
        if let Some(raw) = self.ini.get(section, key) {
            crate::evlog::event(
                crate::evlog::Cat::Config,
                &format!("unreadable {section}.{key} '{raw}', falling back to {default}"),
            );
            self.rewrite = true;
        }
        default
    }
}

fn bool_str(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
}

const DEFAULT_CONFIG: &str = r"[general]
language = en
poll_interval_ms = 3000
theme = dark
log_level = normal

[notifications]
enabled = true
on_power_failure = true
on_power_restored = true
on_low_battery = true
on_device_fault = true
on_overload = true
on_voltage_out_of_range = true
on_frequency_out_of_range = true
on_runtime_limit = true
on_connection_lost = true
on_connection_restored = true
on_boost_started = true
on_boost_ended = true

[device]
vendor_id = 0x0764
product_id = 0x0601

[window]
start_minimized = true
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;

    /// A key that is present and unreadable is reported, not swallowed.
    ///
    /// The bug this pins: `poll_interval_ms = abc` produced `None`, the
    /// `unwrap_or` behind it produced 3000, the clamp then saw no change and
    /// wrote nothing back. The file kept `abc` for good, the utility ran at
    /// the default, and the log held not one line about either.
    #[test]
    fn an_unreadable_value_falls_back_and_asks_for_a_rewrite() {
        let ini = Ini::parse("[general]\npoll_interval_ms = abc\n");
        let mut f = Fallbacks::new(&ini);
        assert_eq!(
            f.u32("general", "poll_interval_ms", POLL_DEFAULT_MS),
            POLL_DEFAULT_MS
        );
        assert!(
            f.rewrite,
            "an unreadable value must be normalised back into the file"
        );
    }

    /// An id that parses but does not fit is the same case to the reader.
    ///
    /// `try_from` rather than `as`, so a hand-edited id above 0xFFFF falls
    /// back instead of truncating to its low 16 bits and searching quietly for
    /// the wrong device.
    #[test]
    fn an_out_of_range_id_falls_back_like_an_unreadable_one() {
        let ini = Ini::parse("[device]\nvendor_id = 0x10764\n");
        let mut f = Fallbacks::new(&ini);
        assert_eq!(f.u16("device", "vendor_id", 0x0764), 0x0764);
        assert!(f.rewrite);
    }

    /// The one write this utility makes to disk, taken round the full circle.
    ///
    /// `save` was covered by nothing, because reaching it meant overwriting the
    /// real configuration file beside the executable. It is also the place a
    /// defect has already been found once — the user's spelling of a key was
    /// lost on rewrite — and everything it promises is a property of the file
    /// afterwards rather than of a returned value, so only a real file can
    /// answer for it.
    ///
    /// Four promises are checked in one pass, because they are four properties
    /// of a single write and splitting them would mean writing the file four
    /// times to ask about it four times:
    ///
    ///  * unknown keys and comments survive — the file is hand-edited, and a
    ///    utility that silently drops what it does not understand cannot be
    ///    trusted with it;
    ///  * the user's spelling of a key is preserved, since only the value is
    ///    replaced;
    ///  * nothing is left of the atomic swap: no `.ini.tmp` beside the file;
    ///  * what `save` wrote is what `load` reads back.
    #[test]
    fn saving_and_reloading_preserves_the_file_and_the_settings() {
        let dir = TempDir::new("roundtrip");
        let path = dir.path().join("ups-monitor.ini");
        // CRLF, because this file is edited in Notepad on Windows; a tight
        // pair, an unknown key and a comment, because all three have to come
        // back out untouched.
        std::fs::write(
            &path,
            "; hand written
             [general]
             Poll_Interval_MS=1500
             something_we_do_not_know = keep me
             [notifications]
             on_overload = false
",
        )
        .expect("the temp directory must be writable");

        let loaded = Config::load_from(path.clone());
        assert!(loaded.warning.is_none(), "an existing file needs no repair");
        let mut cfg = loaded.config;
        assert_eq!(cfg.poll_interval_ms, 1500, "a tight pair must still parse");
        assert!(
            !cfg.on_overload,
            "the file's value must win over the default"
        );

        cfg.poll_interval_ms = 5000;
        cfg.on_overload = true;
        cfg.language = "ru".to_owned();
        cfg.save().expect("the temp directory must be writable");

        let text = std::fs::read_to_string(&path).expect("the file must still be there");
        assert!(
            text.contains("; hand written"),
            "a comment must survive a rewrite: {text}"
        );
        assert!(
            text.contains("something_we_do_not_know"),
            "an unknown key must survive a rewrite: {text}"
        );
        assert!(
            text.contains("Poll_Interval_MS"),
            "the user's spelling of a key is theirs, not ours: {text}"
        );
        assert!(
            !path.with_extension("ini.tmp").exists(),
            "the atomic swap must leave nothing behind"
        );

        let again = Config::load_from(path).config;
        assert_eq!(again.poll_interval_ms, 5000);
        assert!(again.on_overload);
        assert_eq!(again.language, "ru");
    }

    /// A value that cannot be read is normalised back into the file.
    ///
    /// The other half of the fallback rule: reporting the discrepancy is no use
    /// if the offending line stays on disk to be re-diagnosed at every start
    /// and honoured at none. `load` asks for the write; this is the proof it
    /// happens, and that the value written is the one actually in force.
    #[test]
    fn an_unreadable_value_is_repaired_on_disk() {
        let dir = TempDir::new("repair");
        let path = dir.path().join("ups-monitor.ini");
        std::fs::write(
            &path,
            "[general]
poll_interval_ms = abc
",
        )
        .expect("the temp directory must be writable");

        let cfg = Config::load_from(path.clone()).config;
        assert_eq!(cfg.poll_interval_ms, POLL_DEFAULT_MS);

        let text = std::fs::read_to_string(&path).expect("the file must still be there");
        assert!(
            !text.contains("abc"),
            "the unreadable value must not be left in the file: {text}"
        );
        assert!(
            text.contains(&POLL_DEFAULT_MS.to_string()),
            "the value actually in force must be what the file says: {text}"
        );
    }

    /// An absent key is not a mistake and must not provoke either a line or a
    /// write. Most keys are absent in a hand-written file; treating that as a
    /// fault would rewrite the user's file on every start and fill the log
    /// with twenty lines saying nothing happened.
    #[test]
    fn an_absent_key_is_silent() {
        let ini = Ini::parse("[general]\n");
        let mut f = Fallbacks::new(&ini);
        assert!(f.bool("notifications", "enabled", true));
        assert_eq!(f.i32("window", "panel_x", POS_UNSET), POS_UNSET);
        assert!(!f.rewrite, "an absent key is ordinary, not a fallback");
    }

    /// The INI holds user choices, never device facts.
    ///
    /// `nominal_power_w` used to be written here. It is reported by the UPS
    /// itself, so the file was caching a fact about hardware that can be
    /// unplugged and replaced — and after a swap the stale figure kept being
    /// served as though it were current. Anything the device tells us must be
    /// read from the device every time.
    ///
    /// The same reasoning bars the other identity fields, which is why the
    /// list below is checked rather than just the one that went wrong.
    #[test]
    fn no_device_reported_values_are_persisted() {
        for key in [
            "nominal_power_w",
            "model",
            "serial",
            "firmware",
            "manufacturer",
            "chemistry",
            "battery_voltage",
        ] {
            assert!(
                !DEFAULT_CONFIG.contains(key),
                "{key} is reported by the UPS and must not be cached in the INI"
            );
        }
    }

    /// `vendor_id` and `product_id` are the exception, and deliberately so: they
    /// are how the device is *found*, not something it tells us once found.
    #[test]
    fn device_selection_ids_are_still_configurable() {
        assert!(DEFAULT_CONFIG.contains("vendor_id"));
        assert!(DEFAULT_CONFIG.contains("product_id"));
    }

    /// An unknown or misspelt level loads as `Normal` rather than failing.
    ///
    /// The INI is meant to be hand-edited, so a typo is expected input. The
    /// quieter level is the safe fallback: guessing `Debug` from a word we
    /// did not recognise would start writing diagnostics nobody asked for.
    #[test]
    fn an_unrecognised_log_level_falls_back_to_normal() {
        assert_eq!(LogLevel::from_ini("debug"), LogLevel::Debug);
        assert_eq!(LogLevel::from_ini("DEBUG"), LogLevel::Debug);
        assert_eq!(LogLevel::from_ini("  Debug "), LogLevel::Debug);
        assert_eq!(LogLevel::from_ini("normal"), LogLevel::Normal);
        for bad in ["", "verbose", "trace", "1", "yes"] {
            assert_eq!(
                LogLevel::from_ini(bad),
                LogLevel::Normal,
                "{bad:?} must not silently enable diagnostics"
            );
        }
    }

    /// What is written must read back as the same level, or a saved choice
    /// quietly reverts on the next start.
    #[test]
    fn log_level_round_trips_through_its_ini_spelling() {
        for level in LogLevel::ALL {
            assert_eq!(LogLevel::from_ini(level.as_str()), level);
        }
    }

    /// Every event the user can be notified about has a key in the file.
    ///
    /// The gap this closes: four flags reached `EventKind` and the panel
    /// while the only switches were the original four, so overload rode on
    /// the device-fault switch, both mains-quality flags rode on the
    /// power-failure switch, the runtime limit rode on the low-battery
    /// switch, and the connection events had no switch at all. Muting one
    /// event silenced another the user never asked to mute.
    #[test]
    fn every_configurable_event_has_a_key_in_the_default_ini() {
        for key in Config::SWITCH_KEYS {
            assert!(
                DEFAULT_CONFIG.contains(key),
                "{key} is offered in Settings and must be persisted"
            );
        }
    }

    /// Every notification is on out of the box, with no exceptions.
    ///
    /// This is an engineering utility: it reports what the hardware does, and
    /// it does not decide in advance which of its own observations deserve
    /// the user's attention. A tool that ships parts of itself muted is
    /// making a judgement about an installation it knows nothing about, and
    /// the user finds out what it chose to hide only by reading the source or
    /// noticing an event that never arrived.
    ///
    /// AVR was the exception until this rule replaced it, on the argument
    /// that a popup per correction is noise. It can be — on one installation,
    /// judged by its owner, who now turns it off. That is what the switch is
    /// for, and it is the user's call rather than the default's.
    ///
    /// The defaults concern balloons only. Every event is logged regardless,
    /// which `app::tests::every_event_is_logged_whatever_the_switches_say`
    /// pins.
    #[test]
    fn every_notification_is_enabled_by_default() {
        let d = Config::default();
        assert!(d.notifications_enabled, "the master switch must default on");
        for kind in crate::notify::EventKind::ALL {
            assert!(
                d.notifies_for(kind),
                "{kind:?} must default on: nothing ships muted"
            );
        }

        // And the generated file says so, so a user reading the INI sees the
        // same thing the code does.
        for line in DEFAULT_CONFIG.lines() {
            let line = line.trim();
            if line.starts_with("on_") || line == "enabled = true" {
                assert!(
                    line.ends_with("= true"),
                    "the shipped INI must enable everything, found: {line}"
                );
            }
        }
    }

    /// The refused second instance reads the language and nothing else, so
    /// the pure helper behind `peek_language` must cope with every shape of
    /// input without inventing values.
    #[test]
    fn peeking_the_language_never_needs_a_valid_file() {
        assert_eq!(language_in(None), "en", "no file means the default");
        assert_eq!(language_in(Some("")), "en", "an empty file too");
        assert_eq!(language_in(Some("[general]\nlanguage = ru\n")), "ru");
        assert_eq!(
            language_in(Some("garbage that is not ini")),
            "en",
            "unparseable content must not stop the dialog"
        );
    }

    /// A device id above 16 bits is a typo, not a request for its low half.
    #[test]
    fn oversized_device_ids_fall_back_rather_than_truncate() {
        let ini = crate::ini::Ini::parse("[device]\nvendor_id = 0x10764\n");
        let v = ini
            .get_u32("device", "vendor_id")
            .and_then(|v| u16::try_from(v).ok());
        assert_eq!(v, None, "0x10764 must not silently become 0x0764");
    }

    /// A first run leaves a file behind; a value outside its bounds is
    /// corrected on disk rather than only in memory.
    ///
    /// Both halves are one rule — what is on disk must match what the utility
    /// is running with — and both are invisible from the returned `Config`,
    /// which holds the right numbers either way. The failure they guard
    /// against is the file staying wrong: a first run that writes nothing
    /// gives the user no file to edit, and an out-of-range interval left in
    /// place is re-diagnosed at every start and honoured at none.
    #[test]
    fn what_is_on_disk_matches_what_is_running() {
        let dir = TempDir::new("firstrun");
        let fresh = dir.path().join("ups-monitor.ini");
        assert!(!fresh.exists());
        let loaded = Config::load_from(fresh.clone());
        assert!(loaded.warning.is_none(), "a writable directory needs none");
        assert!(
            fresh.exists(),
            "a first run must leave a file the user can edit"
        );
        assert_eq!(loaded.config.poll_interval_ms, POLL_DEFAULT_MS);

        let out_of_range = dir.path().join("clamped.ini");
        std::fs::write(&out_of_range, "[general]\npoll_interval_ms = 100\n")
            .expect("the temp directory must be writable");
        let cfg = Config::load_from(out_of_range.clone()).config;
        assert_eq!(cfg.poll_interval_ms, POLL_MIN_MS, "the bound is in force");
        let text = std::fs::read_to_string(&out_of_range).expect("the file must still be there");
        assert!(
            text.contains(&format!("poll_interval_ms = {POLL_MIN_MS}")),
            "the corrected value must reach the file: {text}"
        );
    }

    /// The panel position is written only when both coordinates are real.
    ///
    /// [`POS_UNSET`] is `i32::MIN`, and half a position is not a position: the
    /// sentinel reaching the file would put `-2147483648` in front of somebody
    /// editing it in Notepad, and reading it back afterwards only works
    /// because the same sentinel happens to survive the round trip. Each of
    /// the three combinations answers a different way of getting the guard
    /// wrong, which is why all three are asked rather than just the one that
    /// writes.
    #[test]
    fn a_half_known_panel_position_is_not_written() {
        let dir = TempDir::new("panelpos");
        let saved_with = |name: &str, x: i32, y: i32| {
            let path = dir.path().join(name);
            let mut cfg = Config::load_from(path.clone()).config;
            cfg.panel_x = x;
            cfg.panel_y = y;
            cfg.save().expect("the temp directory must be writable");
            std::fs::read_to_string(&path).expect("the file must still be there")
        };

        let both = saved_with("both.ini", 100, 200);
        assert!(both.contains("panel_x = 100"), "{both}");
        assert!(both.contains("panel_y = 200"), "{both}");

        for (name, x, y) in [("no-y.ini", 100, POS_UNSET), ("no-x.ini", POS_UNSET, 200)] {
            let text = saved_with(name, x, y);
            assert!(
                !text.contains("panel_x") && !text.contains("panel_y"),
                "half a position is not a position: {text}"
            );
        }
    }

    /// Every resource this utility opens is named relative to the executable,
    /// so the directory it resolves against must be an absolute one that holds
    /// the executable.
    ///
    /// An empty or relative answer here is not a wrong directory but a moving
    /// one: relative paths resolve against the process's working directory,
    /// which for a tray utility started from a shortcut, a scheduled task or a
    /// startup folder is whatever the launcher happened to be in. The
    /// configuration would then be read from one place on one start and
    /// another on the next, with nothing to say so.
    #[test]
    fn resources_resolve_beside_the_executable() {
        let exe = std::env::current_exe().expect("a running test has a path");
        let base = base_dir();
        assert!(
            base.is_absolute(),
            "a relative base directory follows the working directory: {base:?}"
        );
        assert!(base.is_dir(), "the base directory must exist: {base:?}");
        assert!(
            exe.starts_with(&base),
            "the base directory must be the one holding the executable: {base:?} / {exe:?}"
        );
    }

    /// The generated file carries settings, not prose.
    ///
    /// Explanations belong on the code that implements the behaviour, where
    /// they stay correct when it changes. In the INI they are a second copy
    /// that silently rots, and they are shipped to every user whether or not
    /// the paragraph applies to them.
    #[test]
    fn the_generated_ini_carries_no_commentary() {
        for line in DEFAULT_CONFIG.lines() {
            let line = line.trim();
            assert!(
                !line.starts_with(';') && !line.starts_with('#'),
                "the emitted INI must contain no comment lines, found: {line}"
            );
        }
    }
}
