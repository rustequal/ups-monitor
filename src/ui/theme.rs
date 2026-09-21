//! Themes, defined in code.
//!
//! Themes used to load from `themes/*.ini` on disk with an embedded default
//! merged underneath. That is gone: the same move already made for the
//! interface strings (compiled into the binary) is now made for themes.
//! There is no `themes/` directory, no file to travel with the exe and fail to,
//! and no INI to parse at startup.
//!
//! The themes form a small inheritance hierarchy. A private [`base`] parent
//! holds everything that does not change between themes — all the geometry and
//! layout metrics, the font size, and the tray/app icons — and is not itself
//! selectable. The two shipped themes, [`dark`] and [`light`], are built from
//! `base` and override only their colours. The mechanism is Rust's struct
//! update syntax (`..base()`): every field a theme does not name is taken from
//! the parent verbatim, checked by the compiler, with no field duplicated. So
//! window sizing, button-width fitting, text placement and icon appearance
//! exist in exactly one place and both themes share them by construction.

use crate::color::Color;
use crate::icon::{IconSet, ICONS};
use crate::ui::Dpi;
/// Everything a theme says about colour.
///
/// Split out of `Theme`, and `Copy`, which is what let `layout::Palette` be
/// deleted. That type was a field-by-field copy of seventeen of these, rebuilt
/// on every repaint and on every call to `content_size`, and it existed for one
/// stated reason: `Theme` is not `Copy` because it carries an `IconSet` the
/// painter has no use for. Grouping the fields answers that without a second
/// type — a painter takes the group it needs and never sees the icons.
///
/// A copy of a struct is also a list that has to be kept in step with the
/// original, and the fourth audit found this one had already fallen out of
/// step elsewhere in the same file. One definition cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Colors {
    pub background: Color,
    pub surface: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub accent: Color,
    pub ok: Color,
    pub warning: Color,
    pub critical: Color,
    pub border: Color,
    /// The softer border tone drawn as a full rounded frame *under* the side
    /// segments, so the corners are not bare.
    ///
    /// `fill_bordered` draws the four side segments in `border` with the
    /// corners inset, which leaves the corners empty. This underlay fills them:
    /// a continuous rounded rectangle in a lighter tone beneath the darker
    /// sides, so a button reads as a closed shape with accented flanks rather
    /// than four floating dashes. The dark theme sets it equal to `border`
    /// (its corners simply get the same tone); the light theme makes it a
    /// lighter grey than the accented sides.
    pub border_soft: Color,
    /// Fill of a text field while it holds keyboard focus.
    ///
    /// A stored colour rather than a shift of `surface`, because deriving it
    /// from the hover shift breaks on the light theme: there `surface` is pure
    /// white and the hover direction is *darker*, so a shifted focus fill
    /// slides toward the window ground (`#ECECEC`) and the field dissolves into
    /// the window. The cue has to sit **between the window background and pure
    /// white** on the light theme, which is not a direction the hover shift can
    /// express. The dark theme keeps a fill lighter than its `surface`, as
    /// before. Each theme names the colour it wants outright.
    pub field_focus: Color,
    /// Colour of the keyboard focus ring — the thin frame drawn around the
    /// control the keyboard is currently aimed at.
    ///
    /// The only thing a theme says about the ring. Its width, its distance
    /// from the control and every rule about when it appears live in
    /// `ui::focus` and the painter, identical in every theme: keyboard
    /// navigation is not a matter of styling, and a theme that could change
    /// how Tab behaves would be a theme that could break it.
    ///
    /// A blue in both themes, offset from the accent in whichever direction
    /// that theme calls readable — lighter on the dark ground, darker on the
    /// light one — for the same reason `hover_shift` has a direction. Offset
    /// rather than equal to the accent because the ring is drawn a pixel
    /// outside a control that may itself be filled with the accent: a ticked
    /// checkbox would otherwise be a blue box inside a blue frame.
    ///
    /// One hue, near 215, in both: the dark theme's ring is a pale tint of it
    /// and the light theme's a darker shade, which is the same move
    /// `hover_shift` makes in each direction. Neither goes to the end of its
    /// range — 7.9:1 on the dark ground and 3.5:1 on the light one, against
    /// the 16.7:1 and 17.8:1 of the pure white and pure black they started
    /// from. A marker has to be found, not endured: it appears on every Tab
    /// and stays for the life of the window.
    pub focus_ring: Color,
    /// How far a button's fill colour moves when the pointer hovers over it.
    ///
    /// The *mechanism* is shared: [`Colors::hover_fill`] shifts a button's fill
    /// by this amount so a hovered button reads as live before it is clicked.
    /// The *direction* is the theme's own choice — this is the only field of
    /// the hover feedback a theme sets. Positive lightens, negative darkens:
    /// the dark theme lightens its dark controls toward the light, the light
    /// theme darkens its white controls toward the ground, so the feedback
    /// stays visible on either background. `base` leaves it at zero (no
    /// feedback), a placeholder both themes replace, exactly like the colour
    /// fields.
    ///
    /// Colour, not geometry: it is a delta applied to a `Color` and has no
    /// notion of DPI, which is why it is here and not in [`Metrics`].
    pub hover_shift: i16,
}

impl Colors {
    /// The fill a button takes while the pointer hovers over it.
    ///
    /// The shared hover-feedback function, defined once and inherited by both
    /// selectable themes. It applies the theme's own `hover_shift` to whatever
    /// the button's resting fill is, so the dark theme lightens and the light
    /// theme darkens from the same call site: the painter asks a `Colors` for
    /// `hover_fill(resting)` without knowing which theme is active or which way
    /// it moves.
    pub(crate) fn hover_fill(self, resting: Color) -> Color {
        resting.shifted(self.hover_shift)
    }

    /// The fill of a hovered button whose resting fill is `surface` — the
    /// common case, and the one `Palette` used to precompute.
    pub(crate) fn hover_surface(self) -> Color {
        self.hover_fill(self.surface)
    }
}

/// Everything a theme says about size and spacing, in pixels.
///
/// **Integers, and that is the point.** These were `f32` and every reader
/// truncated for itself: `theme.spacing as i32`, `theme.padding as i32`,
/// `theme.indent as i32 * depth`, four dozen times over. `scaled` multiplied
/// floats and the readers cut them down, so at 150 % the measurer and the
/// painter agreed on `spacing / 2` only because both happened to truncate
/// before dividing. Reorder those two operations at one site and the layout
/// comes apart on a fractional scale, with nothing to show which of the two
/// sites was wrong. Rounding once, here, leaves no such choice to make: past
/// this point there is only one representation of a distance and `as i32`
/// disappears from the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Metrics {
    pub corner_radius: i32,
    pub padding: i32,
    pub spacing: i32,
    /// Gap between the label column and the value column.
    ///
    /// Its own metric rather than a multiple of `spacing`, because it is the
    /// one gap a reader actually perceives as the divide between the two
    /// columns: too small and the value reads as a continuation of the label,
    /// too large and the eye has to travel. The window is measured rather than
    /// fixed, so widening this pushes the values right *and* widens the window,
    /// instead of eating into the value column.
    pub column_gap: i32,
    /// How far one level of nesting steps a row in.
    ///
    /// Used by subordinate checkboxes — the per-event switches under the
    /// notifications master switch — so the hierarchy is visible in the
    /// layout rather than implied by order alone.
    pub indent: i32,
    /// Minimum width of the panel window.
    ///
    /// A floor, not a fixed size. The window measures the strings it is about
    /// to draw and uses whichever is larger, so a language with compact labels
    /// gets a compact window rather than one sized for the longest
    /// translation of any language.
    pub panel_width: i32,
    /// Minimum width of the Settings dialog, separate from the panel's.
    ///
    /// Like `panel_width`, a floor rather than the answer: the dialog measures
    /// its own content and takes whichever is larger. The two are separate
    /// because the windows are driven by different things — Settings by its
    /// longest per-event checkbox caption, the panel by its label column plus
    /// the buzzer row — and one number for both meant the wider one dictated
    /// the other.
    pub settings_width: i32,
    /// Height of one text row. Drives every vertical measurement, so the
    /// whole panel tightens or loosens with it.
    pub row_height: i32,
    /// Smallest a button may be, before its caption is taken into account.
    ///
    /// A button still grows past this to fit its caption; the minimum keeps
    /// short captions from producing buttons too small to aim at.
    pub button_min_width: i32,
}

impl Metrics {
    /// Scales every metric to a monitor's DPI.
    ///
    /// Every value here is authored at 96 DPI, the same baseline Win32 has
    /// always called "100 %". `content_size` and `paint` read them straight as
    /// pixels; nothing in that layer multiplies by a scale factor, because it
    /// was never meant to — scaling is this function's one job, applied once,
    /// at the boundary where a theme is about to drive an actual window.
    ///
    /// `dpi` of 96 is a no-op by construction, so calling this with the
    /// unscaled baseline is always safe and never a special case callers need
    /// to guard against.
    ///
    /// Written as an exhaustive destructure rather than `..*self`. Struct
    /// update syntax would take every unnamed field verbatim, which is the
    /// right default for the themes' inheritance and precisely the wrong one
    /// here: a metric added and not added to the list below would be silently
    /// left at its 96-DPI value, and the symptom — one measurement out of a
    /// dozen not growing with the monitor — is the same class of near-invisible
    /// layout bug this function exists to prevent. Destructured, the compiler
    /// refuses to build until the new field is accounted for.
    ///
    /// Private to this module, and that is load-bearing rather than tidiness.
    /// This returns the same type it takes, so nothing about the value says
    /// whether it has been scaled once, twice or not at all; the guarantee that
    /// it happens exactly once lives in there being exactly one caller.
    /// [`Theme::scaled`] is that caller, and its return type — [`ScaledTheme`],
    /// not `Theme` — is what carries the fact outward.
    fn scaled(self, dpi: Dpi) -> Metrics {
        let Metrics {
            corner_radius,
            padding,
            spacing,
            column_gap,
            indent,
            panel_width,
            settings_width,
            row_height,
            button_min_width,
        } = self;
        Metrics {
            corner_radius: dpi.scale_96(corner_radius),
            padding: dpi.scale_96(padding),
            spacing: dpi.scale_96(spacing),
            column_gap: dpi.scale_96(column_gap),
            indent: dpi.scale_96(indent),
            panel_width: dpi.scale_96(panel_width),
            settings_width: dpi.scale_96(settings_width),
            row_height: dpi.scale_96(row_height),
            button_min_width: dpi.scale_96(button_min_width),
        }
    }

    /// Extra bottom padding under a window's last row, so the gap below it
    /// matches the gap above the title as the eye sees it.
    ///
    /// `padding` is reserved at both ends, but the two are not visually equal.
    /// The title sits vertically centred in a row taller than its own text, so
    /// half a `spacing` of blank appears above the words — the top gap the eye
    /// measures is `padding + spacing / 2`. The last row has no such surround,
    /// so an equal `padding` at the bottom reads as tighter. Charging the
    /// difference makes the two ends match as seen rather than as measured.
    pub(crate) fn bottom_slack(self) -> i32 {
        self.spacing / 2
    }

    /// The window's bottom margin: the blank between the last row and the
    /// bottom edge, as the eye measures it. `padding` plus the slack that
    /// balances it against the visual top gap.
    pub(crate) fn bottom_margin(self) -> i32 {
        self.padding + self.bottom_slack()
    }

    /// The gap reserved *above* the Settings dialog's button row, so the space
    /// over the buttons equals the space under them.
    ///
    /// The buttons are the last thing in the dialog, and the space beneath them
    /// is [`bottom_margin`](Self::bottom_margin). For the gap above to read as
    /// equal, the button row carries this lead-in itself rather than relying on
    /// a spacer tuned for one predecessor. The row before the buttons is a
    /// checkbox in the ordinary case and the wrapped warning when the interval
    /// is invalid; both are made to leave the same `spacing / 2` of trailing
    /// blank below their last line of text, so subtracting that half-line here
    /// lands the button box exactly `bottom_margin` below the text in either
    /// case. Balanced by construction, not by a tuned constant that would need
    /// re-checking whenever the warning is present.
    pub(crate) fn button_row_lead(self) -> i32 {
        self.bottom_margin() - self.spacing / 2
    }
}

/// A complete theme: colour, geometry, the font size and the icons.
///
/// Three groups and a number, rather than twenty-two flat fields. The grouping
/// is what a reader has to know anyway — which of these does DPI touch, which
/// does the painter need, which is neither — made explicit and enforced: DPI
/// scaling is now a method on [`Metrics`] and cannot reach a colour, and the
/// painter takes the group it uses rather than a hand-maintained copy of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Theme {
    pub colors: Colors,
    pub metrics: Metrics,
    /// UI font size in points.
    ///
    /// Neither colour nor scaled geometry, which is why it sits here rather
    /// than in either group. It is in *points*, not pixels, and `font_height`
    /// already converts points to pixels at the window's real DPI
    /// (`points * dpi / 72`); scaling it alongside the metrics would square the
    /// effect, so a 150 % monitor would ask for 150 % of 150 %. It used to live
    /// among the geometry with a paragraph of `scaled` explaining that it was
    /// the one field that must be skipped — an exception that is now a
    /// structural fact instead of a comment.
    ///
    /// Sized together with `row_height` all the same: a theme raising the font
    /// without raising the row would get clipped text, so both live in
    /// [`base`].
    pub font_points: f32,
    /// Tray and application icons.
    ///
    /// A base-theme property, shared by both themes: the tray and the exe live
    /// outside the panel window, so the window background colour has no bearing
    /// on them. The value comes from `icon::ICONS`, the single definition also
    /// read by the build script that draws the exe icon.
    pub icons: IconSet,
}

impl Theme {
    /// Resolves the theme for a configured code.
    ///
    /// The INI is hand-edited, so an unknown code is ordinary input rather than
    /// corruption; it falls back to the default (dark) theme. `found` lets the
    /// caller log the fallback, exactly as the locale loader does.
    pub(crate) fn by_code(code: &str) -> crate::resolved::Resolved<Theme> {
        use crate::resolved::Resolved;
        match Builtin::from_code(code) {
            Some(b) => Resolved::found(b.theme()),
            None => Resolved::fell_back(Builtin::default().theme()),
        }
    }

    /// The default theme: dark. Used when no configuration selects otherwise —
    /// a first run with no INI comes up dark.
    pub(crate) fn default_theme() -> Theme {
        Builtin::default().theme()
    }

    /// The theme as it applies on a monitor at `dpi`: the metrics scaled,
    /// everything else verbatim.
    ///
    /// Colours have no notion of DPI and the icons are rendered at whatever
    /// size the caller asks for, so there is nothing for this to do to them —
    /// and now no way for it to try.
    ///
    /// The one crossing from authored geometry to device pixels, and the only
    /// thing anywhere that produces a [`ScaledTheme`].
    pub(crate) fn scaled(self, dpi: Dpi) -> ScaledTheme {
        ScaledTheme(Theme {
            metrics: self.metrics.scaled(dpi),
            ..self
        })
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::default_theme()
    }
}

/// A theme whose geometry is in the device pixels of one particular monitor.
///
/// A separate name for what used to be the same type as [`Theme`], because the
/// difference between the two is the difference between 21 pixels and 32, and
/// nothing in a value said which it was. `Theme::scaled` returned a `Theme`;
/// `WindowState` held one of each; and which of `state.theme` and
/// `state.base_theme` a given line wanted was decided by reading the variable's
/// name and trusting it. Every reader of either field rested on that, and one
/// written tomorrow would compile whichever it reached for — measuring a window
/// against unscaled geometry produces a window that is simply the wrong size on
/// any monitor above 100 %, with nothing at all to say so.
///
/// One transition, one direction, no way back. [`Theme::scaled`] is the only
/// constructor — the field is private, so no other module can assemble one —
/// and there is deliberately no method returning the `Theme` inside. Rescaling
/// a scaled theme is therefore not something a caller can express, rather than
/// something a comment asks them not to do. What genuinely needs the 96-DPI
/// original keeps the original: `WindowState` stores the `Theme` it was given
/// beside this, and `WM_DPICHANGED` rescales from that.
///
/// Two groups are readable, not four. `font_points` and `icons` are carried
/// across — the crossing copies the whole theme — but nothing on this side ever
/// asks a *scaled* theme for either: the font size reaches the painter through
/// `Script`, and the marks belong to the tray and the title bar, which work
/// from the 96-DPI theme they were given. Readers were written for both and
/// deleted when no call site turned up, which is the rule here for anything
/// else that is never read.
///
/// The whole `Theme` is held rather than the two groups that are read, and that
/// is what keeps the crossing complete: a group added to `Theme` tomorrow
/// arrives here without anybody remembering to bring it, where a two-field
/// struct built from a literal would silently leave it behind.
///
/// Both groups are read through methods rather than fields for the same reason
/// the field here is private: a `pub` field is an assignment target, and
/// assigning a fresh `Metrics` into a scaled theme would put unscaled geometry
/// back inside the type whose whole meaning is that its geometry is scaled.
/// Each returns by value; both groups are `Copy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScaledTheme(Theme);

impl ScaledTheme {
    /// The theme's colours, which scaling does not touch.
    pub(crate) fn colors(self) -> Colors {
        self.0.colors
    }

    /// The geometry, in this monitor's device pixels.
    pub(crate) fn metrics(self) -> Metrics {
        self.0.metrics
    }
}

/// The selectable themes, in display order. Dark is first and is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Builtin {
    #[default]
    Dark,
    Light,
}

impl Builtin {
    /// Every shipped theme, in the order the settings list shows them. Dark
    /// first, because it is the default.
    pub(crate) const ALL: [Builtin; 2] = [Builtin::Dark, Builtin::Light];

    /// The stable identifier written to and read from the INI.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Builtin::Dark => "dark",
            Builtin::Light => "light",
        }
    }

    /// The [`crate::strings::Key`] for the localised display name shown in
    /// the settings dropdown.
    pub(crate) fn name_key(self) -> crate::strings::Key {
        use crate::strings::Key;
        match self {
            Builtin::Dark => Key::ThemeDark,
            Builtin::Light => Key::ThemeLight,
        }
    }

    pub(crate) fn from_code(code: &str) -> Option<Builtin> {
        Builtin::ALL.into_iter().find(|b| b.code() == code)
    }

    pub(crate) fn theme(self) -> Theme {
        match self {
            Builtin::Dark => dark(),
            Builtin::Light => light(),
        }
    }
}
/// The two blends the interface performs on a theme colour.
///
/// Here rather than in `color.rs` for the same reason as the `COLORREF`
/// conversion: a hover shift and an alpha resolution are what a *theme*
/// does with a colour, and the build script that shares `color.rs` has
/// neither a pointer to hover with nor a surface to resolve against.
impl Color {
    /// Shifts every channel by `delta`, clamped to `[0, 255]`, keeping alpha.
    ///
    /// A positive delta lightens, a negative one darkens; the shift is the
    /// same absolute step on each channel, so a neutral grey stays neutral and
    /// a tinted colour keeps its hue while moving toward white or black. Used
    /// for pointer-hover feedback on buttons: the base theme applies the shift,
    /// each theme names its direction (see `Theme::hover_shift`).
    #[must_use]
    pub(crate) const fn shifted(self, delta: i16) -> Color {
        Color {
            r: shift_channel(self.r, delta),
            g: shift_channel(self.g, delta),
            b: shift_channel(self.b, delta),
            a: self.a,
        }
    }

    /// Blends `self` over an opaque background, honouring alpha. GDI has no
    /// alpha blending for text and lines, so translucent theme colours are
    /// resolved against the surface they sit on before being handed to it.
    #[must_use]
    pub(crate) fn over(self, bg: Color) -> Color {
        if self.a == 255 {
            return Color::from_rgb(self.r, self.g, self.b);
        }
        let a = u32::from(self.a);
        let inv = 255 - a;
        let mix = |f: u8, b: u8| (((u32::from(f) * a) + (u32::from(b) * inv)) / 255) as u8;
        Color::from_rgb(mix(self.r, bg.r), mix(self.g, bg.g), mix(self.b, bg.b))
    }
}

/// One channel shifted by `delta` and clamped to `[0, 255]`.
const fn shift_channel(c: u8, delta: i16) -> u8 {
    let v = c as i16 + delta;
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// The base parent: all geometry, layout metrics, the font size and the icons.
///
/// Not selectable and not exposed. Its colours are placeholders, present only
/// so the struct has a complete value for `..base()` to update from — and no
/// theme takes any of them. That used to be a claim; it is now checked. While
/// the colours were flat fields of `Theme`, a theme that forgot one silently
/// inherited the dark placeholder and shipped a dark tone in the light palette,
/// and the only thing standing against it was that nobody had forgotten yet.
/// Grouped, a theme names its `Colors` outright, so a missing colour is a
/// missing struct field and the build stops.
fn base() -> Theme {
    Theme {
        colors: Colors {
            // Placeholder colours; both themes replace every one of these.
            background: Color::from_rgb(0x1E, 0x1E, 0x1E),
            surface: Color::from_rgb(0x25, 0x25, 0x26),
            text_primary: Color::from_rgb(0xE0, 0xE0, 0xE0),
            text_secondary: Color::from_rgb(0x9E, 0x9E, 0x9E),
            accent: Color::from_rgb(0x0A, 0x84, 0xFF),
            ok: Color::from_rgb(0x34, 0xC7, 0x59),
            warning: Color::from_rgb(0xFF, 0x9F, 0x0A),
            critical: Color::from_rgb(0xFF, 0x3B, 0x30),
            border: Color::from_rgb(0x3A, 0x3A, 0x3A),
            border_soft: Color::from_rgb(0x3A, 0x3A, 0x3A),
            // Placeholder; both themes name their own focus fill.
            field_focus: Color::from_rgb(0x31, 0x31, 0x32),
            // Placeholder; both themes name their own ring colour.
            focus_ring: Color::from_rgb(0x8A, 0xB4, 0xF8),
            // Placeholder: no feedback. Both themes name their own direction.
            hover_shift: 0,
        },
        // Geometry and layout: the real content of the base theme, inherited
        // unchanged by every selectable theme. Authored at 96 DPI.
        metrics: Metrics {
            corner_radius: 6,
            padding: 12,
            spacing: 8,
            column_gap: 20,
            indent: 18,
            panel_width: 320,
            settings_width: 340,
            row_height: 21,
            button_min_width: 96,
        },
        font_points: crate::ui::DEFAULT_FONT_POINTS,
        icons: ICONS,
    }
}

/// The dark theme, and the default. Its colours are the ones the utility has
/// always shipped.
fn dark() -> Theme {
    Theme {
        colors: Colors {
            background: Color::from_rgb(0x1E, 0x1E, 0x1E),
            surface: Color::from_rgb(0x25, 0x25, 0x26),
            text_primary: Color::from_rgb(0xE0, 0xE0, 0xE0),
            text_secondary: Color::from_rgb(0x9E, 0x9E, 0x9E),
            accent: Color::from_rgb(0x0A, 0x84, 0xFF),
            ok: Color::from_rgb(0x34, 0xC7, 0x59),
            warning: Color::from_rgb(0xFF, 0x9F, 0x0A),
            critical: Color::from_rgb(0xFF, 0x3B, 0x30),
            // Accented side segments: lighter than the ground so the flanks catch
            // the eye. On a dark background "accented" means lighter, the mirror of
            // the light theme where it means darker.
            border: Color::from_rgb(0x5A, 0x5A, 0x5A),
            // The corner underlay recedes: the subtler mid-tone the theme used to
            // carry as its only border. So the same gradation as the light theme —
            // prominent flanks, quiet corners — reads on the dark ground too.
            border_soft: Color::from_rgb(0x3A, 0x3A, 0x3A),
            // A focused field lightens from #252526 to #313132 — the same tone the
            // field carried before, a step above the surface but well short of a
            // hovered button, so it reads as "waiting for input" on the dark
            // ground without shouting.
            field_focus: Color::from_rgb(0x31, 0x31, 0x32),
            // A pale, half-desaturated blue: 7.9:1 against the window ground,
            // where white was 16.7:1. White was legible and unpleasant — the ring
            // sits on screen for as long as the window is open, and at that
            // duration the brightest tone in the theme is glare rather than
            // information. This is lighter than the accent it may be drawn around
            // (#0A84FF, the fill of a ticked checkbox) and much less saturated, so
            // the frame and the box it frames stay two separate things.
            focus_ring: Color::from_rgb(0x8A, 0xB4, 0xF8),
            // A hovered control lightens: on the dark ground a control moving
            // toward the light is what reads as "the pointer is here".
            hover_shift: 0x18,
        },
        ..base()
    }
}

/// The light theme. Only the colours differ from `base`; every metric, the
/// font size and the icons are inherited unchanged.
///
/// The palette is not the dark theme inverted — inversion yields muddy,
/// low-contrast results. It is picked for legibility on a light background:
/// a near-white (not pure white) ground to take the glare off, near-black
/// primary text for maximum contrast, and status hues darkened so green,
/// amber and red stay readable on white, where their bright forms wash out.
fn light() -> Theme {
    Theme {
        colors: Colors {
            // Slightly darker than a plain near-white so the white controls on it
            // (buttons, dropdowns, the value boxes) read as raised rather than
            // dissolving into the ground.
            background: Color::from_rgb(0xEC, 0xEC, 0xEC),
            // Controls sit at pure white, a clear step above the ground.
            surface: Color::from_rgb(0xFF, 0xFF, 0xFF),
            // Pure black for all text in the light theme: labels and values alike.
            // Grey secondary text looked washed out on the light ground, so both
            // primary and secondary text are #000000; only the status hues below
            // (ok, warning, critical) and the accent carry colour.
            text_primary: Color::from_rgb(0x00, 0x00, 0x00),
            text_secondary: Color::from_rgb(0x00, 0x00, 0x00),
            // A darker blue: the dark theme's bright #0A84FF vibrates on white.
            accent: Color::from_rgb(0x00, 0x66, 0xCC),
            // Status hues darkened for a light ground; the bright tray colours
            // wash out on white.
            ok: Color::from_rgb(0x1E, 0x8E, 0x3E),
            warning: Color::from_rgb(0xB3, 0x6B, 0x00),
            critical: Color::from_rgb(0xC5, 0x22, 0x1F),
            // The accented side segments: a deliberately dark grey so buttons and
            // dropdowns have a crisp, visible flank instead of a faint hairline.
            border: Color::from_rgb(0x70, 0x76, 0x7D),
            // The corner underlay: lighter than the sides, so the rounded corners
            // are closed but do not compete with the accented flanks.
            border_soft: Color::from_rgb(0xB0, 0xB4, 0xB8),
            // A focused field on the light theme keeps pure white: its surface is
            // already #FFFFFF, a clear step above the #ECECEC window ground, so the
            // field reads as raised without any shift. Moving it darker would only
            // slide it toward the ground and make it dissolve into the window; the
            // focus cue here is the caret, not a fill change.
            field_focus: Color::from_rgb(0xFF, 0xFF, 0xFF),
            // The same blue as the dark theme's ring, taken down rather than made
            // into a different colour: hue 212 against its 217, and darker and
            // more saturated because on a light ground the readable direction is
            // downward. 3.5:1 against the window ground and 4.1:1 against the
            // white controls.
            //
            // Lighter than it first was. A deep #0B4FA8 measured well — 6.6:1 —
            // and read as black: past a point, taking a hue darker stops making it
            // more visible and only drains the hue out of it, which for a marker
            // whose whole job is to be recognised at a glance is the wrong trade.
            // This clears the 3:1 a graphical marker needs with enough margin for
            // a one-pixel line, and still looks blue.
            focus_ring: Color::from_rgb(0x40, 0x80, 0xC8),
            // A hovered control darkens: the white controls sit above a near-white
            // ground, so moving them toward the ground — not away from it — is what
            // the eye reads as feedback. The mirror of the dark theme's lightening.
            // The step is a good deal past the background tone (#ECECEC), not just
            // to it: a lighter shift landed the hovered fill right on the ground
            // colour and the button dissolved into it, so the feedback has to clear
            // the ground to read at all.
            hover_shift: -0x2C,
        },
        ..base()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_is_the_default() {
        assert_eq!(Builtin::default(), Builtin::Dark);
        // A first run with no INI resolves to dark.
        assert_eq!(
            Theme::default_theme().colors.background,
            Color::from_rgb(0x1E, 0x1E, 0x1E)
        );
    }

    /// The dark theme must stay bit-for-bit the colours the utility has always
    /// shipped. It is the theme in use, and this pins it against accidental
    /// drift while the light theme is developed alongside. The border tones
    /// are the exception: they gained a flank/corner gradation on purpose (a
    /// prominent side segment over a quieter corner underlay), so they are
    /// pinned to their new values rather than the old flat one.
    #[test]
    fn dark_colours_are_unchanged() {
        let t = dark();
        assert_eq!(t.colors.background, Color::from_rgb(0x1E, 0x1E, 0x1E));
        assert_eq!(t.colors.surface, Color::from_rgb(0x25, 0x25, 0x26));
        assert_eq!(t.colors.text_primary, Color::from_rgb(0xE0, 0xE0, 0xE0));
        assert_eq!(t.colors.text_secondary, Color::from_rgb(0x9E, 0x9E, 0x9E));
        assert_eq!(t.colors.accent, Color::from_rgb(0x0A, 0x84, 0xFF));
        assert_eq!(t.colors.ok, Color::from_rgb(0x34, 0xC7, 0x59));
        assert_eq!(t.colors.warning, Color::from_rgb(0xFF, 0x9F, 0x0A));
        assert_eq!(t.colors.critical, Color::from_rgb(0xFF, 0x3B, 0x30));
        // Accented side segments, quieter corner underlay.
        assert_eq!(t.colors.border, Color::from_rgb(0x5A, 0x5A, 0x5A));
        assert_eq!(t.colors.border_soft, Color::from_rgb(0x3A, 0x3A, 0x3A));
        assert_eq!(t.colors.field_focus, Color::from_rgb(0x31, 0x31, 0x32));
    }

    /// Geometry, font size and icons are the base theme's, so both selectable
    /// themes must carry identical values for them. This is the point of the
    /// `..base()` inheritance: layout logic is not duplicated per theme.
    #[test]
    fn geometry_and_icons_are_inherited_unchanged() {
        let d = dark();
        let l = light();
        assert_eq!(d.metrics.corner_radius, l.metrics.corner_radius);
        assert_eq!(d.metrics.padding, l.metrics.padding);
        assert_eq!(d.metrics.spacing, l.metrics.spacing);
        assert_eq!(d.font_points, l.font_points);
        assert_eq!(d.metrics.column_gap, l.metrics.column_gap);
        assert_eq!(d.metrics.indent, l.metrics.indent);
        assert_eq!(d.metrics.panel_width, l.metrics.panel_width);
        assert_eq!(d.metrics.settings_width, l.metrics.settings_width);
        assert_eq!(d.metrics.row_height, l.metrics.row_height);
        assert_eq!(d.metrics.button_min_width, l.metrics.button_min_width);
        // Icons are shared verbatim between the two themes.
        assert_eq!(d.icons, l.icons);
        assert_eq!(d.icons, ICONS);
        // The base metrics themselves.
        assert_eq!(d.metrics.corner_radius, 6);
        assert_eq!(d.metrics.panel_width, 320);
        assert_eq!(d.metrics.settings_width, 340);
        assert_eq!(d.metrics.row_height, 21);
        assert_eq!(d.metrics.button_min_width, 96);
    }

    /// The light theme differs from the dark one in colour, not geometry: if
    /// its palette ever equalled the dark theme's, the theme would be a no-op
    /// choice.
    #[test]
    fn light_differs_from_dark_only_in_colour() {
        let d = dark();
        let l = light();
        assert_ne!(d.colors.background, l.colors.background);
        assert_ne!(d.colors.text_primary, l.colors.text_primary);
        assert_ne!(d.colors.accent, l.colors.accent);
        // The soft corner underlay differs from the accented sides in *both*
        // themes: prominent flanks, quiet corners, on each ground.
        assert_ne!(d.colors.border_soft, d.colors.border);
        assert_ne!(l.colors.border_soft, l.colors.border);
    }

    /// Hover feedback is a shared mechanism with a per-theme direction. Both
    /// themes must move a hovered button, and in opposite directions: the dark
    /// theme lightens its dark control, the light theme darkens its white one,
    /// so the feedback is visible on either ground.
    #[test]
    fn hover_shift_moves_the_fill_in_each_theme_s_direction() {
        let d = dark();
        let l = light();
        // The dark theme lightens: shift is positive.
        assert!(d.colors.hover_shift > 0);
        // The light theme darkens: shift is negative.
        assert!(l.colors.hover_shift < 0);
        // The base leaves it inert; both themes replace it.
        assert_eq!(base().colors.hover_shift, 0);

        // The shared method moves a resting fill by the theme's own direction,
        // from one call site that knows neither the theme nor the direction.
        let dark_ctrl = d.colors.surface;
        assert_ne!(d.colors.hover_fill(dark_ctrl), dark_ctrl);
        // Dark lightens: every channel rises (or holds at the ceiling).
        assert!(d.colors.hover_fill(dark_ctrl).r >= dark_ctrl.r);
        assert!(d.colors.hover_fill(dark_ctrl).g >= dark_ctrl.g);

        let light_ctrl = l.colors.surface;
        assert_ne!(l.colors.hover_fill(light_ctrl), light_ctrl);
        // Light darkens: every channel falls (or holds at the floor).
        assert!(l.colors.hover_fill(light_ctrl).r <= light_ctrl.r);
        assert!(l.colors.hover_fill(light_ctrl).g <= light_ctrl.g);
    }

    /// The focus fill is a stored per-theme colour. The dark theme lightens
    /// its surface so a focused field reads as filled on the dark ground; the
    /// light theme keeps pure white — its surface already stands clear of the
    /// #ECECEC window ground, and darkening it would only sink it into the
    /// window, so on the light theme the caret alone marks focus.
    #[test]
    fn field_focus_reads_on_both_themes() {
        let d = dark();
        let l = light();

        // Dark lightens from its surface, a visible step above it.
        assert_ne!(d.colors.field_focus, d.colors.surface);
        assert!(d.colors.field_focus.r >= d.colors.surface.r);
        assert!(d.colors.field_focus.g >= d.colors.surface.g);
        assert!(d.colors.field_focus.b >= d.colors.surface.b);

        // Light keeps its surface white: no fill change on focus.
        assert_eq!(l.colors.field_focus, l.colors.surface);
        assert_eq!(l.colors.field_focus, Color::from_rgb(0xFF, 0xFF, 0xFF));
    }

    /// The hovered fill must clear the window ground, not land on it. The
    /// light theme's earlier shift darkened a white button only as far as the
    /// background tone, so the hovered button dissolved into the ground instead
    /// of standing out. The feedback has to sit a visible margin past the
    /// background on the far side from the resting fill.
    #[test]
    fn hover_fill_stands_clear_of_the_background() {
        // A comfortable margin: below this the two tones are too close to tell
        // apart at a glance.
        const MARGIN: i16 = 16;

        let l = light();
        // White button, near-white ground: the hovered fill must be clearly
        // *below* the ground, not merely below the button.
        let hovered = l.colors.hover_fill(l.colors.surface);
        assert!(
            (i16::from(l.colors.background.r) - i16::from(hovered.r)) >= MARGIN,
            "light hover fill {:#04x} is not clearly darker than the ground {:#04x}",
            hovered.r,
            l.colors.background.r
        );

        let d = dark();
        // Dark button, dark ground: the hovered fill must be clearly *above*
        // the ground.
        let hovered = d.colors.hover_fill(d.colors.surface);
        assert!(
            (i16::from(hovered.r) - i16::from(d.colors.background.r)) >= MARGIN,
            "dark hover fill {:#04x} is not clearly lighter than the ground {:#04x}",
            hovered.r,
            d.colors.background.r
        );
    }

    #[test]
    fn codes_round_trip() {
        for b in Builtin::ALL {
            assert_eq!(Builtin::from_code(b.code()), Some(b));
        }
        assert_eq!(Builtin::from_code("dark"), Some(Builtin::Dark));
        assert_eq!(Builtin::from_code("light"), Some(Builtin::Light));
        // The old code is gone; it resolves to nothing.
        assert_eq!(Builtin::from_code("default"), None);
    }

    /// An unknown code falls back to the default theme and reports it, the
    /// same contract the locale loader has.
    #[test]
    fn unknown_code_falls_back_to_default() {
        let unknown = Theme::by_code("nonsense");
        assert!(!unknown.recognised);
        assert_eq!(unknown.value.colors.background, dark().colors.background);

        let known = Theme::by_code("light");
        assert!(known.recognised);
        assert_eq!(known.value.colors.background, light().colors.background);
    }

    /// The tray mark is one drawing across every state, and each spec is
    /// two-tone: fill differs from stroke so the outline reads against the
    /// dimmed interior.
    #[test]
    fn icons_are_the_shipped_set() {
        let t = Theme::default_theme();
        for spec in [
            t.icons.normal,
            t.icons.on_battery,
            t.icons.critical,
            t.icons.disconnected,
        ] {
            assert_ne!(spec.fill, spec.stroke);
        }
        // The application mark is the brand blue.
        assert_eq!(t.icons.app.fill, Color::from_rgb(0x0A, 0x84, 0xFF));
    }

    /// 96 DPI is the baseline every geometric field is authored at, so
    /// scaling to it must be a no-op — callers that always route through
    /// `scaled()` (including on an unscaled desktop) should never see a
    /// value shift underneath them.
    #[test]
    fn scaling_to_96_dpi_is_a_no_op() {
        let t = dark();
        let s = t.scaled(Dpi::new(96));
        assert_eq!(s.metrics().row_height, t.metrics.row_height);
        assert_eq!(s.metrics().padding, t.metrics.padding);
        assert_eq!(s.metrics().spacing, t.metrics.spacing);
        assert_eq!(s.metrics().corner_radius, t.metrics.corner_radius);
        assert_eq!(s.metrics().column_gap, t.metrics.column_gap);
        assert_eq!(s.metrics().indent, t.metrics.indent);
        assert_eq!(s.metrics().panel_width, t.metrics.panel_width);
        assert_eq!(s.metrics().settings_width, t.metrics.settings_width);
        assert_eq!(s.metrics().button_min_width, t.metrics.button_min_width);
    }

    /// At 150% (144 DPI, the display in the report this guards against)
    /// every geometric field grows by exactly that factor, so a window laid
    /// out from the scaled theme is sized in real device pixels rather than
    /// being drawn small and then bitmap-stretched by the compositor.
    #[test]
    fn scaling_multiplies_every_geometric_field() {
        let t = dark();
        let s = t.scaled(Dpi::new(144));
        // The expected value is computed by the same rule the metrics are:
        // multiply by the ratio and round to the nearest whole pixel. Half a
        // pixel is not a thing a window can be laid out in, and 21 px at 150 %
        // is exactly 31.5 — which used to be truncated to 31 by each reader in
        // turn, so every row was a pixel tighter than it was authored to be.
        let at_144 = |v: i32| (v * 144 + 48) / 96;
        assert_eq!(s.metrics().row_height, at_144(t.metrics.row_height));
        assert_eq!(s.metrics().padding, at_144(t.metrics.padding));
        assert_eq!(s.metrics().spacing, at_144(t.metrics.spacing));
        assert_eq!(s.metrics().corner_radius, at_144(t.metrics.corner_radius));
        assert_eq!(s.metrics().column_gap, at_144(t.metrics.column_gap));
        assert_eq!(s.metrics().indent, at_144(t.metrics.indent));
        assert_eq!(s.metrics().panel_width, at_144(t.metrics.panel_width));
        assert_eq!(s.metrics().settings_width, at_144(t.metrics.settings_width));
        assert_eq!(
            s.metrics().button_min_width,
            at_144(t.metrics.button_min_width)
        );
        // And every one of them actually grew — a rule that returned its input
        // would satisfy the equalities above.
        assert!(s.metrics().row_height > t.metrics.row_height);
        assert_eq!(
            s.metrics().row_height,
            32,
            "21 px at 150 % is 31.5, rounded up"
        );
    }

    /// Colours, `font_points` and icons are untouched by scaling: colours have
    /// no notion of DPI, and `font_points` stays in points because
    /// `font_height` is the one place that turns points into device pixels.
    /// Scaling it here as well would compound with that conversion instead
    /// of composing with it.
    ///
    /// The last two are read through the wrapped `Theme` rather than through a
    /// method, because `ScaledTheme` deliberately offers neither: no caller
    /// asks a scaled theme for a point size or a mark. They are still carried
    /// across, and the point of checking is that crossing must not quietly
    /// alter something on its way through. This module is the one place the
    /// representation is visible, and so the one place the check can be made.
    #[test]
    fn scaling_leaves_colours_font_points_and_icons_alone() {
        let t = dark();
        let s = t.scaled(Dpi::new(144));
        assert_eq!(s.colors().background, t.colors.background);
        assert_eq!(s.colors().text_primary, t.colors.text_primary);
        assert_eq!(s.colors().accent, t.colors.accent);
        assert_eq!(s.colors().border, t.colors.border);
        assert_eq!(s.colors().border_soft, t.colors.border_soft);
        assert_eq!(s.colors().field_focus, t.colors.field_focus);
        assert_eq!(s.colors().hover_shift, t.colors.hover_shift);
        assert_eq!(s.0.font_points, t.font_points);
        assert_eq!(s.0.icons, t.icons);
    }
}
