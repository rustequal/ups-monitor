//! The string schema: one variant per translatable string, and the dotted
//! names that identify them.
//!
//! Separate from [`languages`](super::languages) because this is what changes
//! when a *string* is added — one line in the table below, and one line in each
//! of the twenty-four language rows next door. Keeping the schema out of the
//! data means that first half of the edit is a diff of a few lines rather than
//! a diff inside a three-thousand-line table.
//!
//! # One list, four things generated from it
//!
//! Everything here comes out of the `declare_keys!` table below:
//! the [`Key`] enum, the dotted-name array, the list of every variant, and the
//! [`Strings`] struct a language fills in. They used to be four hand-written
//! lists in the same order, and "in the same order" was maintained by reading.
//!
//! That mattered because of what the data next door looks like. A language was
//! a bare array of a hundred and forty-one string literals, correct only if
//! every one sat at the index its meaning belongs to. A *missing* string was
//! caught by the array's length; a **swapped pair** was caught by nothing —
//! not by the compiler, and not by
//! `every_language_answers_every_key`, which checks that a string is present
//! and non-empty, not that it means what its position claims. A translation
//! whose words for "input" and "output" sat at each other's index would have
//! compiled, passed the suite and shipped, mislabelling the two voltage rows
//! the utility exists to show.
//!
//! With [`Strings`] a translation is a struct literal with named fields. A
//! missing one is a compile error that names it, a duplicate is a compile
//! error, and there is no order to get wrong: the mapping from field to index
//! is written once, in the generated `into_array`, from the same table as the
//! enum.

use super::KEY_COUNT;

/// Generates the schema from one table of `Variant field_name "dotted.name"`.
///
/// The three columns are on one line each on purpose. They are three spellings
/// of one key — the enum variant a caller names, the struct field a translator
/// fills, and the identifier that appears in diagnostics — and the only way to
/// make them disagree is to mistype one of the three while looking at the other
/// two. Split across separate lists, as they were, they could drift a hundred
/// lines apart with nothing to notice.
///
/// The discriminants are the declaration order and are no longer written out.
/// `#[repr(usize)]` numbers the variants from zero in the order given, which is
/// exactly the index each one needs, so the old `= 0` … `= 140` were a
/// hand-maintained copy of something the compiler already knew — a
/// hundred-and-forty-one-line sequence that had to be renumbered by hand to
/// insert anything, and that nothing would have checked if it had been
/// renumbered wrongly.
macro_rules! declare_keys {
    ($($variant:ident $field:ident $name:literal,)*) => {
        /// Every translatable string, as a compile-time key.
        ///
        /// The canonical identifier for a UI string: `Locale::t` takes a `Key`,
        /// not a `&str`, so a mistyped or removed key is a compile error rather
        /// than a `"???"` discovered by eye in the running window.
        /// `#[repr(usize)]` makes the variant *its own* index into every
        /// language's array, so a lookup is `strings[key as usize]` with no
        /// search. Variants are appended, never inserted, because the
        /// discriminant is the index — and every list derived from this one is
        /// generated beside it, so appending is a one-line edit here.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(usize)]
        pub(crate) enum Key {
            $(
                #[doc = $name]
                $variant,
            )*
        }

        /// Dotted names of the [`Key`] variants, in the same order.
        ///
        /// Test-only: it names a key in a failure message and lets a test walk
        /// the whole table. The program itself identifies every string by its
        /// [`Key`] and never needs the string form, so `#[cfg(test)]` states
        /// what this is rather than carrying it as unused production code.
        #[cfg(test)]
        pub(crate) const KEYS: [&str; KEY_COUNT] = [$($name,)*];

        impl Key {
            /// The dotted name of this key, e.g. `"panel.title"`.
            #[cfg(test)]
            pub(crate) fn label(self) -> &'static str {
                // The table is generated from the same list as the enum, so
                // the discriminant is always in it; `get` is how that is said
                // without a subscript, and the fallback names the failure
                // rather than aborting a test run with a panic from inside a
                // macro expansion.
                KEYS.get(self as usize).copied().unwrap_or("<unknown key>")
            }

            /// Every variant, in declaration order, for tests that must
            /// exercise the whole table.
            #[cfg(test)]
            pub(crate) const ALL: [Key; KEY_COUNT] = [$(Key::$variant,)*];
        }

        /// One language's translations, by name rather than by position.
        ///
        /// This is what a translation is written as. Rust's own rules then
        /// supply the guarantees the bare array could not: a field left out is
        /// a compile error that names it, a field given twice is a compile
        /// error, and the order the fields are written in does not matter
        /// because there is no order — the struct is converted to the array in
        /// one place, generated from the same table as `Key`.
        ///
        /// The cost is that a translation reads as `field: "text"` instead of
        /// `"text", // field`, which is three characters and the difference
        /// between a comment nobody has to keep true and a name the compiler
        /// checks.
        pub(crate) struct Strings {
            $(
                #[doc = $name]
                pub $field: &'static str,
            )*
        }

        impl Strings {
            /// The translations as the flat array a lookup indexes into.
            ///
            /// `const`, so it runs while the table is being built and the
            /// shipped binary holds only the array; the struct exists at
            /// compile time and nowhere else.
            pub(crate) const fn into_array(self) -> [&'static str; KEY_COUNT] {
                [$(self.$field,)*]
            }
        }
    };
}

declare_keys! {
    MenuPanel                      menu_panel                          "menu.panel",
    MenuSettings                   menu_settings                       "menu.settings",
    MenuExit                       menu_exit                           "menu.exit",
    PanelTitle                     panel_title                         "panel.title",
    PanelGroupInput                panel_group_input                   "panel.group_input",
    PanelGroupOutput               panel_group_output                  "panel.group_output",
    PanelGroupBattery              panel_group_battery                 "panel.group_battery",
    PanelGroupStatus               panel_group_status                  "panel.group_status",
    PanelGroupDevice               panel_group_device                  "panel.group_device",
    PanelInputVoltage              panel_input_voltage                 "panel.input_voltage",
    PanelInputNominal              panel_input_nominal                 "panel.input_nominal",
    PanelTransferWindow            panel_transfer_window               "panel.transfer_window",
    PanelOutputVoltage             panel_output_voltage                "panel.output_voltage",
    PanelLoadPercent               panel_load_percent                  "panel.load_percent",
    PanelLoadWatts                 panel_load_watts                    "panel.load_watts",
    PanelLoadVa                    panel_load_va                       "panel.load_va",
    PanelCharge                    panel_charge                        "panel.charge",
    PanelBatteryVoltage            panel_battery_voltage               "panel.battery_voltage",
    PanelBatteryNominal            panel_battery_nominal               "panel.battery_nominal",
    PanelRuntime                   panel_runtime                       "panel.runtime",
    PanelChargeState               panel_charge_state                  "panel.charge_state",
    PanelCapacityLimit             panel_capacity_limit                "panel.capacity_limit",
    PanelWarningCapacity           panel_warning_capacity              "panel.warning_capacity",
    PanelRuntimeLimit              panel_runtime_limit                 "panel.runtime_limit",
    PanelState                     panel_state                         "panel.state",
    PanelModel                     panel_model                         "panel.model",
    PanelFirmware                  panel_firmware                      "panel.firmware",
    PanelSerial                    panel_serial                        "panel.serial",
    PanelManufacturer              panel_manufacturer                  "panel.manufacturer",
    PanelChemistry                 panel_chemistry                     "panel.chemistry",
    PanelNominalPower              panel_nominal_power                 "panel.nominal_power",
    PanelLastTest                  panel_last_test                     "panel.last_test",
    PanelBeeper                    panel_beeper                        "panel.beeper",
    StateOnline                    state_online                        "state.online",
    StateOnBattery                 state_on_battery                    "state.on_battery",
    StateDisconnected              state_disconnected                  "state.disconnected",
    StateCharging                  state_charging                      "state.charging",
    StateDischarging               state_discharging                   "state.discharging",
    StateFullyCharged              state_fully_charged                 "state.fully_charged",
    StateIdle                      state_idle                          "state.idle",
    FlagLowBattery                 flag_low_battery                    "flag.low_battery",
    FlagInternalFailure            flag_internal_failure               "flag.internal_failure",
    FlagOverload                   flag_overload                       "flag.overload",
    FlagVoltageOutOfRange          flag_voltage_out_of_range           "flag.voltage_out_of_range",
    FlagFrequencyOutOfRange        flag_frequency_out_of_range         "flag.frequency_out_of_range",
    FlagRuntimeLimitExpired        flag_runtime_limit_expired          "flag.runtime_limit_expired",
    FlagBoost                      flag_boost                          "flag.boost",
    TestTestPassed                 test_test_passed                    "test.test_passed",
    TestTestPassedWarning          test_test_passed_warning            "test.test_passed_warning",
    TestTestError                  test_test_error                     "test.test_error",
    TestTestAborted                test_test_aborted                   "test.test_aborted",
    TestTestInProgress             test_test_in_progress               "test.test_in_progress",
    TestTestNotRun                 test_test_not_run                   "test.test_not_run",
    TestTestScheduled              test_test_scheduled                 "test.test_scheduled",
    TestTestUnknown                test_test_unknown                   "test.test_unknown",
    BeeperDisabled                 beeper_disabled                     "beeper.disabled",
    BeeperEnabled                  beeper_enabled                      "beeper.enabled",
    BeeperMuted                    beeper_muted                        "beeper.muted",
    BeeperUnsupported              beeper_unsupported                  "beeper.unsupported",
    BeeperEnable                   beeper_enable                       "beeper.enable",
    BeeperDisable                  beeper_disable                      "beeper.disable",
    SettingsStartMinimized         settings_start_minimized            "settings.start_minimized",
    SettingsTitle                  settings_title                      "settings.title",
    SettingsPollInterval           settings_poll_interval              "settings.poll_interval",
    SettingsNotifications          settings_notifications              "settings.notifications",
    SettingsOnPowerFailure         settings_on_power_failure           "settings.on_power_failure",
    SettingsOnPowerRestored        settings_on_power_restored          "settings.on_power_restored",
    SettingsOnLowBattery           settings_on_low_battery             "settings.on_low_battery",
    SettingsOnDeviceFault          settings_on_device_fault            "settings.on_device_fault",
    SettingsOnOverload             settings_on_overload                "settings.on_overload",
    SettingsOnVoltageOutOfRange    settings_on_voltage_out_of_range    "settings.on_voltage_out_of_range",
    SettingsOnFrequencyOutOfRange  settings_on_frequency_out_of_range  "settings.on_frequency_out_of_range",
    SettingsOnRuntimeLimit         settings_on_runtime_limit           "settings.on_runtime_limit",
    SettingsOnBoostStarted         settings_on_boost_started           "settings.on_boost_started",
    SettingsOnBoostEnded           settings_on_boost_ended             "settings.on_boost_ended",
    SettingsOnConnectionLost       settings_on_connection_lost         "settings.on_connection_lost",
    SettingsOnConnectionRestored   settings_on_connection_restored     "settings.on_connection_restored",
    SettingsLogLevel               settings_log_level                  "settings.log_level",
    SettingsLogNormal              settings_log_normal                 "settings.log_normal",
    SettingsLogDebug               settings_log_debug                  "settings.log_debug",
    SettingsTheme                  settings_theme                      "settings.theme",
    SettingsLanguage               settings_language                   "settings.language",
    SettingsOk                     settings_ok                         "settings.ok",
    SettingsCancel                 settings_cancel                     "settings.cancel",
    SettingsInvalidInterval        settings_invalid_interval           "settings.invalid_interval",
    NotifyPowerFailureTitle        notify_power_failure_title          "notify.power_failure_title",
    NotifyPowerFailureBody         notify_power_failure_body           "notify.power_failure_body",
    NotifyPowerRestoredTitle       notify_power_restored_title         "notify.power_restored_title",
    NotifyPowerRestoredBody        notify_power_restored_body          "notify.power_restored_body",
    NotifyLowBatteryTitle          notify_low_battery_title            "notify.low_battery_title",
    NotifyLowBatteryBody           notify_low_battery_body             "notify.low_battery_body",
    NotifyDeviceFaultTitle         notify_device_fault_title           "notify.device_fault_title",
    NotifyDeviceFaultBody          notify_device_fault_body            "notify.device_fault_body",
    NotifyOverloadTitle            notify_overload_title               "notify.overload_title",
    NotifyOverloadBody             notify_overload_body                "notify.overload_body",
    NotifyDisconnectedTitle        notify_disconnected_title           "notify.disconnected_title",
    NotifyDisconnectedBody         notify_disconnected_body            "notify.disconnected_body",
    NotifyReconnectedTitle         notify_reconnected_title            "notify.reconnected_title",
    NotifyReconnectedBody          notify_reconnected_body             "notify.reconnected_body",
    NotifyVoltageOutOfRangeTitle   notify_voltage_out_of_range_title   "notify.voltage_out_of_range_title",
    NotifyVoltageOutOfRangeBody    notify_voltage_out_of_range_body    "notify.voltage_out_of_range_body",
    NotifyFrequencyOutOfRangeTitle notify_frequency_out_of_range_title "notify.frequency_out_of_range_title",
    NotifyFrequencyOutOfRangeBody  notify_frequency_out_of_range_body  "notify.frequency_out_of_range_body",
    NotifyRuntimeLimitTitle        notify_runtime_limit_title          "notify.runtime_limit_title",
    NotifyRuntimeLimitBody         notify_runtime_limit_body           "notify.runtime_limit_body",
    NotifyBoostTitle               notify_boost_title                  "notify.boost_title",
    NotifyBoostBody                notify_boost_body                   "notify.boost_body",
    NotifyBoostEndedTitle          notify_boost_ended_title            "notify.boost_ended_title",
    NotifyBoostEndedBody           notify_boost_ended_body             "notify.boost_ended_body",
    ErrorDeviceNotFound            error_device_not_found              "error.device_not_found",
    ErrorDeviceNotFoundHint        error_device_not_found_hint         "error.device_not_found_hint",
    ErrorConfigNotWritable         error_config_not_writable           "error.config_not_writable",
    ErrorMultipleDevices           error_multiple_devices              "error.multiple_devices",
    ErrorSelfTestNotRun            error_self_test_not_run             "error.self_test_not_run",
    ErrorLogNotWritable            error_log_not_writable              "error.log_not_writable",
    InstanceAlreadyRunningTitle    instance_already_running_title      "instance.already_running_title",
    InstanceAlreadyRunning         instance_already_running            "instance.already_running",
    UnitVolt                       unit_volt                           "unit.volt",
    UnitWatt                       unit_watt                           "unit.watt",
    UnitVoltAmp                    unit_volt_amp                       "unit.volt_amp",
    UnitPercent                    unit_percent                        "unit.percent",
    UnitMinutes                    unit_minutes                        "unit.minutes",
    PanelSelfTest                  panel_self_test                     "panel.self_test",
    SelftestConfirmTitle           selftest_confirm_title              "selftest.confirm_title",
    SelftestConfirmBody            selftest_confirm_body               "selftest.confirm_body",
    SelftestNotAcknowledged        selftest_not_acknowledged           "selftest.not_acknowledged",
    SelftestNotSafe                selftest_not_safe                   "selftest.not_safe",
    SelftestTimeout                selftest_timeout                    "selftest.timeout",
    SelftestChannelError           selftest_channel_error              "selftest.channel_error",
    ThemeDark                      theme_dark                          "theme.dark",
    ThemeLight                     theme_light                         "theme.light",
    SelftestConfirmYes             selftest_confirm_yes                "selftest.confirm_yes",
    SelftestConfirmNo              selftest_confirm_no                 "selftest.confirm_no",
    SelftestCancelled              selftest_cancelled                  "selftest.cancelled",
    ConnectBusy                    connect_busy                        "connect.busy",
    ConnectUnresponsive            connect_unresponsive                "connect.unresponsive",
    ConnectIncompatible            connect_incompatible                "connect.incompatible",
}
