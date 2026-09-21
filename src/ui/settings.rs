//! Settings state. Edits a draft copy so Cancel discards without touching the
//! live config or the INI file.
//!
//! Validation lives here rather than in the window code, so the rules are
//! testable without creating a window.

use crate::config::{Config, LogLevel, POLL_MAX_MS, POLL_MIN_MS};
use crate::lang::Locale;
use crate::notify;
use crate::strings::{self, Language};
use crate::ui::row::{DropdownItem, Edit, Gap, HotspotId, Row};
use crate::ui::scrollbar::{
    DROPDOWN_MAX_VISIBLE, DROPDOWN_PAGE, HOT_SCROLL_DOWN, HOT_SCROLL_PAGE_DOWN, HOT_SCROLL_PAGE_UP,
    HOT_SCROLL_THUMB, HOT_SCROLL_UP,
};
use crate::ui::textfield::{CharFilter, FieldEdit, TextField};
use crate::ui::theme::{Builtin, Theme};
/// Every dropdown in the dialog, in the order they appear.
///
/// One list of the three ids, so opening, closing, routing a keystroke and
/// finding which one is showing all walk the same set. Three ids written out
/// separately in four places is how one of them eventually gets left out of a
/// loop — and a list missing from the dismissal loop is a list that stays open
/// under the control the user just clicked.
const LIST_IDS: [HotspotId; 3] = [HOT_LANGUAGE, HOT_LOG_LEVEL, HOT_THEME];

/// How many options the list `id` holds, or `None` if `id` names no list.
///
/// The one place that question is answered. It was answered in three: twice in
/// the main loop, to decide whether an event belongs to a list or to the
/// focused field, and once inside the scrollbar handler, which answered it by
/// assuming the language list. The counts are not constants — the language and
/// theme lists are sized by what the build ships — so they have to be asked
/// for, and asking in three places is how one of them comes to disagree.
pub(crate) fn option_count(
    id: HotspotId,
    themes: &[Builtin],
    languages: &[Language],
) -> Option<usize> {
    match id {
        HOT_LANGUAGE => Some(languages.len()),
        HOT_LOG_LEVEL => Some(LogLevel::ALL.len()),
        HOT_THEME => Some(themes.len()),
        _ => None,
    }
}

/// Longest interval the field will hold: `60000` is five digits, and the
/// millisecond count never needs more. The cap stops a paste-free field from
/// growing without bound while still admitting every value in range.
const INTERVAL_MAX_DIGITS: usize = 5;

/// Hotspot ids for the settings controls. Numbered above the panel's own
/// ids so the two views can never collide on a click.
pub(crate) const HOT_INTERVAL: HotspotId = HotspotId::new(100);
pub(crate) const HOT_NOTIFICATIONS: HotspotId = HotspotId::new(101);
pub(crate) const HOT_POWER_FAILURE: HotspotId = HotspotId::new(102);
pub(crate) const HOT_POWER_RESTORED: HotspotId = HotspotId::new(103);
pub(crate) const HOT_LOW_BATTERY: HotspotId = HotspotId::new(104);
pub(crate) const HOT_DEVICE_FAULT: HotspotId = HotspotId::new(105);
pub(crate) const HOT_THEME: HotspotId = HotspotId::new(106);
pub(crate) const HOT_LANGUAGE: HotspotId = HotspotId::new(107);
pub(crate) const HOT_OK: HotspotId = HotspotId::new(108);
pub(crate) const HOT_CANCEL: HotspotId = HotspotId::new(109);
pub(crate) const HOT_START_MINIMIZED: HotspotId = HotspotId::new(110);
pub(crate) const HOT_OVERLOAD: HotspotId = HotspotId::new(111);
pub(crate) const HOT_VOLTAGE_OUT_OF_RANGE: HotspotId = HotspotId::new(112);
pub(crate) const HOT_RUNTIME_LIMIT: HotspotId = HotspotId::new(113);
pub(crate) const HOT_CONNECTION_LOST: HotspotId = HotspotId::new(114);
pub(crate) const HOT_LOG_LEVEL: HotspotId = HotspotId::new(115);
pub(crate) const HOT_BOOST_STARTED: HotspotId = HotspotId::new(116);
pub(crate) const HOT_BOOST_ENDED: HotspotId = HotspotId::new(117);
pub(crate) const HOT_FREQUENCY_OUT_OF_RANGE: HotspotId = HotspotId::new(118);
pub(crate) const HOT_CONNECTION_RESTORED: HotspotId = HotspotId::new(119);

/// What a click on the settings view asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Redraw; the draft changed but nothing was committed.
    Changed,
    /// Validate and save.
    Commit,
    /// Discard and return to the status view.
    Cancel,
    /// Click landed on nothing.
    None,
}

/// The settings dialog's working copy, edited freely and committed only on OK.
///
/// A separate value from [`Config`] rather than editing the live one, because
/// the dialog must be cancellable: every checkbox toggle and keystroke lands
/// here immediately — the panel redraws from the draft, so a click has to show
/// at once — and Cancel then costs a drop rather than an undo log.
///
/// The interval is a [`TextField`] and not a `u32` for the same reason. A user
/// clearing the box to retype it passes through the empty string, and a
/// numeric field would have to invent a value for that moment or refuse the
/// keystroke; the text is kept as typed and parsed once, at commit, where an
/// unparseable or out-of-range entry can be reported instead of silently
/// becoming something else.
///
/// The list fields carry their own open/scroll/highlight state because a
/// dropdown is drawn by the same immediate-mode painter as everything else:
/// there is no widget to remember it, so the draft does.
pub(crate) struct SettingsDraft {
    pub interval: TextField,
    pub notifications_enabled: bool,
    pub on_power_failure: bool,
    pub on_power_restored: bool,
    pub on_low_battery: bool,
    pub on_device_fault: bool,
    pub on_overload: bool,
    pub on_voltage_out_of_range: bool,
    pub on_frequency_out_of_range: bool,
    pub on_runtime_limit: bool,
    pub on_connection_lost: bool,
    pub on_connection_restored: bool,
    pub on_boost_started: bool,
    pub on_boost_ended: bool,
    pub theme: String,
    pub language: String,
    pub log_level: LogLevel,
    pub start_minimized: bool,
    pub error: Option<String>,
    /// Whether the language list is showing, and where its highlight and
    /// scroll sit while it is.
    ///
    /// Held in the draft rather than in the window because the rows are
    /// rebuilt from the draft on every repaint: state kept only in the window
    /// would be discarded the moment anything else changed. `highlight` is
    /// `None` until a key moves it, so opening the list starts on whatever is
    /// currently selected without having to be told what that is.
    ///
    /// The three dropdowns share one `ListState` shape so the open/highlight/
    /// scroll logic is written once and routed to by id, rather than three
    /// near-identical field triples that drift. Log level and theme have few
    /// enough options never to scroll, but carrying the field regardless keeps
    /// them structurally identical to the language list — no `Some`/`None`
    /// special-casing of scroll anywhere.
    pub lang: ListState,
    pub log: ListState,
    pub theme_list: ListState,
}

/// Open/highlight/scroll state of one dropdown list. See [`SettingsDraft::lang`].
#[derive(Clone, Copy, Default)]
pub(crate) struct ListState {
    /// Whether the list is currently showing.
    pub open: bool,
    /// The highlighted option, or `None` until a key or hover moves it, in
    /// which case the caller falls back to the currently selected option.
    pub highlight: Option<usize>,
    /// Index of the first visible option when the list is longer than the
    /// visible window.
    pub scroll: usize,
}

/// Declares the checkbox id -> field mapping once and emits both accessors.
///
/// `checkbox` reads through `&self` for drawing, `checkbox_mut` writes through
/// `&mut self` for the click path. Hand-writing the pair meant maintaining the
/// same fourteen-entry list twice; here the list exists once and the two
/// `match`es are generated from it, which is the difference between a
/// divergence that a test catches and one that cannot occur.
macro_rules! checkbox_map {
    ($($id:ident => $field:ident),+ $(,)?) => {
        /// The checkbox behind a hotspot id, if that id names one.
        fn checkbox(&self, id: HotspotId) -> Option<&bool> {
            match id {
                $($id => Some(&self.$field),)+
                _ => None,
            }
        }

        /// The same mapping, mutably: the click path toggles through this.
        ///
        /// Returning the field itself collapses arms that would otherwise
        /// differ only in which `bool` they flip, each repeating the same
        /// toggle-and-report lines.
        fn checkbox_mut(&mut self, id: HotspotId) -> Option<&mut bool> {
            match id {
                $($id => Some(&mut self.$field),)+
                _ => None,
            }
        }

        /// Every checkbox, read out of the configuration.
        ///
        /// Emitted from the same pairs as the accessors above, because the
        /// draft's field and the configuration's field have one name between
        /// them. Written out, this was a fourteen-line list, its mirror in
        /// `apply` was another, and a switch missing from either is a setting
        /// that silently refuses to be changed.
        fn load_switches(&mut self, cfg: &Config) {
            $(self.$field = cfg.$field;)+
        }

        /// The same fourteen, written back.
        fn store_switches(&self, cfg: &mut Config) {
            $(cfg.$field = self.$field;)+
        }
    };
}

impl SettingsDraft {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        let mut draft = Self {
            interval: TextField::new(
                cfg.poll_interval_ms.to_string(),
                CharFilter::Digits,
                INTERVAL_MAX_DIGITS,
            ),
            theme: cfg.theme.clone(),
            language: cfg.language.clone(),
            log_level: cfg.log_level,
            // Placeholders: `load_switches` below fills every checkbox from
            // the configuration, so naming them here would be the same list
            // written twice.
            notifications_enabled: false,
            start_minimized: false,
            on_power_failure: false,
            on_power_restored: false,
            on_low_battery: false,
            on_device_fault: false,
            on_overload: false,
            on_voltage_out_of_range: false,
            on_frequency_out_of_range: false,
            on_runtime_limit: false,
            on_connection_lost: false,
            on_connection_restored: false,
            on_boost_started: false,
            on_boost_ended: false,
            error: None,
            lang: ListState::default(),
            log: ListState::default(),
            theme_list: ListState::default(),
        };
        // Every checkbox at once, from the pairs the macro already states.
        draft.load_switches(cfg);
        draft
    }

    /// The per-event switches, in display order: each event kind paired with
    /// the hotspot id of its checkbox.
    ///
    /// One list, used by `rows` to draw them and by `checkbox_mut` to find the
    /// field a click lands on. Two lists would be two places to add an event
    /// to, and the failure mode of forgetting the second is a checkbox that
    /// draws but does not toggle — visible only to whoever clicks it.
    ///
    /// The label is not stored here: it is `kind.info().switch_key`, so the
    /// switch and the event it gates cannot name two different strings. Only
    /// the hotspot id — a UI concern the event model knows nothing about —
    /// lives here. The order follows `EventKind::ALL`: power events
    /// first (what the utility is installed for), the two connection events
    /// last (a different kind of problem from anything the mains does).
    pub(crate) const EVENT_SWITCHES: [(notify::EventKind, HotspotId); 12] = {
        use notify::EventKind as K;
        [
            (K::PowerFailure, HOT_POWER_FAILURE),
            (K::PowerRestored, HOT_POWER_RESTORED),
            (K::LowBattery, HOT_LOW_BATTERY),
            (K::RuntimeLimitExpired, HOT_RUNTIME_LIMIT),
            (K::VoltageOutOfRange, HOT_VOLTAGE_OUT_OF_RANGE),
            (K::FrequencyOutOfRange, HOT_FREQUENCY_OUT_OF_RANGE),
            (K::BoostStarted, HOT_BOOST_STARTED),
            (K::BoostEnded, HOT_BOOST_ENDED),
            (K::DeviceFault, HOT_DEVICE_FAULT),
            (K::Overload, HOT_OVERLOAD),
            (K::Disconnected, HOT_CONNECTION_LOST),
            (K::Reconnected, HOT_CONNECTION_RESTORED),
        ]
    };

    /// Builds the settings rows for the current draft.
    pub(crate) fn rows(
        &self,
        locale: Locale,
        theme: &Theme,
        themes: &[Builtin],
        languages: &'static [Language],
    ) -> Vec<Row> {
        let mut rows = Vec::new();
        rows.push(Row::Header(
            locale.t(strings::Key::SettingsTitle).to_owned(),
        ));
        rows.push(Row::Space(Gap::Half));

        let (sel_start, sel_end) = self.interval.selection().unwrap_or((0, 0));
        rows.push(Row::Field {
            label: locale.t(strings::Key::SettingsPollInterval).to_owned(),
            value: self.interval.text().to_owned(),
            caret: self.interval.cursor(),
            sel_start,
            sel_end,
            id: HOT_INTERVAL,
        });

        // English name first, native name in parentheses: the Latin half is
        // legible whatever fonts the machine has, and the native half confirms
        // the choice to whoever is looking for their own language.
        let lang_index = languages
            .iter()
            .position(|l| l.code == self.language)
            .unwrap_or(0);
        rows.push(dropdown_row(
            locale.t(strings::Key::SettingsLanguage).to_owned(),
            languages.iter().map(language_item).collect(),
            lang_index,
            &self.lang,
            HOT_LANGUAGE,
        ));

        // A dropdown, like the language and log-level rows: two themes ship
        // now, so the control is a genuine choice. Each option's label is the
        // theme's localised name; there is no native-name second half, so the
        // parenthesised part is left empty.
        let theme_index = themes
            .iter()
            .position(|t| t.code() == self.theme)
            .unwrap_or(0);
        rows.push(dropdown_row(
            locale.t(strings::Key::SettingsTheme).to_owned(),
            themes
                .iter()
                .map(|t| plain_item(locale, t.name_key()))
                .collect(),
            theme_index,
            &self.theme_list,
            HOT_THEME,
        ));

        // A list rather than a cycler despite having only two values, so the
        // two rows above it behave the same way. Two adjacent controls that
        // look alike and respond differently to a click is something a user
        // discovers by trying, which is the wrong way to find out.
        let level_index = LogLevel::ALL
            .iter()
            .position(|l| *l == self.log_level)
            .unwrap_or(0);
        rows.push(dropdown_row(
            locale.t(strings::Key::SettingsLogLevel).to_owned(),
            LogLevel::ALL
                .iter()
                .map(|l| plain_item(locale, l.lang_key()))
                .collect(),
            level_index,
            &self.log,
            HOT_LOG_LEVEL,
        ));

        rows.push(Row::Space(Gap::Half));
        rows.push(Row::Checkbox {
            label: locale.t(strings::Key::SettingsStartMinimized).to_owned(),
            checked: self.start_minimized,
            depth: 0,
            id: HOT_START_MINIMIZED,
        });

        rows.push(Row::Space(Gap::Half));
        rows.push(Row::Checkbox {
            label: locale.t(strings::Key::SettingsNotifications).to_owned(),
            checked: self.notifications_enabled,
            depth: 0,
            id: HOT_NOTIFICATIONS,
        });

        // The per-event switches only mean anything while the master switch
        // is on, so they are hidden rather than shown as dead controls.
        if self.notifications_enabled {
            rows.extend(self.event_switch_rows(locale));
        }

        // The gap above the button row is carried by the row itself
        // (`button_row_lead`), which assumes the row before the buttons leaves
        // the same `spacing / 2` of trailing blank a checkbox does. In the
        // ordinary case that row *is* a checkbox. When the interval is invalid
        // the warning is inserted instead, so it is given a matching
        // half-spacing spacer below it — that way the buttons sit the same
        // distance under the last line of text whether or not the warning is
        // showing. The warning is a `Notice`: always one line, no wrapping. It
        // is contracted to fit one line on every language (a translation that
        // would not fit is shortened, see the strings table).
        if let Some(err) = &self.error {
            rows.push(Row::Space(Gap::Half));
            rows.push(Row::Notice {
                text: err.clone(),
                color: theme.colors.critical,
            });
            rows.push(Row::Space(Gap::Half));
        }

        rows.push(Row::Buttons {
            left: locale.t(strings::Key::SettingsOk).to_owned(),
            left_id: HOT_OK,
            right: locale.t(strings::Key::SettingsCancel).to_owned(),
            right_id: HOT_CANCEL,
        });
        rows
    }

    // The one place a checkbox id is tied to the field it controls.
    //
    // Two readers need this mapping and they need it through different kinds
    // of reference: `rows` draws each box from `&self`, `on_click` toggles it
    // through `&mut self`. Written out, that was two fourteen-arm `match`es
    // over the same list, and the failure mode of a mismatch between them is
    // the worst kind — a checkbox that draws one field and toggles another
    // responds to clicks by appearing not to. A test pinned the two together,
    // but a test detects drift where this prevents it: the macro states the
    // pairs once and emits both accessors from them, so the two cannot
    // disagree by construction.
    checkbox_map! {
        HOT_START_MINIMIZED => start_minimized,
        HOT_NOTIFICATIONS => notifications_enabled,
        HOT_POWER_FAILURE => on_power_failure,
        HOT_POWER_RESTORED => on_power_restored,
        HOT_LOW_BATTERY => on_low_battery,
        HOT_DEVICE_FAULT => on_device_fault,
        HOT_OVERLOAD => on_overload,
        HOT_VOLTAGE_OUT_OF_RANGE => on_voltage_out_of_range,
        HOT_FREQUENCY_OUT_OF_RANGE => on_frequency_out_of_range,
        HOT_RUNTIME_LIMIT => on_runtime_limit,
        HOT_CONNECTION_LOST => on_connection_lost,
        HOT_CONNECTION_RESTORED => on_connection_restored,
        HOT_BOOST_STARTED => on_boost_started,
        HOT_BOOST_ENDED => on_boost_ended,
    }

    /// The twelve per-event switches, one row per notifiable event kind.
    ///
    /// Its own function because it is the only part of this list that is
    /// generated rather than written out: the rows come from
    /// [`Self::EVENT_SWITCHES`], so a new `EventKind` appears here without
    /// anything being added, and that reasoning belongs with the loop rather
    /// than in the middle of a page layout.
    fn event_switch_rows(&self, locale: Locale) -> impl Iterator<Item = Row> + '_ {
        Self::EVENT_SWITCHES
            .into_iter()
            .map(move |(kind, id)| Row::Checkbox {
                label: locale.t(kind.info().switch_key).to_owned(),
                // Read through the same accessor the click path writes through,
                // so a switch cannot draw one state and toggle another.
                checked: self.switch(id),
                // One level in from the master switch above them.
                depth: 1,
                id,
            })
    }

    /// The current state of a checkbox, by the same id the click path uses.
    /// A thin read over the shared map above, so a row can never be drawn from
    /// a field other than the one its click writes.
    fn switch(&self, id: HotspotId) -> bool {
        self.checkbox(id).copied().unwrap_or(false)
    }

    /// Chooses option `index` of list `id`, closing it.
    ///
    /// The [`ListState`] a dropdown id names, or `None` for an id that is not
    /// a dropdown. One routing table shared by open, close, highlight and
    /// scroll, so the three lists cannot drift in how they are reached.
    fn list_mut(&mut self, id: HotspotId) -> Option<&mut ListState> {
        match id {
            HOT_LANGUAGE => Some(&mut self.lang),
            HOT_LOG_LEVEL => Some(&mut self.log),
            HOT_THEME => Some(&mut self.theme_list),
            _ => None,
        }
    }

    /// Shared read-only counterpart of [`Self::list_mut`].
    fn list_ref(&self, id: HotspotId) -> Option<&ListState> {
        match id {
            HOT_LANGUAGE => Some(&self.lang),
            HOT_LOG_LEVEL => Some(&self.log),
            HOT_THEME => Some(&self.theme_list),
            _ => None,
        }
    }

    /// The list currently showing, if any.
    ///
    /// At most one ever is — every click closes the lists it did not land on —
    /// and this is what makes that invariant readable instead of assumed. The
    /// scrollbar needs it: its ids say *that* a list is being scrolled, never
    /// which, because there is only ever one on screen to scroll.
    fn open_list(&self) -> Option<HotspotId> {
        LIST_IDS
            .into_iter()
            .find(|id| self.list_ref(*id).is_some_and(|l| l.open))
    }

    /// The one place a selection is written, reached from both the click path
    /// and the keyboard path. Two copies of "set the value, close the list,
    /// clear the highlight" is how one of them eventually forgets a step —
    /// and a list left open with its highlight stale is invisible until the
    /// next keypress does something unexpected.
    ///
    /// `themes` and `languages` are the lists the caller built the rows from,
    /// and the index is an index into *those*. It used to be applied to the
    /// global tables instead, while the same module took both as parameters
    /// everywhere else — `on_click`, `on_scrollbar`, `option_count`, `rows`.
    /// Two sources for one list is a defect even while they agree: a
    /// parameter in a signature is a promise that the caller decides, and the
    /// index came from a list built on that promise. (`LogLevel::ALL` is not a
    /// parameter anywhere in this module, so it stays global here too — the
    /// point is one source per list, not that every list must be threaded.)
    fn choose(
        &mut self,
        id: HotspotId,
        index: usize,
        themes: &[Builtin],
        languages: &[Language],
    ) -> Action {
        match id {
            HOT_LANGUAGE => {
                if let Some(l) = languages.get(index) {
                    l.code.clone_into(&mut self.language);
                }
            }
            HOT_LOG_LEVEL => {
                if let Some(l) = LogLevel::ALL.get(index) {
                    self.log_level = *l;
                }
            }
            HOT_THEME => {
                if let Some(b) = themes.get(index) {
                    b.code().clone_into(&mut self.theme);
                }
            }
            _ => return Action::None,
        }
        if let Some(list) = self.list_mut(id) {
            list.open = false;
            list.highlight = None;
        }
        Action::Changed
    }

    /// Opens or closes a list, and on opening scrolls it to the current
    /// choice — a language near the end of twenty-four would otherwise be off
    /// screen with no sign of where the selection went.
    fn toggle_list(&mut self, id: HotspotId, current: usize) -> Action {
        let Some(list) = self.list_mut(id) else {
            return Action::None;
        };
        list.open = !list.open;
        let showing = list.open;
        list.highlight = showing.then_some(current);
        // Scroll the current choice into view on opening. For a list shorter
        // than the visible window this saturates to zero, so the same line
        // serves the log-level and theme lists without a special case: they
        // simply never have anywhere to scroll to.
        list.scroll = if showing {
            current.saturating_sub(DROPDOWN_VISIBLE / 2)
        } else {
            0
        };
        Action::Changed
    }

    /// Applies a click on a settings control to the draft.
    pub(crate) fn on_click(
        &mut self,
        id: HotspotId,
        themes: &[Builtin],
        languages: &[Language],
    ) -> Action {
        // An option inside an open list. Checked first because option ids live
        // in their own numeric band and cannot collide with a control's.
        if let Some((base, index)) = id.split_option() {
            return self.choose(base, index, themes, languages);
        }

        // The open list's own scrollbar. Checked before the dismissal below
        // and returning early, because everything after this point closes any
        // list that is not the one clicked — and a scrollbar click is not a
        // click on another control, it is a click on part of the list itself.
        // Falling through would have closed the list on the first press of a
        // stepper button, which is the one interaction the whole scrollbar
        // exists to provide.
        if let Some(action) = self.on_scrollbar(id, themes, languages) {
            return action;
        }

        // Any click closes a list that is not the one clicked — including a
        // click that hit no control at all, which arrives as HotspotId::NOTHING. A
        // list left open behind a click elsewhere covers the control the user
        // just moved to, and being hit-tested first it would keep swallowing
        // their clicks.
        let was_open = self.open_list().is_some();
        for list_id in LIST_IDS {
            let Some(list) = self.list_mut(list_id) else {
                continue;
            };
            list.open &= id == list_id;
            if !list.open {
                list.highlight = None;
            }
        }

        if let Some(flag) = self.checkbox_mut(id) {
            *flag = !*flag;
            // Nothing about focus here. Which control the keyboard is aimed at
            // is the window's, for every kind of control alike; a draft that
            // also had an opinion would be a second answer to one question,
            // and it was — the window cleared field focus on a click away, the
            // draft did not, and the dialog went on drawing a caret the
            // keyboard could no longer reach.
            return Action::Changed;
        }
        match id {
            HOT_THEME => {
                let at = themes
                    .iter()
                    .position(|t| t.code() == self.theme)
                    .unwrap_or(0);
                self.toggle_list(id, at)
            }
            HOT_LANGUAGE => {
                let at = languages
                    .iter()
                    .position(|l| l.code == self.language)
                    .unwrap_or(0);
                self.toggle_list(id, at)
            }
            HOT_LOG_LEVEL => {
                let at = LogLevel::ALL
                    .iter()
                    .position(|l| *l == self.log_level)
                    .unwrap_or(0);
                self.toggle_list(id, at)
            }
            HOT_INTERVAL => {
                // Focus is the window's to grant; the press that delivered this
                // click has already granted it. All that is left is to repaint.
                Action::Changed
            }
            HOT_OK => Action::Commit,
            HOT_CANCEL => Action::Cancel,
            // Any id not handled above — the bare background among them —
            // activates nothing, but the click may have just dismissed an open
            // list, and that needs a repaint. `HotspotId::NOTHING` used to have an arm
            // of its own directly above this one, with the same guard and the
            // same result: a duplicate that could never be reached, because
            // this arm matches everything the other one did.
            _ if was_open => Action::Changed,
            _ => Action::None,
        }
    }

    /// Applies a click on the open list's scrollbar, if `id` names part of
    /// one. Returns `None` when it does not, so the caller falls through to
    /// its ordinary control handling.
    ///
    /// The scrollbar belongs to whichever list is open, and that is how it is
    /// found: its ids name a part of the bar, never a list, because only one
    /// list is ever on screen to own it. Today only the language list is long
    /// enough to be drawn with a bar at all — but reading that fact backwards
    /// and routing every bar click to the language list was an assumption about
    /// the option counts of the other two, held in a different function from
    /// the one that decides them.
    ///
    /// With no list open there is nothing to scroll, and the event is absorbed
    /// rather than fallen through: it is still a click on a scrollbar, and
    /// letting it reach the ordinary control handling would make it read as a
    /// click on the background.
    fn on_scrollbar(
        &mut self,
        id: HotspotId,
        themes: &[Builtin],
        languages: &[Language],
    ) -> Option<Action> {
        // `Option`, not a step of zero: "this part moves the list by n" and
        // "this part moves it by nothing" are different answers, and a
        // sentinel value inside the number would put the second one where the
        // reader expects a distance. Both still go through the open-list check
        // below, which is the whole point of naming them here rather than
        // returning early.
        let step = match id {
            // Positive is toward the top, matching the wheel: `on_list_edit`
            // subtracts the notch count from the first visible index, so the
            // up button and a wheel-up must carry the same sign.
            HOT_SCROLL_UP => Some(1),
            HOT_SCROLL_DOWN => Some(-1),
            // The same page the keyboard's Page Up and Page Down move by:
            // one screenful less the overlapping line, named once next to the
            // visible count it is derived from.
            HOT_SCROLL_PAGE_UP => Some(DROPDOWN_PAGE as i32),
            HOT_SCROLL_PAGE_DOWN => Some(-(DROPDOWN_PAGE as i32)),
            // The thumb scrolls by being dragged, not by being clicked. The
            // press already began the drag; the release that lands here must
            // do nothing at all, or a click on the thumb would jump the list
            // before the drag it started could move it.
            HOT_SCROLL_THUMB => None,
            _ => return None,
        };
        // No list open: this click asks for nothing, exactly as every arm
        // below this line already answered. The thumb used to return
        // `Changed` *above* this check, and so asked for a repaint of an open
        // list at a moment when there was none.
        let Some(open) = self.open_list() else {
            return Some(Action::None);
        };
        let Some(step) = step else {
            // The thumb, with a list open: nothing moves, but the press that
            // began the drag changed how the control is drawn.
            return Some(Action::Changed);
        };
        Some(self.on_list_edit(open, Edit::Scroll(step), themes, languages))
    }

    /// Applies a keyboard or wheel event aimed at an open list.
    ///
    /// Separate from `on_edit` because the two answer to different controls:
    /// `on_edit` belongs to the focused text field, this to whichever list is
    /// showing. A list is open only when no field has focus, so the two never
    /// compete for a keystroke.
    ///
    /// The option count is derived from `themes` and `languages` rather than
    /// taken as a parameter beside them. It used to be passed in, computed by
    /// the caller from these same lists — so a caller could hand over a count
    /// that did not match the lists it also handed over, and the length would
    /// have two sources. One question, one answer: `option_count`.
    pub(crate) fn on_list_edit(
        &mut self,
        id: HotspotId,
        edit: Edit,
        themes: &[Builtin],
        languages: &[Language],
    ) -> Action {
        let Some(count) = option_count(id, themes, languages) else {
            return Action::None;
        };
        // `Cancel` and `Commit` need `&self` calls (`choose`) that cannot run
        // while a `&mut` borrow of the list is held, so they are handled
        // before the borrow is taken.
        match edit {
            Edit::Commit => {
                let Some(list) = self.list_ref(id) else {
                    return Action::None;
                };
                if !list.open || count == 0 {
                    return Action::None;
                }
                let at = list.highlight.unwrap_or(0).min(count - 1);
                return self.choose(id, at, themes, languages);
            }
            Edit::Cancel => {
                let Some(list) = self.list_mut(id) else {
                    return Action::None;
                };
                if !list.open {
                    return Action::None;
                }
                list.highlight = None;
                list.open = false;
                return Action::Changed;
            }
            _ => {}
        }

        let Some(list) = self.list_mut(id) else {
            return Action::None;
        };
        if !list.open || count == 0 {
            return Action::None;
        }
        let at = list.highlight.unwrap_or(0).min(count - 1);
        let moved = match edit {
            // Where the key lands is `Move`'s arithmetic, not this function's:
            // arrows, pages and the two ends differ only in distance, and all
            // six share the tail below that scrolls the result into view.
            Edit::Highlight(mv) => mv.apply(at, count),
            // The pointer names the option directly. It never scrolls the
            // list: the option is under the cursor, so it is on screen by
            // construction, and scrolling to "reveal" it would slide the list
            // out from under the pointer.
            Edit::Hover(index) => {
                list.highlight = Some(index.min(count - 1));
                return Action::Changed;
            }
            Edit::Scroll(notches) => {
                // Wheel-up is positive and moves the view toward the top,
                // i.e. subtracts from the first visible index. Lists shorter
                // than the window have `max == 0` and clamp to no movement.
                let max = count.saturating_sub(DROPDOWN_VISIBLE) as i32;
                list.scroll = (list.scroll as i32 - notches).clamp(0, max) as usize;
                return Action::Changed;
            }
            // A thumb drag names a position outright rather than a movement.
            // Clamped here as well as at the source: this is the field's own
            // invariant, and a caller working from geometry that a pending
            // repaint has not yet replaced must not be able to put the view
            // past the end of the list.
            Edit::ScrollTo(index) => {
                list.scroll = index.min(count.saturating_sub(DROPDOWN_VISIBLE));
                return Action::Changed;
            }
            _ => return Action::None,
        };
        list.highlight = Some(moved);
        // Keep the highlight on screen after an arrow key.
        if moved < list.scroll {
            list.scroll = moved;
        } else if moved >= list.scroll + DROPDOWN_VISIBLE {
            list.scroll = moved + 1 - DROPDOWN_VISIBLE;
        }
        Action::Changed
    }

    /// Applies a keyboard or mouse edit to the focused interval field.
    ///
    /// Commit and Cancel are the field's own (Enter and Escape); every other
    /// variant is translated to a [`FieldEdit`] and handed to the `TextField`,
    /// which owns the caret and selection. The digit filter lives in the
    /// field, not here: refusing non-digits at the keyboard beats reporting
    /// them at OK, and the field enforces it uniformly for typing and pasting
    /// alike (there is no paste, but the rule has one home either way).
    pub(crate) fn on_edit(&mut self, id: HotspotId, edit: Edit) -> Action {
        if id != HOT_INTERVAL {
            return Action::None;
        }
        let field_edit = match edit {
            Edit::Insert(c) => FieldEdit::Insert(c),
            Edit::Backspace => FieldEdit::Backspace,
            Edit::Delete => FieldEdit::Delete,
            Edit::CaretLeft { extend } => FieldEdit::Left { extend },
            Edit::CaretRight { extend } => FieldEdit::Right { extend },
            Edit::CaretHome { extend } => FieldEdit::Home { extend },
            Edit::CaretEnd { extend } => FieldEdit::End { extend },
            Edit::SelectAll => FieldEdit::SelectAll,
            Edit::CaretTo { index, extend } => FieldEdit::CaretTo { index, extend },
            Edit::Commit => return Action::Commit,
            Edit::Cancel => return Action::Cancel,
            // Navigation and scrolling belong to an open list, and a list is
            // open only when no field has focus, so these cannot arrive here.
            Edit::Highlight(_) | Edit::Scroll(_) | Edit::ScrollTo(_) | Edit::Hover(_) => {
                return Action::None
            }
        };
        // A caret move that changes nothing (arrowing past the edge) still
        // reports `Changed` cheaply here; the caller repaints, which is
        // harmless and keeps the blink phase honest. Refusing to repaint on a
        // rejected digit is not worth a second return path.
        self.interval.apply(field_edit);
        Action::Changed
    }

    /// Validates and writes into the live config. Returns false and sets
    /// `error` when the interval is out of range, leaving the config
    /// untouched so a rejected edit cannot half-apply.
    pub(crate) fn apply_to(&mut self, cfg: &mut Config, locale: Locale) -> bool {
        let parsed = self.interval.text().trim().parse::<u32>().ok();
        let Some(ms) = parsed.filter(|v| (POLL_MIN_MS..=POLL_MAX_MS).contains(v)) else {
            self.error = Some(locale.t1(
                strings::Key::SettingsInvalidInterval,
                &POLL_MIN_MS.to_string(),
            ));
            return false;
        };
        self.error = None;

        cfg.poll_interval_ms = ms;
        self.store_switches(cfg);
        cfg.theme.clone_from(&self.theme);
        cfg.language.clone_from(&self.language);
        cfg.log_level = self.log_level;
        true
    }
}

/// How many options the language list shows before it scrolls. Mirrors the
/// painter's cap so scroll arithmetic here agrees with what is drawn.
const DROPDOWN_VISIBLE: usize = DROPDOWN_MAX_VISIBLE;

/// One dropdown row, assembled from the five things that differ between them.
///
/// Language, theme and log level are the same control three times: a list of
/// options, the position of the current value in it, and the open/highlight/
/// scroll state that belongs to that list. Written out three times, the one
/// piece of logic among those fields — a highlight falls back to the selection
/// when the list has not been navigated — had to be got right three times.
fn dropdown_row(
    label: String,
    options: Vec<DropdownItem>,
    selected: usize,
    list: &ListState,
    id: HotspotId,
) -> Row {
    Row::Dropdown {
        label,
        options,
        selected,
        highlighted: list.highlight.unwrap_or(selected),
        open: list.open,
        scroll: list.scroll,
        id,
    }
}

/// An option whose whole text is one localized string.
///
/// The themes and the log levels have no second, native-script name the way
/// the languages do; leaving `native` empty is what says so.
fn plain_item(locale: Locale, key: strings::Key) -> DropdownItem {
    DropdownItem {
        label: locale.t(key).to_owned(),
        native: String::new(),
        font: None,
    }
}

/// One language as an option in the list.
///
/// English name first because it is Latin script in every entry, so the list
/// stays scannable regardless of the native name's script. The native name
/// follows in parentheses, and is dropped where it would repeat the English
/// one — "English (English)" says nothing twice.
pub(crate) fn language_item(l: &'static Language) -> DropdownItem {
    DropdownItem {
        label: l.english_name.to_owned(),
        native: if l.english_name == l.native_name {
            String::new()
        } else {
            l.native_name.to_owned()
        },
        font: l.font,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests name the individual list moves; the module itself passes
    // whatever `Edit::Highlight` carries straight to `Move::apply`.
    //
    // `HotspotId::NOTHING` is here for the same reason: the dialog no longer names it
    // either. It used to have an arm of its own in `on_click`, fully shadowed
    // by the catch-all below it, and the arm's removal left the import used by
    // nothing but these tests.
    use crate::ui::row::Move;
    fn locale() -> Locale {
        Locale::english()
    }

    /// Replaces the interval field's contents in a test, mirroring what the
    /// user would type. The field is a `TextField`, so tests set it through
    /// its constructor rather than assigning a bare string.
    fn set_interval(d: &mut SettingsDraft, text: &str) {
        d.interval = TextField::new(text, CharFilter::Digits, INTERVAL_MAX_DIGITS);
    }

    /// Every checkbox toggles and reports the change.
    ///
    /// These were six copies of the same lines; the refactor makes the toggle
    /// uniform by construction and this pins it. Focus is deliberately not
    /// asserted here: which control the keyboard is aimed at belongs to the
    /// window, not to the draft.
    #[test]
    fn every_checkbox_toggles_and_reports_change() {
        let cfg = Config::default();
        for id in [
            HOT_START_MINIMIZED,
            HOT_NOTIFICATIONS,
            HOT_POWER_FAILURE,
            HOT_POWER_RESTORED,
            HOT_LOW_BATTERY,
            HOT_DEVICE_FAULT,
        ] {
            let mut d = SettingsDraft::from_config(&cfg);

            let before = *d.checkbox_mut(id).expect("id must be a checkbox");
            let action = d.on_click(id, &[], &[]);

            assert!(
                matches!(action, Action::Changed),
                "id {id:?} must report a change"
            );
            assert_eq!(
                *d.checkbox_mut(id).unwrap(),
                !before,
                "id {id:?} must flip its own flag"
            );
        }
    }

    /// Every event switch draws and toggles the same field.
    ///
    /// `rows` reads through `switch` and `on_click` writes through
    /// `checkbox_mut`, which is two matches over the same ids. A checkbox
    /// wired to one field in the first and another in the second draws a
    /// state it does not control, and nothing but clicking it would show
    /// that — no test would fail and no code would look wrong.
    #[test]
    fn every_switch_reads_and_writes_the_same_field() {
        let cfg = Config::default();
        for (kind, id) in SettingsDraft::EVENT_SWITCHES {
            let mut d = SettingsDraft::from_config(&cfg);
            let before = d.switch(id);
            let label = kind.info().switch_key.label();
            assert_eq!(
                *d.checkbox_mut(id)
                    .unwrap_or_else(|| panic!("{label} ({id:?}) must be a checkbox")),
                before,
                "{label}: the row reads a different field than the click writes"
            );
            d.on_click(id, &[], &[]);
            assert_eq!(d.switch(id), !before, "{label} did not toggle");
        }
    }

    /// Every `EventKind` the user can be notified about has a switch here.
    ///
    /// The audit this encodes: twelve event kinds were gated by four
    /// switches, so overload rode on the device-fault switch, both
    /// mains-quality flags on the power-failure switch, the runtime limit on
    /// the low-battery switch, and the connection pair on nothing at all.
    /// Muting one event silenced others the user never chose to mute.
    ///
    /// The count is asserted rather than the list, because the list is the
    /// thing under test. `notify::tests::every_event_kind_has_a_settings_switch`
    /// is the other half: it walks `EventKind` exhaustively, so a new variant
    /// fails to compile until it is given a switch.
    #[test]
    fn every_notifiable_event_has_its_own_switch() {
        // Twelve switches for twelve kinds: nothing shares a control any
        // more. See `Config::notifies_for`, which reads them.
        assert_eq!(SettingsDraft::EVENT_SWITCHES.len(), 12);

        let ids: std::collections::HashSet<_> = SettingsDraft::EVENT_SWITCHES
            .iter()
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(
            ids.len(),
            SettingsDraft::EVENT_SWITCHES.len(),
            "two switches share a hotspot id"
        );
    }

    /// The label of every switch resolves to real text.
    ///
    /// A missing key renders the key itself into the dialog — "settings.
    /// `on_overload`" sitting where a sentence belongs. Cheap to check, and the
    /// failure is invisible until someone opens the window in that language.
    #[test]
    fn every_switch_has_a_label() {
        let l = locale();
        for (kind, _) in SettingsDraft::EVENT_SWITCHES {
            let key = kind.info().switch_key;
            assert_ne!(
                l.t(key),
                key.label(),
                "{} has no text in the embedded locale",
                key.label()
            );
        }
        for key in [
            strings::Key::SettingsLogLevel,
            strings::Key::SettingsLogNormal,
            strings::Key::SettingsLogDebug,
        ] {
            assert_ne!(
                l.t(key),
                key.label(),
                "{} has no text in the embedded locale",
                key.label()
            );
        }
    }

    /// Every log level is reachable, and the control opens before it chooses.
    ///
    /// The two-click shape is the point: a dropdown that changed the value on
    /// the click that opened it would alter a setting the user was only
    /// looking at. Previously this control cycled on every click and the test
    /// asserted that; the assertion was rewritten rather than deleted because
    /// the invariant it protects — no level is unreachable — still holds.
    #[test]
    fn every_log_level_is_reachable_through_the_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let start = d.log_level;

        d.on_click(HOT_LOG_LEVEL, &[], &[]);
        assert!(d.log.open, "the first click opens the list");
        assert_eq!(
            d.log_level, start,
            "opening a list must not change the value"
        );

        for (i, level) in LogLevel::ALL.iter().enumerate() {
            d.log.open = true;
            d.on_click(HOT_LOG_LEVEL.option(i), &[], &[]);
            assert_eq!(d.log_level, *level, "{level:?} is unreachable");
            assert!(!d.log.open, "choosing an option closes the list");
        }
    }

    /// The theme control is a dropdown now, like language and log level, and
    /// every shipped theme must be reachable through it. Pins the migration
    /// from the old click-cycler to a real list.
    #[test]
    fn every_theme_is_reachable_through_the_list() {
        use crate::ui::theme::Builtin;
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let start = d.theme.clone();

        d.on_click(HOT_THEME, &Builtin::ALL, &[]);
        assert!(d.theme_list.open, "the first click opens the list");
        assert_eq!(d.theme, start, "opening a list must not change the value");

        for (i, b) in Builtin::ALL.iter().enumerate() {
            d.theme_list.open = true;
            d.on_click(HOT_THEME.option(i), &Builtin::ALL, &[]);
            assert_eq!(d.theme, b.code(), "{b:?} is unreachable");
            assert!(!d.theme_list.open, "choosing an option closes the list");
        }
    }

    /// The default theme a fresh config carries is dark, and it survives a
    /// round-trip through the draft.
    #[test]
    fn theme_defaults_to_dark_and_round_trips() {
        let cfg = Config::default();
        assert_eq!(cfg.theme, "dark");
        let d = SettingsDraft::from_config(&cfg);
        assert_eq!(d.theme, "dark");
    }

    /// Clicking the control a second time closes the list without choosing.
    #[test]
    fn clicking_an_open_list_closes_it_unchanged() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let before = d.log_level;
        d.on_click(HOT_LOG_LEVEL, &[], &[]);
        d.on_click(HOT_LOG_LEVEL, &[], &[]);
        assert!(!d.log.open);
        assert_eq!(d.log_level, before);
    }

    /// A click anywhere else closes an open list.
    ///
    /// Without this the list stays over the controls below it, covering the
    /// one the user just moved to — and because the overlay is hit-tested
    /// first, it would keep swallowing their clicks.
    #[test]
    fn clicking_elsewhere_closes_an_open_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        d.on_click(HOT_LANGUAGE, &[], crate::lang::Locale::available());
        assert!(d.lang.open);
        d.on_click(HOT_NOTIFICATIONS, &[], crate::lang::Locale::available());
        assert!(!d.lang.open, "a click elsewhere must dismiss the list");
    }

    /// Clicking into the interval field closes an open list.
    ///
    /// The field acts on mouse-*press* in the window layer, which used to push
    /// only a caret edit and never an id, so the draft's list-closing rule —
    /// keyed off the clicked id — never ran, and the language list stayed open
    /// over a field that had visibly taken focus. The window now records the
    /// press as a click on the field, which reaches `on_click` here; this pins
    /// that the field id closes the list exactly as any other click does.
    #[test]
    fn clicking_the_interval_field_closes_an_open_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        d.on_click(HOT_LANGUAGE, &[], crate::lang::Locale::available());
        assert!(d.lang.open);
        d.on_click(HOT_INTERVAL, &[], crate::lang::Locale::available());
        assert!(
            !d.lang.open,
            "clicking into the interval field must dismiss the list"
        );
    }

    /// Arrow keys move the highlight without committing it.
    #[test]
    fn arrows_move_the_highlight_but_do_not_choose() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        d.on_click(HOT_LANGUAGE, &[], languages);
        let before = d.language.clone();

        d.on_list_edit(HOT_LANGUAGE, Edit::Highlight(Move::Next), &[], languages);
        assert_eq!(d.lang.highlight, Some(1));
        assert_eq!(d.language, before, "arrowing must not change the setting");

        d.on_list_edit(HOT_LANGUAGE, Edit::Commit, &[], languages);
        assert_eq!(d.language, languages[1].code, "Enter takes the highlight");
        assert!(!d.lang.open);
    }

    /// Escape closes the list and leaves the value alone. It must not reach
    /// the dialog: backing out of a list is not backing out of the settings.
    #[test]
    fn escape_closes_the_list_without_cancelling_the_dialog() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        d.on_click(HOT_LANGUAGE, &[], languages);
        let before = d.language.clone();
        d.on_list_edit(HOT_LANGUAGE, Edit::Highlight(Move::Next), &[], languages);

        let action = d.on_list_edit(HOT_LANGUAGE, Edit::Cancel, &[], languages);
        assert!(
            matches!(action, Action::Changed),
            "Escape in a list redraws; it does not cancel the dialog"
        );
        assert!(!d.lang.open);
        assert_eq!(d.language, before);
    }

    /// The highlight cannot leave the list at either end.
    #[test]
    fn the_highlight_stops_at_both_ends() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let n = languages.len();

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.highlight = Some(0);
        d.on_list_edit(
            HOT_LANGUAGE,
            Edit::Highlight(Move::Previous),
            &[],
            languages,
        );
        assert_eq!(d.lang.highlight, Some(0), "must not run off the top");

        d.lang.highlight = Some(n - 1);
        d.on_list_edit(HOT_LANGUAGE, Edit::Highlight(Move::Next), &[], languages);
        assert_eq!(d.lang.highlight, Some(n - 1), "must not run off the bottom");
    }

    /// Arrowing past the visible window scrolls it, so the highlight is never
    /// on a row that is not drawn.
    #[test]
    fn the_highlight_stays_on_screen() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let n = languages.len();
        assert!(
            n > DROPDOWN_VISIBLE,
            "this test is meaningless unless the list scrolls"
        );

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.highlight = Some(0);
        d.lang.scroll = 0;
        for _ in 0..n {
            d.on_list_edit(HOT_LANGUAGE, Edit::Highlight(Move::Next), &[], languages);
            let h = d.lang.highlight.expect("highlight is set");
            assert!(
                h >= d.lang.scroll && h < d.lang.scroll + DROPDOWN_VISIBLE,
                "highlight {h} is outside the visible window at scroll {}",
                d.lang.scroll
            );
        }
    }

    /// Page Up and Page Down move the highlight a page at a time and drag the
    /// visible window along with it, exactly as the arrows do — the same tail
    /// scrolls the result into view, so a page cannot leave the highlight on a
    /// row that is not drawn.
    ///
    /// Two pages down reach the end of a twenty-four language list and stay
    /// there, which is the behaviour the keys are expected to have on a list
    /// of any length.
    #[test]
    fn paging_moves_the_highlight_and_keeps_it_on_screen() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let n = languages.len();
        assert!(
            n > DROPDOWN_VISIBLE,
            "this test is meaningless unless the list scrolls"
        );

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.highlight = Some(0);
        d.lang.scroll = 0;

        d.on_list_edit(
            HOT_LANGUAGE,
            Edit::Highlight(Move::PageForward),
            &[],
            languages,
        );
        assert_eq!(d.lang.highlight, Some(DROPDOWN_PAGE));
        let h = d.lang.highlight.expect("highlight is set");
        assert!(
            h >= d.lang.scroll && h < d.lang.scroll + DROPDOWN_VISIBLE,
            "a page must scroll the highlight into view"
        );

        // Far enough down that another page would run off the end.
        for _ in 0..n {
            d.on_list_edit(
                HOT_LANGUAGE,
                Edit::Highlight(Move::PageForward),
                &[],
                languages,
            );
        }
        assert_eq!(
            d.lang.highlight,
            Some(n - 1),
            "paging past the end lands on the end"
        );
        assert_eq!(
            d.lang.scroll,
            n - DROPDOWN_VISIBLE,
            "and the last option is on screen"
        );

        for _ in 0..n {
            d.on_list_edit(
                HOT_LANGUAGE,
                Edit::Highlight(Move::PageBack),
                &[],
                languages,
            );
        }
        assert_eq!(d.lang.highlight, Some(0), "and back to the first");
        assert_eq!(d.lang.scroll, 0);
        assert!(d.lang.open, "paging must not dismiss the list");
    }

    /// The wheel scrolls within bounds and never past either end.
    #[test]
    fn the_wheel_scrolls_within_bounds() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let n = languages.len();
        let max = n - DROPDOWN_VISIBLE;

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.scroll = 0;
        d.on_list_edit(HOT_LANGUAGE, Edit::Scroll(5), &[], languages);
        assert_eq!(d.lang.scroll, 0, "cannot scroll above the first option");

        d.on_list_edit(HOT_LANGUAGE, Edit::Scroll(-1000), &[], languages);
        assert_eq!(d.lang.scroll, max, "cannot scroll past the last screenful");
    }

    /// The stepper buttons move the list, and do not close it.
    ///
    /// Closing is the danger, not the scrolling: every other click in the
    /// dialog dismisses whichever list is not the one clicked, and a
    /// scrollbar button is a click on the list itself rather than on another
    /// control. Routed through the ordinary path it would have shut the list
    /// on the first press — the one interaction the scrollbar exists for.
    #[test]
    fn the_stepper_buttons_scroll_without_closing_the_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();

        d.on_click(HOT_LANGUAGE, &[], languages);
        assert!(d.lang.open, "the list must be open to start with");
        d.lang.scroll = 4;

        d.on_click(HOT_SCROLL_UP, &[], languages);
        assert!(d.lang.open, "a stepper must not dismiss the list");
        assert_eq!(d.lang.scroll, 3, "up steps one line toward the top");

        d.on_click(HOT_SCROLL_DOWN, &[], languages);
        assert!(d.lang.open);
        assert_eq!(d.lang.scroll, 4, "down steps one line back");
    }

    /// The track pages, and a page overlaps by a line so the eye can carry
    /// across the jump.
    #[test]
    fn the_track_pages_the_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.scroll = 0;

        d.on_click(HOT_SCROLL_PAGE_DOWN, &[], languages);
        assert!(d.lang.open, "paging must not dismiss the list");
        assert_eq!(
            d.lang.scroll, DROPDOWN_PAGE,
            "a page is a screenful less the overlapping line"
        );

        d.on_click(HOT_SCROLL_PAGE_UP, &[], languages);
        assert_eq!(d.lang.scroll, 0, "and back again");
    }

    /// Clicking the thumb does not itself scroll. The press began a drag; a
    /// jump on the release would move the list out from under the drag that
    /// was already following the pointer.
    #[test]
    fn clicking_the_thumb_does_not_jump_the_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.scroll = 6;
        d.on_click(HOT_SCROLL_THUMB, &[], languages);
        assert!(d.lang.open, "the thumb must not dismiss the list");
        assert_eq!(
            d.lang.scroll, 6,
            "the thumb scrolls by dragging, not by clicking"
        );
    }

    /// A drag names a position outright, and is clamped to the list.
    #[test]
    fn a_drag_positions_the_list_and_clamps() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let n = languages.len();
        let max = n - DROPDOWN_VISIBLE;

        d.on_click(HOT_LANGUAGE, &[], languages);
        d.on_list_edit(HOT_LANGUAGE, Edit::ScrollTo(3), &[], languages);
        assert_eq!(d.lang.scroll, 3, "a drag sets the offset directly");

        d.on_list_edit(HOT_LANGUAGE, Edit::ScrollTo(9_999), &[], languages);
        assert_eq!(
            d.lang.scroll, max,
            "a drag cannot run past the last screenful"
        );
    }

    /// The log-level list has two options, so it never produces scrollbar
    /// ids at all — and if one arrived anyway, it must not silently scroll
    /// the language list that is not even open.
    #[test]
    fn a_scrollbar_click_with_no_list_open_does_nothing() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();

        assert!(!d.lang.open, "no list open to begin with");
        d.lang.scroll = 7;
        for id in [
            HOT_SCROLL_UP,
            HOT_SCROLL_DOWN,
            HOT_SCROLL_PAGE_UP,
            HOT_SCROLL_PAGE_DOWN,
        ] {
            d.on_click(id, &[], languages);
            assert_eq!(
                d.lang.scroll, 7,
                "id {id:?} moved a list that was not showing"
            );
            assert!(!d.lang.open, "id {id:?} opened a list");
        }
    }

    /// The scrollbar drives whichever list is open, not the language list.
    ///
    /// The handler used to route every bar click to `HOT_LANGUAGE`, justified
    /// by the language list being the only one long enough to be drawn with a
    /// bar. That is true of what this build ships and it was an assumption
    /// about the option counts of the other two lists, held in a different
    /// function from the one that decides them — so a build that shipped more
    /// themes would have had a scrollbar that scrolled the wrong list.
    ///
    /// The theme list stands in for that build here: `option_count` sizes it
    /// from the slice it is given, so a long slice is exactly what "a build
    /// with many themes" means to this code.
    #[test]
    fn the_scrollbar_drives_the_list_that_is_open() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let themes: Vec<Builtin> = (0..DROPDOWN_VISIBLE * 2)
            .map(|i| Builtin::ALL[i % Builtin::ALL.len()])
            .collect();

        d.on_click(HOT_THEME, &themes, languages);
        assert!(
            d.theme_list.open,
            "the theme list must be open to start with"
        );
        d.theme_list.scroll = 0;
        d.lang.scroll = 7;

        d.on_click(HOT_SCROLL_PAGE_DOWN, &themes, languages);
        assert!(d.theme_list.open, "paging must not dismiss the list");
        assert_eq!(
            d.theme_list.scroll, DROPDOWN_PAGE,
            "the bar belongs to the list on screen, and that is the theme list"
        );
        assert_eq!(
            d.lang.scroll, 7,
            "the language list is not showing and must not have moved"
        );
    }

    /// Opening the language list scrolls to the current choice.
    ///
    /// With twenty-four languages a list that always opened at the top would
    /// hide the current selection for most of them, and the user would have
    /// no sign of where it went.
    #[test]
    fn opening_the_list_reveals_the_current_choice() {
        let mut cfg = Config::default();
        let languages = crate::lang::Locale::available();
        let last = languages.len() - 1;
        cfg.language = languages[last].code.to_owned();

        let mut d = SettingsDraft::from_config(&cfg);
        d.on_click(HOT_LANGUAGE, &[], languages);
        assert_eq!(d.lang.highlight, Some(last));
        assert!(
            last >= d.lang.scroll && last < d.lang.scroll + DROPDOWN_VISIBLE,
            "the selected language must be visible when the list opens"
        );
    }

    /// Only one list is open at a time.
    #[test]
    fn opening_one_list_closes_the_other() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        d.on_click(HOT_LANGUAGE, &[], languages);
        assert!(d.lang.open);
        d.on_click(HOT_LOG_LEVEL, &[], languages);
        assert!(d.log.open);
        assert!(!d.lang.open, "two lists open at once would overlap");
    }

    /// Option ids round-trip and cannot collide with a control's id.
    ///
    /// The hit test tells them apart by number alone, so an overlap would
    /// route a click on a list option to whichever control shared its id.
    #[test]
    fn option_ids_are_distinct_from_control_ids() {
        for base in [HOT_LANGUAGE, HOT_LOG_LEVEL] {
            for index in 0..30usize {
                let id = base.option(index);
                assert_eq!(id.split_option(), Some((base, index)));
            }
        }
        // Every control in this dialog is a plain small integer.
        for id in [
            HOT_INTERVAL,
            HOT_LANGUAGE,
            HOT_LOG_LEVEL,
            HOT_OK,
            HOT_CANCEL,
            HOT_THEME,
        ] {
            assert_eq!(
                id.split_option(),
                None,
                "control id {id:?} parses as an option id"
            );
        }
    }

    /// Every language offers a non-empty Latin label to scan the list by.
    #[test]
    fn every_language_option_has_a_latin_label() {
        for l in crate::lang::Locale::available() {
            let item = language_item(l);
            assert!(
                !item.label.is_empty(),
                "{}: every option needs a Latin label",
                l.code
            );
        }
    }

    /// A click on an option chooses it — the path the mouse actually takes.
    ///
    /// Regression test for a dropdown that could be opened and scrolled but
    /// not clicked: keyboard selection worked, the mouse did nothing.
    #[test]
    fn clicking_an_option_selects_it() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();

        d.on_click(HOT_LANGUAGE, &[], languages);
        assert!(d.lang.open);

        let target = 3usize;
        let click = HOT_LANGUAGE.option(target);
        let action = d.on_click(click, &[], languages);
        assert!(matches!(action, Action::Changed), "the click must be taken");
        assert_eq!(d.language, languages[target].code);
        assert!(!d.lang.open);
    }

    /// The pointer moving over an option highlights it.
    #[test]
    fn hovering_moves_the_highlight() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        d.on_click(HOT_LANGUAGE, &[], languages);

        d.on_list_edit(HOT_LANGUAGE, Edit::Hover(4), &[], languages);
        assert_eq!(d.lang.highlight, Some(4));
        assert_eq!(d.language, cfg.language, "hovering must not choose");

        // And a click then takes what the pointer is showing.
        d.on_click(HOT_LANGUAGE.option(4), &[], languages);
        assert_eq!(d.language, languages[4].code);
    }

    /// Hovering never scrolls: the option is under the cursor, so it is
    /// already visible, and scrolling would slide the list out from under it.
    #[test]
    fn hovering_does_not_scroll() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        d.on_click(HOT_LANGUAGE, &[], languages);
        d.lang.scroll = 5;
        d.on_list_edit(HOT_LANGUAGE, Edit::Hover(6), &[], languages);
        assert_eq!(d.lang.scroll, 5);
    }

    /// A click on bare background dismisses an open list.
    ///
    /// Regression test for a dropdown that stayed open when the user clicked
    /// anywhere else in the window. The window procedure dropped clicks that
    /// matched no hotspot, so the draft never heard about them and the list
    /// hung over a dialog that had plainly moved on.
    #[test]
    fn clicking_bare_background_closes_an_open_list() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        let before = d.language.clone();

        d.on_click(HOT_LANGUAGE, &[], languages);
        assert!(d.lang.open);

        let action = d.on_click(HotspotId::NOTHING, &[], languages);
        assert!(!d.lang.open, "a click on nothing must dismiss the list");
        assert!(
            matches!(action, Action::Changed),
            "the dismissal needs a repaint"
        );
        assert_eq!(d.language, before, "dismissing must not choose");
    }

    /// Dismissing also drops the highlight, so reopening starts from the
    /// current selection rather than wherever the keyboard last was.
    #[test]
    fn dismissing_a_list_clears_its_highlight() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let languages = crate::lang::Locale::available();
        d.on_click(HOT_LANGUAGE, &[], languages);
        d.on_list_edit(HOT_LANGUAGE, Edit::Highlight(Move::Next), &[], languages);
        assert!(d.lang.highlight.is_some());

        d.on_click(HotspotId::NOTHING, &[], languages);
        assert_eq!(d.lang.highlight, None);
    }

    /// Subordinate switches carry a nesting depth, not spaces in their label.
    ///
    /// The tab ring of the real dialog is the drawing order and nothing else:
    /// the interval field, the three lists, the two switches, then OK and
    /// Cancel — and the twelve event switches only while they are on screen.
    ///
    /// `ui::focus` is tested against synthetic rows; this pins the ring the
    /// user actually walks, including the one case where the row list changes
    /// shape under it. Focus on a switch that has just been hidden would be a
    /// ring drawn nowhere and a Space that does nothing.
    #[test]
    fn the_tab_ring_follows_the_dialog_and_drops_hidden_switches() {
        let locale = crate::lang::Locale::english();
        let theme = crate::ui::theme::Theme::default();
        let languages = crate::lang::Locale::available();

        let mut cfg = Config::default();
        cfg.notifications_enabled = false;
        let off = SettingsDraft::from_config(&cfg);
        let quiet_rows = off.rows(locale, &theme, &[], languages);
        let quiet: Vec<_> = crate::ui::focus::ring(&quiet_rows).collect();

        cfg.notifications_enabled = true;
        let on = SettingsDraft::from_config(&cfg);
        let loud_rows = on.rows(locale, &theme, &[], languages);
        let loud: Vec<_> = crate::ui::focus::ring(&loud_rows).collect();

        assert_eq!(
            quiet,
            vec![
                HOT_INTERVAL,
                HOT_LANGUAGE,
                HOT_THEME,
                HOT_LOG_LEVEL,
                HOT_START_MINIMIZED,
                HOT_NOTIFICATIONS,
                HOT_OK,
                HOT_CANCEL,
            ],
            "with notifications off the ring is the visible controls, in order"
        );
        assert_eq!(
            loud.len(),
            quiet.len() + SettingsDraft::EVENT_SWITCHES.len(),
            "every event switch joins the ring when it is shown"
        );
        for (_, id) in SettingsDraft::EVENT_SWITCHES {
            assert!(loud.contains(&id), "a shown switch is reachable by Tab");
            assert!(!quiet.contains(&id), "a hidden switch is not");
        }
        // Cancel is last, so the ring closing round to the first control is
        // what makes the dialog's very first Tab land on the interval field.
        assert_eq!(
            crate::ui::focus::initial(&on.rows(locale, &theme, &[], languages)),
            Some(HOT_CANCEL)
        );
    }

    /// The indent used to be four spaces prepended to the text, which moved
    /// only the label and left every checkbox box in one column; it then
    /// disappeared entirely without any test noticing. A depth the layout can
    /// see moves the whole row and cannot be lost in string handling.
    #[test]
    fn event_switches_are_nested_under_the_master_switch() {
        let mut cfg = Config::default();
        cfg.notifications_enabled = true;
        let d = SettingsDraft::from_config(&cfg);
        let locale = crate::lang::Locale::english();
        let theme = crate::ui::theme::Theme::default();
        let rows = d.rows(locale, &theme, &[], crate::lang::Locale::available());

        let master = rows
            .iter()
            .find_map(|r| match r {
                Row::Checkbox { depth, id, .. } if *id == HOT_NOTIFICATIONS => Some(*depth),
                _ => None,
            })
            .expect("the master switch is present");
        assert_eq!(master, 0, "the master switch is not nested");

        for (_, id) in SettingsDraft::EVENT_SWITCHES {
            let (depth, label) = rows
                .iter()
                .find_map(|r| match r {
                    Row::Checkbox {
                        depth,
                        id: rid,
                        label,
                        ..
                    } if *rid == id => Some((*depth, label.clone())),
                    _ => None,
                })
                .expect("every event switch is present");
            assert_eq!(depth, 1, "event switches sit one level in");
            assert!(
                !label.starts_with(' '),
                "the indent must be structural, not spaces in {label:?}"
            );
        }
    }

    /// English is labelled once, not "English (English)".
    #[test]
    fn an_option_does_not_repeat_itself() {
        let en = language_item(&crate::lang::Locale::available()[0]);
        assert_eq!(en.label, "English");
        assert!(en.native.is_empty());
    }

    /// The chosen level reaches the config, and a rejected interval blocks it
    /// like every other field.
    #[test]
    fn log_level_round_trips_and_respects_validation() {
        let mut cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        d.log_level = LogLevel::Debug;
        assert!(d.apply_to(&mut cfg, locale()));
        assert_eq!(cfg.log_level, LogLevel::Debug);

        let mut d = SettingsDraft::from_config(&cfg);
        d.log_level = LogLevel::Normal;
        set_interval(&mut d, "1");
        assert!(!d.apply_to(&mut cfg, locale()));
        assert_eq!(
            cfg.log_level,
            LogLevel::Debug,
            "a rejected interval must not let the level through"
        );
    }

    /// The new switches reach the config on OK.
    #[test]
    fn every_switch_reaches_the_config() {
        let mut cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        // Flip all of them away from their defaults at once.
        for (_, id) in SettingsDraft::EVENT_SWITCHES {
            d.on_click(id, &[], &[]);
        }
        assert!(d.apply_to(&mut cfg, locale()));

        assert!(!cfg.on_power_failure);
        assert!(!cfg.on_power_restored);
        assert!(!cfg.on_low_battery);
        assert!(!cfg.on_device_fault);
        assert!(!cfg.on_overload);
        assert!(!cfg.on_voltage_out_of_range);
        assert!(!cfg.on_frequency_out_of_range);
        assert!(!cfg.on_runtime_limit);
        assert!(!cfg.on_connection_lost);
        assert!(!cfg.on_connection_restored);
        // No exceptions: everything ships on, so flipping turns everything
        // off. AVR used to default off and was the one asymmetry here.
        assert!(!cfg.on_boost_started);
        assert!(!cfg.on_boost_ended);
    }

    /// The connection events sit at the bottom of the list.
    ///
    /// They are a different kind of problem from everything above them:
    /// every other switch is about something the UPS reported concerning the
    /// power, while these two are about the utility's own link to the device.
    /// Placed mid-list they split the power events into two groups either
    /// side of an unrelated interruption, which is what the ordering existed
    /// to avoid.
    #[test]
    fn connection_events_come_last() {
        let ids: Vec<_> = SettingsDraft::EVENT_SWITCHES
            .iter()
            .map(|(_, id)| *id)
            .collect();
        let n = ids.len();
        assert_eq!(
            &ids[n - 2..],
            &[HOT_CONNECTION_LOST, HOT_CONNECTION_RESTORED],
            "the connection pair must be the last two entries"
        );
    }

    /// Voltage and frequency are offered separately.
    ///
    /// They were briefly one control on the grounds that this firmware
    /// reports no frequency value, only the flag. That is a limit on the
    /// detail each event carries, not a reason to merge two conditions with
    /// different causes and different remedies.
    #[test]
    fn mains_voltage_and_frequency_are_separate_switches() {
        let ids: Vec<_> = SettingsDraft::EVENT_SWITCHES
            .iter()
            .map(|(_, id)| *id)
            .collect();
        assert!(ids.contains(&HOT_VOLTAGE_OUT_OF_RANGE));
        assert!(ids.contains(&HOT_FREQUENCY_OUT_OF_RANGE));
        assert_ne!(HOT_VOLTAGE_OUT_OF_RANGE, HOT_FREQUENCY_OUT_OF_RANGE);
    }

    /// Clicking the interval field reports a change so the dialog repaints.
    /// It has nothing else to do: keyboard focus was granted by the press that
    /// produced this click and lives in the window, for every kind of control
    /// alike. The draft holding a second copy of it is what let the click-away
    /// case diverge, and it no longer holds one.
    #[test]
    fn clicking_the_interval_field_reports_change() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        let action = d.on_click(HOT_INTERVAL, &[], &[]);
        assert!(matches!(action, Action::Changed));
    }

    #[test]
    fn interval_field_accepts_only_digits() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "");
        draft.on_edit(HOT_INTERVAL, Edit::Insert('4'));
        draft.on_edit(HOT_INTERVAL, Edit::Insert('x'));
        draft.on_edit(HOT_INTERVAL, Edit::Insert('2'));
        assert_eq!(draft.interval.text(), "42");
        draft.on_edit(HOT_INTERVAL, Edit::Backspace);
        assert_eq!(draft.interval.text(), "4");
    }

    /// Arrow keys, Home and End move the caret through `on_edit`, and typing
    /// inserts at the caret rather than the end. This exercises the whole
    /// window-edit-to-field path, not just the `TextField` in isolation.
    #[test]
    fn caret_keys_move_and_insert_at_the_caret() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "300");
        draft.on_edit(HOT_INTERVAL, Edit::CaretHome { extend: false });
        draft.on_edit(HOT_INTERVAL, Edit::CaretRight { extend: false });
        draft.on_edit(HOT_INTERVAL, Edit::Insert('9'));
        assert_eq!(draft.interval.text(), "3900");
        assert_eq!(draft.interval.cursor(), 2);
    }

    /// Delete removes the character to the right of the caret.
    #[test]
    fn delete_removes_forward() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "300");
        draft.on_edit(HOT_INTERVAL, Edit::CaretHome { extend: false });
        draft.on_edit(HOT_INTERVAL, Edit::Delete);
        assert_eq!(draft.interval.text(), "00");
    }

    /// Shift+arrow selects, and typing replaces the selection.
    #[test]
    fn selection_is_replaced_by_typing() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "3000");
        draft.on_edit(HOT_INTERVAL, Edit::SelectAll);
        assert_eq!(draft.interval.selection(), Some((0, 4)));
        draft.on_edit(HOT_INTERVAL, Edit::Insert('5'));
        assert_eq!(draft.interval.text(), "5");
        assert_eq!(draft.interval.selection(), None);
    }

    /// A mouse click resolves to a caret index via `CaretTo`, and a shifted
    /// one extends the selection from where the caret was.
    #[test]
    fn caret_to_places_and_extends() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "3000");
        draft.on_edit(
            HOT_INTERVAL,
            Edit::CaretTo {
                index: 1,
                extend: false,
            },
        );
        assert_eq!(draft.interval.cursor(), 1);
        assert_eq!(draft.interval.selection(), None);
        draft.on_edit(
            HOT_INTERVAL,
            Edit::CaretTo {
                index: 4,
                extend: true,
            },
        );
        assert_eq!(draft.interval.selection(), Some((1, 4)));
    }

    /// Enter and Escape are the field's own commit and cancel, not text edits.
    #[test]
    fn enter_commits_and_escape_cancels() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        assert!(matches!(
            draft.on_edit(HOT_INTERVAL, Edit::Commit),
            Action::Commit
        ));
        assert!(matches!(
            draft.on_edit(HOT_INTERVAL, Edit::Cancel),
            Action::Cancel
        ));
    }

    #[test]
    fn toggling_master_switch_flips_state() {
        let cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        let before = draft.notifications_enabled;
        draft.on_click(HOT_NOTIFICATIONS, &[], &[]);
        assert_ne!(draft.notifications_enabled, before);
    }

    #[test]
    fn start_minimized_round_trips_through_the_draft() {
        let mut cfg = Config::default();
        cfg.start_minimized = true;
        let mut draft = SettingsDraft::from_config(&cfg);
        assert!(draft.start_minimized, "draft must start from the config");

        draft.on_click(HOT_START_MINIMIZED, &[], &[]);
        assert!(!draft.start_minimized);
        assert!(draft.apply_to(&mut cfg, locale()));
        assert!(!cfg.start_minimized, "the choice must reach the config");
    }

    /// A rejected interval must not let this through either, or the user gets
    /// a partial save they never confirmed.
    #[test]
    fn start_minimized_is_not_applied_when_validation_fails() {
        let mut cfg = Config::default();
        cfg.start_minimized = true;
        let mut draft = SettingsDraft::from_config(&cfg);
        draft.start_minimized = false;
        set_interval(&mut draft, "1");
        assert!(!draft.apply_to(&mut cfg, locale()));
        assert!(cfg.start_minimized, "partial save leaked through");
    }

    #[test]
    fn accepts_interval_in_range() {
        let mut cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "5000");
        assert!(draft.apply_to(&mut cfg, locale()));
        assert_eq!(cfg.poll_interval_ms, 5000);
        assert!(draft.error.is_none());
    }

    #[test]
    fn rejects_out_of_range_without_touching_config() {
        let mut cfg = Config::default();
        let before = cfg.poll_interval_ms;
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "50");
        draft.theme = "changed".into();

        assert!(!draft.apply_to(&mut cfg, locale()));
        assert!(draft.error.is_some());
        // A rejected interval must not let the other fields through, or the
        // user gets a partial save they never confirmed.
        assert_eq!(cfg.poll_interval_ms, before);
        assert_ne!(cfg.theme, "changed");
    }

    #[test]
    fn rejects_non_numeric() {
        let mut cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        for bad in ["", "abc", "10.5", "-1"] {
            set_interval(&mut draft, bad);
            assert!(!draft.apply_to(&mut cfg, locale()), "should reject {bad:?}");
        }
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let mut cfg = Config::default();
        let mut draft = SettingsDraft::from_config(&cfg);
        set_interval(&mut draft, "  3000  ");
        assert!(draft.apply_to(&mut cfg, locale()));
        assert_eq!(cfg.poll_interval_ms, 3000);
    }

    /// A list edit that arrives with no list open changes nothing.
    ///
    /// Every other list test opens the list first, which is what the dialog
    /// does — but the keyboard path does not check: `on_key_down` translates
    /// Enter into `Edit::Commit` and hands it here whenever the focus ring is
    /// on a dropdown, open or not. Without the guard, Enter on a closed
    /// language list would select whatever index the last highlight left
    /// behind, silently, with no list on screen to say what happened.
    #[test]
    fn a_closed_list_ignores_every_edit() {
        let cfg = Config::default();
        let languages = crate::lang::Locale::available();

        for edit in [
            Edit::Commit,
            Edit::Cancel,
            Edit::Highlight(Move::Next),
            Edit::Hover(3),
            Edit::Scroll(1),
            Edit::ScrollTo(2),
        ] {
            let mut d = SettingsDraft::from_config(&cfg);
            assert_eq!(
                d.on_list_edit(HOT_LANGUAGE, edit, &[], languages),
                Action::None,
                "{edit:?} on a closed list"
            );
            assert_eq!(d.language, cfg.language);
            assert_eq!(d.lang.highlight, None);
            assert_eq!(d.lang.scroll, 0);
        }
    }

    /// An edit aimed at a control that is not a list changes nothing.
    ///
    /// Pinned as behaviour rather than as a claim about which guard answers,
    /// and that distinction was measured: replacing the `option_count` guard
    /// with a permissive default leaves this green, because `list_ref` two
    /// lines below refuses the same ids for the same reason. "Is this id a
    /// list" is answered twice in this module — by `option_count`, which the
    /// router also asks, and by the `list_ref`/`list_mut` pair — and the two
    /// name the same three ids. That is a second source of truth, recorded
    /// here as a finding rather than repaired: it is not on this cycle's list,
    /// and the repair is a change to a signature the router depends on.
    #[test]
    fn an_edit_aimed_at_something_that_is_not_a_list_does_nothing() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        assert_eq!(
            d.on_list_edit(HOT_INTERVAL, Edit::Commit, &[], &[]),
            Action::None
        );
    }

    /// A list with no options at all commits nothing.
    ///
    /// The count comes from the caller's own tables, so an empty one is
    /// reachable rather than hypothetical, and both paths that index by the
    /// highlight — Commit and the arrow tail — subtract one from it. Without
    /// the guard that subtraction underflows, and on an unsigned index the
    /// result is not a wrong option but a panic in the message loop.
    #[test]
    fn an_empty_list_is_not_indexed() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        d.lang.open = true;

        assert_eq!(
            d.on_list_edit(HOT_LANGUAGE, Edit::Commit, &[], &[]),
            Action::None
        );
        assert_eq!(
            d.on_list_edit(HOT_LANGUAGE, Edit::Highlight(Move::Next), &[], &[]),
            Action::None
        );
        assert_eq!(d.language, cfg.language);
    }

    /// Enter takes the highlight, and takes it clamped.
    ///
    /// Two facts in one place because they share the expression. The fallback
    /// answers Enter pressed on a list opened but never arrowed through —
    /// which cannot happen through `toggle_list`, since that sets the
    /// highlight to the current choice, and is exactly why it needs saying:
    /// the fallback is reached only when some other path leaves the highlight
    /// unset, and then it must land on a real option rather than nowhere. The
    /// clamp answers a highlight left over from a longer list, which the
    /// language list becomes whenever the available set changes under it.
    #[test]
    fn enter_takes_the_highlight_or_the_first_option_and_never_runs_off_the_end() {
        let cfg = Config::default();
        let languages = crate::lang::Locale::available();

        let mut unmoved = SettingsDraft::from_config(&cfg);
        unmoved.lang.open = true;
        unmoved.lang.highlight = None;
        assert_eq!(
            unmoved.on_list_edit(HOT_LANGUAGE, Edit::Commit, &[], languages),
            Action::Changed
        );
        let first = languages
            .first()
            .expect("the language table is never empty");
        assert_eq!(unmoved.language, first.code);

        let mut stale = SettingsDraft::from_config(&cfg);
        stale.lang.open = true;
        stale.lang.highlight = Some(999);
        assert_eq!(
            stale.on_list_edit(HOT_LANGUAGE, Edit::Commit, &[], languages),
            Action::Changed
        );
        let last = languages.last().expect("the language table is never empty");
        assert_eq!(stale.language, last.code);
    }

    /// The pointer cannot highlight an option past the end of the list.
    ///
    /// The window builds hover events from the hotspots the painter
    /// registered, so an index past the end means the painted list and the
    /// draft disagree about how long it is — a repaint in flight. The draft
    /// clamps rather than trusting the geometry, because it is the side that
    /// knows the length.
    #[test]
    fn hovering_past_the_end_lands_on_the_last_option() {
        let cfg = Config::default();
        let languages = crate::lang::Locale::available();
        let mut d = SettingsDraft::from_config(&cfg);
        d.on_click(HOT_LANGUAGE, &[], languages);

        d.on_list_edit(HOT_LANGUAGE, Edit::Hover(999), &[], languages);
        assert_eq!(d.lang.highlight, Some(languages.len() - 1));
    }

    /// An edit that belongs to a text field does nothing to an open list.
    ///
    /// The two vocabularies overlap — both arrive as [`Edit`] — and the list
    /// path answers only the variants that mean something to a list. A typed
    /// digit reaching the list would be reported as a change and repaint the
    /// dialog for a keystroke that did nothing.
    #[test]
    fn a_field_edit_does_not_disturb_an_open_list() {
        let cfg = Config::default();
        let languages = crate::lang::Locale::available();
        let mut d = SettingsDraft::from_config(&cfg);
        d.on_click(HOT_LANGUAGE, &[], languages);
        let before = d.lang.highlight;

        assert_eq!(
            d.on_list_edit(HOT_LANGUAGE, Edit::Insert('7'), &[], languages),
            Action::None
        );
        assert_eq!(d.lang.highlight, before);
    }

    /// Opening a list whose current value is not among its options starts at
    /// the first one.
    ///
    /// Reachable from the INI: a theme code nobody recognises falls back to
    /// the dark theme for rendering, but the draft still holds the text that
    /// was written. Opening the list must then show something rather than
    /// nothing.
    #[test]
    fn opening_a_list_on_an_unknown_value_starts_at_the_top() {
        let cfg = Config::default();
        let themes = &Builtin::ALL;
        let mut d = SettingsDraft::from_config(&cfg);
        d.theme = "no such theme".into();

        d.on_click(HOT_THEME, themes, &[]);
        assert!(d.theme_list.open);
        assert_eq!(d.theme_list.highlight, Some(0));
        assert_eq!(d.theme_list.scroll, 0);
    }

    /// A click on nothing, with nothing open, asks for nothing.
    ///
    /// The counterpart of `clicking_bare_background_closes_an_open_list`: that
    /// one pins the repaint a dismissal owes, this one pins that a click on
    /// the background is not itself a reason to repaint. Without both, an
    /// arm that always answered `Changed` would look correct.
    #[test]
    fn a_click_on_nothing_with_nothing_open_asks_for_nothing() {
        let cfg = Config::default();
        let mut d = SettingsDraft::from_config(&cfg);
        assert_eq!(
            d.on_click(HotspotId::NOTHING, &[], &[]),
            Action::None,
            "no list was open, so there is nothing to redraw"
        );
    }
}
