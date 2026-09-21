//! Everything the user sees, and everything that decides what they see.
//!
//! # Layout of this module
//!
//! Two groups of files, and the division between them is the one that matters:
//! what to show, and how to draw it.
//!
//! **What to show** — no Win32 at all, testable without a window:
//!
//! - [`panel`] and [`settings`] build the row list for the status panel and
//!   the settings dialog. [`focus`] derives the tab order from that list;
//!   [`winstate`] says which windows should exist after a given action;
//!   [`mod@format`] turns readings into the strings they print as.
//! - [`row`] is the vocabulary all of the above speak: what a row can be, the
//!   gaps between rows, the item inside a dropdown, the edits a control
//!   reports back, and the hotspot id scheme that lets a click name what it
//!   hit.
//! - [`theme`] is the palette and the metrics those rows are laid out and
//!   painted with.
//!
//! **How to draw it** — Win32 and GDI:
//!
//! - [`text`] — fonts, script runs, and the measuring *and* drawing of text.
//!   The two are in one file deliberately: a string measured in one font and
//!   drawn in another is the defect this area keeps producing.
//! - [`layout`] — the arithmetic. Column widths, the content size, the
//!   fingerprint that keys its cache.
//! - [`scrollbar`] — where a dropdown's scrollbar parts are, and the inverse
//!   that turns a drag back into an offset.
//! - [`textfield`] — caret, selection and editing for the one editable field.
//! - [`window`] — the window itself: creation, state, the window procedure,
//!   and the painter that walks the rows.
//! - [`tray`], [`tray_window`] and [`appicon`] — the notification icon, the
//!   hidden window that receives its messages and the shell broadcasts, and
//!   the icon artwork itself.
//!
//! Tests live beside the code they cover, and the imports at the top of each
//! `mod tests` are a second statement of the dependency list above — one the
//! compiler checks.
//!
//! There used to be a `panel_win32` directory holding the last five of these.
//! It added a level of nesting and a second `mod.rs` for a boundary that was
//! not real: `panel.rs` builds rows, `window.rs` draws them, and neither is
//! more or less "the UI" than the other. Flattening it also shortened every
//! path outside this module from `ui::row::Row` to `ui::row::Row`.

pub(crate) mod appicon;
pub(crate) mod focus;
pub(crate) mod format;
pub(crate) mod gdi;
pub(crate) mod layout;
pub(crate) mod panel;
pub(crate) mod rect;
pub(crate) mod row;
pub(crate) mod scrollbar;
pub(crate) mod settings;
pub(crate) mod text;
pub(crate) mod textfield;
pub(crate) mod theme;
pub(crate) mod tray;
pub(crate) mod tray_window;
pub(crate) mod window;
pub(crate) mod winstate;

/// Every timer id this program sets, in one place.
///
/// Timer ids are scoped to the window they are set on, so two windows may
/// legitimately use the same number — and these two did, both `1`, declared in
/// different files, each with a comment asserting that its own window had no
/// other timers. Both assertions were true and nothing held them true.
///
/// Naming the ids together is what makes a second timer on a window a visible
/// collision instead of a silent one: `SetTimer` with an id that already exists
/// does not fail, it restarts the existing timer, so the symptom would be one
/// of the two features quietly ceasing to fire with nothing anywhere saying so.
pub(crate) mod timers {
    /// The panel's one-second refresh, set on the tray window.
    pub(crate) const REFRESH: usize = 1;
    /// The caret blink, set on whichever panel-class window holds the focused
    /// text field.
    pub(crate) const CARET: usize = 2;

    /// Every id above, named, for the uniqueness test.
    #[cfg(test)]
    pub(crate) const ALL: [(&str, usize); 2] = [("REFRESH", REFRESH), ("CARET", CARET)];
}

#[cfg(test)]
mod timer_registry_tests {
    use super::timers;

    /// No two timer ids collide, and none is zero.
    ///
    /// Zero is a legal id to pass but not one to use: `SetTimer` reports
    /// failure by returning zero, so a timer that asked for id 0 could not be
    /// told apart from one that was never created.
    #[test]
    fn no_two_timer_ids_collide() {
        for (i, (name_a, a)) in timers::ALL.iter().enumerate() {
            assert_ne!(*a, 0, "{name_a} is zero, which is SetTimer's failure value");
            for (name_b, b) in &timers::ALL[i + 1..] {
                assert_ne!(a, b, "{name_a} and {name_b} share id {a:?}");
            }
        }
    }
}

/// Default UI font size in points.
///
/// 11.25pt is what the utility drew at before the size became configurable:
/// the old hard-coded height of -15 pixels at 96 DPI. Stated in points so the
/// DPI assumption is visible rather than baked into a pixel count.
pub(crate) const DEFAULT_FONT_POINTS: f32 = 11.25;

/// The dots per inch of one monitor.
///
/// A `u32` in a hundred-odd lines of ten files, and in signatures beside other
/// `u32`s that mean entirely different things — a hotspot id, a poll interval,
/// a colour channel. Nothing about the parameter type said which was which, so
/// `create_font(family, dpi, points)` and `frame(client, dpi)` were held to
/// their meaning by argument order and by the word `dpi` in the name.
///
/// More than that, it is a number that appears in exactly two pieces of
/// arithmetic in the whole tree, and both are the kind that go wrong quietly:
/// scaling an authored pixel count, and turning a point size into a pixel
/// height. Both used to be written where they were needed, each with its own
/// `as` cast and its own rounding decision. They live here now, which is the
/// same rule this tree applies to every other quantity — one place, not
/// wherever the need arose.
///
/// [`Dpi::raw`] exists for the Win32 calls that take a bare `u32`
/// (`AdjustWindowRectExForDpi`, `GetSystemMetricsForDpi`). That is a boundary,
/// not a back door: nothing inside this crate needs the number, because
/// everything inside this crate that would do arithmetic with it has a method
/// here instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dpi(u32);

impl Dpi {
    /// The DPI every metric in [`theme`] is authored against.
    ///
    /// Not a default in the sense of a fallback — it is the scale the numbers
    /// in the theme *mean*, and [`Dpi::scale_96`] is named for it.
    pub(crate) const BASELINE: Dpi = Dpi(96);

    /// A DPI as Windows reports it.
    pub(crate) fn new(raw: u32) -> Dpi {
        Dpi(raw)
    }

    /// The number itself, for the Win32 entry points that take one.
    pub(crate) fn raw(self) -> u32 {
        self.0
    }

    /// One 96-DPI pixel count, scaled to this monitor and rounded to nearest.
    ///
    /// Integer arithmetic throughout: `(v * dpi + 48) / 96` is `v * dpi / 96`
    /// rounded half up, with no float anywhere to be compared for equality
    /// later. Rounding rather than the truncation the `f32` version ended up
    /// performing at its call sites — truncation loses up to a pixel on every
    /// metric and always downward, so a 150 % monitor got a 31-pixel row where
    /// 31.5 was asked for, and every one of two dozen rows was a little
    /// tighter than authored.
    pub(crate) fn scale_96(self, v: i32) -> i32 {
        // `self.0` comes from `GetDpiForWindow` and is a small positive
        // number; the products here are nowhere near overflow at any real
        // monitor scale.
        (v * self.0 as i32 + 48) / 96
    }

    /// A point size as a pixel height on this monitor.
    ///
    /// Positive, which a caller wanting a `CreateFontW` height must negate —
    /// Win32 reads a negative height as the character height rather than the
    /// cell height, and that sign is a fact about `CreateFontW` rather than
    /// about typography. `font_height` in [`text`] is the one place that
    /// applies it.
    pub(crate) fn points_to_pixels(self, points: f32) -> i32 {
        (points * self.0 as f32 / 72.0).round() as i32
    }
}

#[cfg(test)]
mod hotspot_registry_tests {
    use super::row::{
        HotspotId, HOT_BEEPER, HOT_CONFIRM_NO, HOT_CONFIRM_YES, HOT_SELFTEST, HOT_SETTINGS,
    };
    use super::scrollbar::{
        HOT_SCROLL_DOWN, HOT_SCROLL_PAGE_DOWN, HOT_SCROLL_PAGE_UP, HOT_SCROLL_THUMB, HOT_SCROLL_UP,
    };
    use super::settings::{
        HOT_BOOST_ENDED, HOT_BOOST_STARTED, HOT_CANCEL, HOT_CONNECTION_LOST,
        HOT_CONNECTION_RESTORED, HOT_DEVICE_FAULT, HOT_FREQUENCY_OUT_OF_RANGE, HOT_INTERVAL,
        HOT_LANGUAGE, HOT_LOG_LEVEL, HOT_LOW_BATTERY, HOT_NOTIFICATIONS, HOT_OK, HOT_OVERLOAD,
        HOT_POWER_FAILURE, HOT_POWER_RESTORED, HOT_RUNTIME_LIMIT, HOT_START_MINIMIZED, HOT_THEME,
        HOT_VOLTAGE_OUT_OF_RANGE,
    };

    /// Every fixed hotspot id, in one place. The constants live in the module
    /// that owns each control, but nothing else proves that two of them,
    /// edited in different files at different times, never landed on the same
    /// number — a collision that would route one control's click to the other
    /// with no error anywhere. This is that proof.
    const ALL: &[(&str, HotspotId)] = &[
        ("HOT_BEEPER", HOT_BEEPER),
        ("HOT_SETTINGS", HOT_SETTINGS),
        ("HOT_SELFTEST", HOT_SELFTEST),
        ("HOT_CONFIRM_YES", HOT_CONFIRM_YES),
        ("HOT_CONFIRM_NO", HOT_CONFIRM_NO),
        ("HOT_INTERVAL", HOT_INTERVAL),
        ("HOT_NOTIFICATIONS", HOT_NOTIFICATIONS),
        ("HOT_POWER_FAILURE", HOT_POWER_FAILURE),
        ("HOT_POWER_RESTORED", HOT_POWER_RESTORED),
        ("HOT_LOW_BATTERY", HOT_LOW_BATTERY),
        ("HOT_DEVICE_FAULT", HOT_DEVICE_FAULT),
        ("HOT_THEME", HOT_THEME),
        ("HOT_LANGUAGE", HOT_LANGUAGE),
        ("HOT_OK", HOT_OK),
        ("HOT_CANCEL", HOT_CANCEL),
        ("HOT_START_MINIMIZED", HOT_START_MINIMIZED),
        ("HOT_OVERLOAD", HOT_OVERLOAD),
        ("HOT_VOLTAGE_OUT_OF_RANGE", HOT_VOLTAGE_OUT_OF_RANGE),
        ("HOT_RUNTIME_LIMIT", HOT_RUNTIME_LIMIT),
        ("HOT_CONNECTION_LOST", HOT_CONNECTION_LOST),
        ("HOT_LOG_LEVEL", HOT_LOG_LEVEL),
        ("HOT_BOOST_STARTED", HOT_BOOST_STARTED),
        ("HOT_BOOST_ENDED", HOT_BOOST_ENDED),
        ("HOT_FREQUENCY_OUT_OF_RANGE", HOT_FREQUENCY_OUT_OF_RANGE),
        ("HOT_CONNECTION_RESTORED", HOT_CONNECTION_RESTORED),
        ("HotspotId::NOTHING", HotspotId::NOTHING),
        ("HOT_SCROLL_UP", HOT_SCROLL_UP),
        ("HOT_SCROLL_DOWN", HOT_SCROLL_DOWN),
        ("HOT_SCROLL_PAGE_UP", HOT_SCROLL_PAGE_UP),
        ("HOT_SCROLL_PAGE_DOWN", HOT_SCROLL_PAGE_DOWN),
        ("HOT_SCROLL_THUMB", HOT_SCROLL_THUMB),
    ];

    #[test]
    fn no_two_hotspot_ids_collide() {
        for (i, (name_a, a)) in ALL.iter().enumerate() {
            for (name_b, b) in &ALL[i + 1..] {
                assert_ne!(a, b, "{name_a} and {name_b} share id {a:?}");
            }
        }
    }

    /// No fixed control sits in the option band that [`HotspotId::option`]
    /// allocates for the entries inside an open list.
    ///
    /// A collision there would make a control's click read as an option of
    /// some other control. This was two tests: one comparing each id against
    /// the literal `10_000`, one asking `split_option`. They asserted the same
    /// property, and the first did it by writing `OPTION_BASE` out a second
    /// time — the duplicated constant this project treats as debt wherever
    /// else it appears. `HotspotId` no longer offers the comparison at all,
    /// which is the type saying the same thing: the question "is this an
    /// option id" has an answer, and it is not a number to compare against.
    #[test]
    fn no_fixed_id_intrudes_on_the_option_band() {
        for (name, id) in ALL {
            assert!(
                id.split_option().is_none(),
                "{name} = {id:?} decodes as an option id"
            );
        }
    }
}
