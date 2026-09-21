//! What a window is made of, said in data.
//!
//! A [`Row`] and its variants, the gaps between them, the item inside a
//! dropdown, the edits a control reports back, and the hotspot id scheme that
//! lets a click name what it hit. Nothing here draws, measures or touches
//! Win32 — [`Row::height`] takes the measurements it needs as an argument
//! rather than going and getting them.
//!
//! Separate from [`window`](super::window) for exactly that reason. This is
//! the vocabulary every other file in the module speaks, and the vocabulary
//! `ui::panel`, `ui::settings` and `ui::focus` build their windows out of; it
//! was previously buried between a window procedure and a painter, so reading
//! "what can a row be" meant scrolling past six thousand lines that answer
//! "how is one drawn".

use super::scrollbar::DROPDOWN_PAGE;
use super::text::TextMetrics;
use super::window::FOCUS_RING_OUTSET;
use crate::color::Color;
use crate::ui::theme::ScaledTheme;
pub(crate) const HOT_BEEPER: HotspotId = HotspotId::new(1);
/// Opens the settings dialog from the panel itself.
pub(crate) const HOT_SETTINGS: HotspotId = HotspotId::new(2);
/// Starts a self-test from the panel, after a confirmation dialog.
pub(crate) const HOT_SELFTEST: HotspotId = HotspotId::new(3);
/// The Yes button on the self-test confirmation modal.
pub(crate) const HOT_CONFIRM_YES: HotspotId = HotspotId::new(4);
/// The No button on the self-test confirmation modal.
pub(crate) const HOT_CONFIRM_NO: HotspotId = HotspotId::new(5);

/// A single line of the panel. Rendered as label on the left, value on the
/// right, with the label column sized to the widest label of the current
/// locale rather than to a hardcoded width, so no translation is clipped.
#[derive(PartialEq)]
pub(crate) enum Row {
    Header(String),
    Pair {
        label: String,
        value: String,
        color: Color,
    },
    /// Free-flowing text that wraps across as many lines as it needs rather
    /// than being clipped. Used only by the panel's status messages — the
    /// device-not-found explanation and the multiple-HID warning — which are
    /// full sentences and genuinely run to two or three lines. Its height is
    /// measured, not derived: the rows are built before the window has a font,
    /// so the height is resolved later, by whoever holds one — see
    /// [`TextMetrics`].
    Wrapped {
        text: String,
        color: Color,
    },
    /// A single line of full-width coloured text, never wrapped. Used by the
    /// Settings interval warning, which is contracted to fit one line on every
    /// language (a translation that would not fit is shortened, not wrapped).
    /// It is a distinct row from `Wrapped` on purpose: routing a one-line
    /// message through the wrapping estimate made its reserved height depend
    /// on window width — two lines at a narrow width, one at a wide one — and
    /// that phantom second line opened as blank space between the warning and
    /// the OK/Cancel row below it. A row that is always exactly one line has
    /// no such ambiguity, and the buttons sit a fixed distance under it.
    Notice {
        text: String,
        color: Color,
    },
    /// A label, its value, and a button immediately after the value.
    ///
    /// Used for the buzzer. The button sits beside the value rather than at
    /// the right margin: pinned right it was far from what it controls and
    /// next to nothing, so "Enable" read as a switch for the utility. The row
    /// is the same height as a plain `Pair` — the button overhangs the line
    /// symmetrically instead of growing it.
    LabeledButton {
        label: String,
        value: String,
        color: Color,
        button: String,
        id: HotspotId,
        /// False when the button cannot be clicked right now — the self-test
        /// button while a test is running or the pre-test checks fail. Drawn
        /// greyed and registering no hotspot: a control the user cannot tell
        /// apart from a live one, that does nothing on click, is worse than
        /// one visibly inert.
        enabled: bool,
    },
    /// A title with a button right-aligned on the same line, used for the
    /// panel's top row so Settings sits in the corner rather than at the
    /// bottom of a long scroll.
    TitleButton {
        title: String,
        button: String,
        id: HotspotId,
    },
    /// A labelled on/off box. Clicking anywhere on the row toggles it, which
    /// is a much larger target than the box alone.
    Checkbox {
        label: String,
        checked: bool,
        /// Nesting depth. A subordinate switch sits one level in from the
        /// master switch it belongs to, and the whole row moves — box and
        /// label together.
        ///
        /// Structural rather than spaces prepended to `label`, which is how
        /// this was done before and why the indent quietly disappeared: the
        /// spaces shifted only the text, leaving the boxes in a column and
        /// the labels ragged, and any change to how labels were measured or
        /// trimmed took the indent with it. A depth the layout can see cannot
        /// be lost that way, and the hit rectangle follows it too.
        depth: u8,
        id: HotspotId,
    },
    /// A labelled value chosen from a list that opens over the rows below it.
    ///
    /// Not a `COMBOBOX`: that control brings its own window procedure, its own
    /// popup and its own system-themed painting, which in a `#1E1E1E` panel
    /// draws as a white rectangle. Owner-drawing it would mean painting this
    /// anyway, inside someone else's message protocol.
    ///
    /// Not a second window either. A popup window carries visibility state
    /// that has to be kept in step with the application's, and that is the
    /// exact class of bug this utility spent seven iterations on. What the
    /// mature Rust GUI libraries do instead — egui's `ComboBox`, iced's
    /// `PickList` — is draw the list into an overlay layer above the normal
    /// content and hit-test it first. That is what this is: `open` is one
    /// bool, the list is painted last, and its hotspots are consulted before
    /// everything beneath it.
    Dropdown {
        label: String,
        /// Every option, in display order.
        options: Vec<DropdownItem>,
        /// Index of the current selection into `options`.
        selected: usize,
        /// Which option the keyboard has moved to, which is the selection
        /// until the user presses a key. Separate so arrowing through the
        /// list does not commit until Enter.
        highlighted: usize,
        open: bool,
        /// First visible option, advanced by the wheel and by arrowing past
        /// the bottom edge.
        scroll: usize,
        id: HotspotId,
    },
    /// A labelled editable text field. Exactly one field can hold focus, and
    /// it receives typed characters from the window procedure.
    ///
    /// `caret` and the selection range are character offsets into `value`,
    /// carried here so the painter can place the caret and highlight the
    /// selection. They are only meaningful while the field holds focus, which
    /// the row does not state: focus belongs to the window, which knows it for
    /// every kind of control alike, and the painter reads it from there. A
    /// `focused` flag here would be a second copy of that one fact, carried by
    /// one control out of five and kept in step by hand across the owner's
    /// draft and the main loop — which is exactly what it was. The
    /// authoritative caret and selection still live in the owner's `TextField`;
    /// these are the snapshot the rows are rebuilt from, like every other piece
    /// of draft state.
    Field {
        label: String,
        value: String,
        /// Caret position, in characters.
        caret: usize,
        /// Selected character range `start..end`, empty when `start == end`.
        sel_start: usize,
        sel_end: usize,
        id: HotspotId,
    },
    /// Two buttons on one line, so OK and Cancel sit side by side.
    ///
    /// Named `Buttons`, not `ButtonRow`: the enclosing `Row` already says these
    /// are a row, exactly as `Field`, `Notice` and `Dropdown` do not repeat it.
    /// The variant names what the row *contains*.
    Buttons {
        left: String,
        left_id: HotspotId,
        right: String,
        right_id: HotspotId,
    },
    Space(Gap),
}

/// A vertical gap, named by role instead of by pixels.
///
/// Rows are built at 96 dpi and measured against a theme scaled to the
/// window's dpi, so a literal in a row is a literal that never scales: at
/// 200 % every gap in the layout stayed the size it had at 100 % while every
/// row around it doubled, and the vertical rhythm collapsed. The gap is
/// resolved in [`Row::height`] against the theme it is measured with, which is
/// the scaled one — the same place, and the same theme, every other dimension
/// comes from.
///
/// Two steps, and only the two the layout actually uses. The literals these
/// replace were 4, 6 and 8, which is a ladder nobody designed: 4 and 8 are
/// `spacing / 2` and `spacing`, and 6 was a value between them chosen by eye.
/// Naming the roles forces the choice to be made once — a gap either separates
/// two controls or it separates two blocks — instead of being re-guessed at
/// each call site. Each of the three 6s was resolved on that question: the
/// warning above the readings separates blocks, the two in the checkbox column
/// separate controls.
#[derive(PartialEq, Clone, Copy, Hash)]
pub(crate) enum Gap {
    /// Between controls that belong together: a header and the first field
    /// under it, two checkboxes in a section, the lines of one message.
    Half,
    /// Between blocks that do not: a warning and the readings below it, the
    /// title and a section that follows.
    Single,
}

impl Gap {
    /// The gap in pixels, against the theme it is being measured with.
    pub(crate) fn px(self, theme: &ScaledTheme) -> i32 {
        let spacing = theme.metrics().spacing;
        match self {
            Gap::Half => spacing / 2,
            Gap::Single => spacing,
        }
    }
}

/// One option in a [`Row::Dropdown`].
#[derive(PartialEq, Clone)]
pub(crate) struct DropdownItem {
    /// Latin-script part, always drawable in the default UI font.
    pub label: String,
    /// Native-script part shown in parentheses after `label`, empty when the
    /// two would be the same word. Drawn in `font` rather than the UI font.
    pub native: String,
    /// Font family for `native`, `None` for the default UI font.
    pub font: Option<&'static str>,
}

/// What a click can name.
///
/// A bare `u32` until now, in thirty-one constants across three files and in
/// every signature that carries one — `activate(state, id: u32)` sitting beside
/// `on_char(state, ch: u32)`, `render_rgba(theme, size: u32)`, and a
/// `hovered: Option<usize>` (a dropdown option's *index*) one field away from a
/// `hovered_button: Option<u32>` (an id). Nothing in any of those types said
/// which number meant what, so the reader told them apart by the parameter's
/// name and the writer by remembering.
///
/// # The id scheme
///
/// Three bands, and the boundaries between them are what make a click
/// unambiguous:
///
/// - **Controls**, from 1 upwards. Panel controls take the low numbers
///   (`row.rs`), the settings dialog takes 100 and up (`settings.rs`), and the
///   scrollbar parts take `9_990` and up (`scrollbar.rs`) — numbered above the
///   panel's own so the two views can never collide. Each module declares the
///   ids of the controls it owns, beside them, rather than all thirty-one
///   being listed somewhere none of their owners are.
/// - **[`HotspotId::NOTHING`]**, at `9_999`: a click that landed on no control.
/// - **Options**, from [`OPTION_BASE`] upwards, one [`OPTION_BAND`]-wide band
///   per control, allocated by [`HotspotId::option`].
///
/// # Where ids come from
///
/// [`HotspotId::new`] is `const` and exists for the constants above and for the
/// two encoding methods; those are the only places in the tree that mint one.
/// It cannot be narrower than that — the ids are authored numbers, and a type
/// that refused to be built from a number could not hold them. What the type
/// does guarantee is the part that was actually going wrong: an id and any
/// other quantity are no longer the same type, so one cannot arrive where the
/// other is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HotspotId(u32);

impl HotspotId {
    /// Reported when a click lands on no control at all.
    ///
    /// Distinct from "no click" so an owner can dismiss transient UI — an open
    /// dropdown, a focused field — without the window procedure knowing what
    /// those are. Chosen above every real control id and below the option band
    /// so it can never be mistaken for either.
    pub(crate) const NOTHING: HotspotId = HotspotId(9_999);

    /// An id from the number it is authored as.
    pub(crate) const fn new(raw: u32) -> HotspotId {
        HotspotId(raw)
    }

    /// The id of the nth option in the dropdown this id belongs to.
    ///
    /// Options need ids of their own so a click can name which one was hit,
    /// and they must not collide with any control's. Every option id sits at
    /// or above [`OPTION_BASE`], and each control owns an [`OPTION_BAND`]-wide
    /// band above it: option `n` of the control with id 107 is
    /// `107 * 100 + 10_000 + n`, that is `20_700 + n`. The worked example here
    /// used to say `10_700 + n`, which left out the base the whole scheme
    /// rests on — [`Self::split_option`] subtracts it back out and was right
    /// all along, so only this line was wrong. Bands rather than a shared
    /// counter: the mapping stays computable in both directions without a
    /// lookup table that would have to be rebuilt every repaint.
    ///
    /// `index` is required to be below `OPTION_BAND`; the const assertion
    /// below proves that of every list this build can show.
    pub(crate) fn option(self, index: usize) -> HotspotId {
        HotspotId(self.0 * OPTION_BAND + OPTION_BASE + index as u32)
    }

    /// The (control, option) this id refers to, if it is an option id.
    pub(crate) fn split_option(self) -> Option<(HotspotId, usize)> {
        if self.0 < OPTION_BASE {
            return None;
        }
        let rel = self.0 - OPTION_BASE;
        Some((HotspotId(rel / OPTION_BAND), (rel % OPTION_BAND) as usize))
    }
}

/// Width of the id band each dropdown owns.
///
/// Named once and shared by [`HotspotId::option`] and
/// [`HotspotId::split_option`], which are inverses of each other. The radix
/// used to be the literal `100` written into both, which is two places to
/// change and one place to forget.
const OPTION_BAND: u32 = 100;

/// Where the option ids begin, above every fixed control id.
const OPTION_BASE: u32 = 10_000;

/// No dropdown may hold more options than its band is wide.
///
/// With an index of `OPTION_BAND` or more, `option` spills into the next
/// control's band and `split_option` hands the click back as belonging to a
/// *different control*. The scheme is sound only while this holds, and until
/// now it held on a `debug_assert!` — that is, on nothing at all, since the
/// only configuration this project ships is the release one. An invariant
/// checked in no shipped build is not an invariant.
///
/// Checked here instead, at compile time, over the three tables `option_count`
/// answers from. It costs nothing at run time, cannot be skipped by a profile,
/// and a table that outgrew the band would fail the build rather than produce
/// a list whose lower entries silently click the wrong control.
const _: () = assert!(
    crate::strings::LANGUAGE_COUNT < OPTION_BAND as usize
        && crate::ui::theme::Builtin::ALL.len() < OPTION_BAND as usize
        && crate::config::LogLevel::ALL.len() < OPTION_BAND as usize,
    "a dropdown list is longer than the id band each control owns"
);

impl Row {
    /// Whether this row is a button — one of the raised, clickable controls
    /// that take pointer-hover feedback.
    ///
    /// The single source of "is this a button": the row *variant*, decided
    /// once here rather than copied onto each `Hotspot` at paint time. The
    /// `match` is exhaustive with no wildcard, so a new variant will not
    /// compile until its author states which side it falls on — the same
    /// compiler-enforced completeness the event and switch tables rely on. A
    /// forgotten answer is a build error, not a button that silently gives no
    /// feedback.
    pub(crate) fn is_button(&self) -> bool {
        match self {
            Row::LabeledButton { .. } | Row::TitleButton { .. } | Row::Buttons { .. } => true,
            Row::Header(_)
            | Row::Pair { .. }
            | Row::Wrapped { .. }
            | Row::Notice { .. }
            | Row::Checkbox { .. }
            | Row::Dropdown { .. }
            | Row::Field { .. }
            | Row::Space(_) => false,
        }
    }

    /// The blank charged *above* this row's content, in pixels.
    ///
    /// Split out from [`height`](Self::height) because the painter needs the
    /// two terms separately: it draws the content below the lead-in, so it must
    /// know where the content starts as well as how far the whole row advances.
    /// With only the total available it re-derived the lead-in from the theme
    /// itself, which is how the painter came to carry a second copy of this
    /// table.
    ///
    /// Only two variants charge anything. A `Header`'s lead-in separates it
    /// from the section above and therefore exists only when there is one. The
    /// button row's lead-in balances the space above the buttons against the
    /// window's bottom margin below them.
    pub(super) fn lead_in(&self, theme: &ScaledTheme, header_seen: bool) -> i32 {
        match self {
            Row::Header(_) if header_seen => theme.metrics().spacing / 2,
            Row::Buttons { .. } => theme.metrics().button_row_lead(),
            _ => 0,
        }
    }

    /// The height of this row's content, below its lead-in.
    ///
    /// A `Wrapped` row's height depends on the font and on the width it flows
    /// into, so it is measured rather than derived: `wrap_width` is the width
    /// the painter lays the text into —
    /// [`text_column_width`](super::layout::text_column_width), not the client
    /// width — and `metrics` answers from the font it will be drawn in. The
    /// painter draws the block at exactly this height, and since the number is
    /// the height `DrawTextW` will consume, the block is neither short (which
    /// clipped a line) nor long (which left dead space). A `Notice` is always
    /// one line and needs no measurement.
    pub(super) fn body_height(
        &self,
        theme: &ScaledTheme,
        wrap_width: i32,
        metrics: &mut dyn TextMetrics,
    ) -> i32 {
        let line = theme.metrics().row_height;
        let spacing = theme.metrics().spacing;
        match self {
            Row::Space(gap) => gap.px(theme),
            // The trailing half-spacing under a heading is part of the heading:
            // it separates it from its own first row, which is why it is
            // charged here and not as the next row's lead-in.
            Row::Header(_) => line + spacing / 2,
            Row::Pair { .. } => line,
            Row::Wrapped { text, .. } => metrics.wrapped_height(text, wrap_width),
            Row::Notice { .. } => line,
            // A labelled row with an inline control (the buzzer) is exactly as
            // tall as a plain Pair: the button overhangs the line
            // symmetrically rather than growing the row.
            Row::LabeledButton { .. } => line,
            // The title row keeps the taller button; it is a heading, not a
            // metric, and nothing has to line up with it.
            Row::TitleButton { .. } => line + spacing,
            // A checkbox's box is centred inside the line and is three
            // quarters of its height, so it already sits at least a couple of
            // pixels clear of the row's edges — enough for the focus ring
            // without charging for it.
            Row::Checkbox { .. } => line + spacing / 2,
            // A dropdown is one row whether or not its list is open: the list
            // is an overlay over the rows below and adds no height.
            //
            // These two are the only controls that fill their line edge to
            // edge and sit flush against the top of their row, so nothing else
            // can pay for the focus ring drawn around them: the row buys its
            // own. One outset, not two — the ring above a control is paid for
            // by the row before it, which is either one of these (and has just
            // bought it) or a spacer.
            Row::Dropdown { .. } | Row::Field { .. } => line + spacing / 2 + FOCUS_RING_OUTSET,
            // The button box, and nothing trailing it: the window's bottom
            // margin sits directly below, so an extra gap here would only make
            // the buttons float above their own footer.
            Row::Buttons { .. } => line + spacing,
        }
    }

    /// The vertical space this row occupies, in pixels: its lead-in plus its
    /// body.
    ///
    /// One height table for the two readers that must agree on it — the
    /// painter, which advances its cursor row by row, and
    /// [`measure_with`](super::layout::measure_with), which sizes the window to
    /// the sum. They used to carry parallel `match` arms that the comments
    /// begged to be kept in step by hand. The painter now takes both terms from
    /// here, so its advance *is* this number rather than an independent
    /// expression that happens to equal it.
    pub(crate) fn height(
        &self,
        theme: &ScaledTheme,
        wrap_width: i32,
        header_seen: bool,
        metrics: &mut dyn TextMetrics,
    ) -> i32 {
        self.lead_in(theme, header_seen) + self.body_height(theme, wrap_width, metrics)
    }
}

/// A single keyboard edit against the focused field or open list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Edit {
    Insert(char),
    Backspace,
    /// Delete the character to the right of the caret, or the selection.
    Delete,
    Commit,
    Cancel,
    /// Caret movement within the focused field. `extend` is set when Shift is
    /// held, so the same four keys serve both plain movement and selection —
    /// exactly one bit distinguishes them, as in every platform text control.
    CaretLeft {
        extend: bool,
    },
    CaretRight {
        extend: bool,
    },
    CaretHome {
        extend: bool,
    },
    CaretEnd {
        extend: bool,
    },
    /// Select the entire field (Ctrl+A, or a double click).
    SelectAll,
    /// Place the caret at character index `index`, from a mouse click or
    /// drag; `extend` (a shift-click or an in-progress drag) grows the
    /// selection instead of collapsing it. The index is resolved to a
    /// character by the window procedure, which is the only layer with the
    /// device context needed to turn a pixel into a character offset.
    CaretTo {
        index: usize,
        extend: bool,
    },
    /// Move the highlight in an open list. Carried through the same pipeline
    /// as text edits so the window procedure still never touches application
    /// state — it records what was pressed and the owner decides what it
    /// means.
    Highlight(Move),
    /// Wheel notches, positive upward.
    ///
    /// Also carries the stepper buttons and the track's page jumps, which are
    /// the same operation in different sizes: one line, or one screenful.
    /// Giving them their own variants would have meant three ways to say
    /// "move the view by n" and three places to clamp it.
    Scroll(i32),
    /// Put the first visible option at this index, from a thumb drag.
    ///
    /// Absolute rather than relative because that is what dragging a thumb
    /// means: the pointer names a position in the list directly. Expressed as
    /// a delta it would accumulate rounding on every mouse move, and the
    /// thumb would drift away from the cursor over a long drag.
    ScrollTo(usize),
    /// The pointer moved onto this option of the open list.
    Hover(usize),
}

/// How far a key moves the highlight in an open list.
///
/// One `Edit` variant carries all six because they are one operation — put the
/// highlight somewhere else — differing only in the distance. Six variants of
/// `Edit` would be six arms in the owner's match, five of them computing an
/// index the same way, and a seventh key would make it seven.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Move {
    /// Up arrow.
    Previous,
    /// Down arrow.
    Next,
    /// Page Up.
    PageBack,
    /// Page Down.
    PageForward,
    /// Home.
    First,
    /// End.
    Last,
}

impl Move {
    /// The index this move lands on, from `at`, in a list of `count` options.
    ///
    /// Saturating at both ends rather than wrapping: a list is a column with
    /// two ends, and a Page Down that reappeared at the top would lose the
    /// reader's place. Running off the end lands *on* the end, which is also
    /// what makes a second Page Down reach the bottom of any list — the
    /// behaviour the keys are expected to have.
    ///
    /// Pure arithmetic over two integers, so all six are checked without a
    /// window, a draft or a list.
    pub(crate) fn apply(self, at: usize, count: usize) -> usize {
        let last = count.saturating_sub(1);
        match self {
            Move::Previous => at.saturating_sub(1),
            Move::Next => (at + 1).min(last),
            Move::PageBack => at.saturating_sub(DROPDOWN_PAGE),
            Move::PageForward => (at + DROPDOWN_PAGE).min(last),
            Move::First => 0,
            Move::Last => last,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the row model. Everything here is arithmetic over a `Row` and
    //! a `ScaledTheme`: the height a row asks for, the gap it leaves, whether it
    //! counts as a button, and how a keyboard move walks a list. `StubMetrics`
    //! stands in for GDI, which is all the measurement these need.

    use super::super::scrollbar::DROPDOWN_MAX_VISIBLE;
    use super::*;
    use crate::testsupport::StubMetrics;
    use crate::ui::theme::Theme;
    use crate::ui::Dpi;

    /// The six list moves land where the keys they carry are expected to.
    ///
    /// `Move::apply` is the whole of "where does the highlight go", so every
    /// key that moves it is checked here rather than through a draft and a
    /// list that would only re-run the same arithmetic.
    #[test]
    fn every_list_move_lands_where_its_key_promises() {
        let n = DROPDOWN_MAX_VISIBLE * 2;
        assert_eq!(Move::Previous.apply(5, n), 4);
        assert_eq!(Move::Next.apply(5, n), 6);
        assert_eq!(Move::First.apply(5, n), 0);
        assert_eq!(Move::Last.apply(5, n), n - 1);
        assert_eq!(Move::PageForward.apply(0, n), DROPDOWN_PAGE);
        assert_eq!(Move::PageBack.apply(DROPDOWN_PAGE, n), 0);
    }

    /// A move that runs off an end lands on the end rather than wrapping, so
    /// paging twice reaches the bottom of any list and a third page stays
    /// there.
    #[test]
    fn a_list_move_saturates_at_both_ends() {
        let n = DROPDOWN_MAX_VISIBLE + 2;
        let last = n - 1;
        assert_eq!(Move::Previous.apply(0, n), 0);
        assert_eq!(Move::PageBack.apply(1, n), 0);
        assert_eq!(Move::Next.apply(last, n), last);
        assert_eq!(Move::PageForward.apply(last, n), last);
        assert_eq!(
            Move::PageForward.apply(Move::PageForward.apply(0, n), n),
            last,
            "a list barely longer than a page is fully crossed by two pages"
        );
    }

    /// A list with one option, and the degenerate empty one, have nowhere to
    /// go: the arithmetic must not underflow on `count - 1`.
    #[test]
    fn a_list_move_survives_a_list_with_nothing_to_move_in() {
        for mv in [
            Move::Previous,
            Move::Next,
            Move::PageBack,
            Move::PageForward,
            Move::First,
            Move::Last,
        ] {
            assert_eq!(mv.apply(0, 1), 0);
            assert_eq!(mv.apply(0, 0), 0);
        }
    }

    /// A gap scales with the window, like everything else in the layout.
    ///
    /// Rows are built once, at 96 dpi, and measured against a theme scaled to
    /// the window's dpi — so a pixel literal in a row is a pixel literal that
    /// never scales. `Row::Space` held one. At 200 % every gap stayed the size
    /// it had at 100 % while every row around it doubled, and the vertical
    /// rhythm collapsed: half-spacings that should read as "these belong
    /// together" became hairlines between double-height rows.
    ///
    /// Nothing caught it because both readers agreed — the painter and the
    /// sizing pass were consistently wrong, so the window was sized exactly to
    /// the layout it drew. Only the appearance was wrong, and only away from
    /// 96 dpi.
    #[test]
    fn a_gap_scales_with_the_theme_it_is_measured_against() {
        let unscaled = Theme::default();
        let base = unscaled.scaled(Dpi::new(96));
        let doubled = unscaled.scaled(Dpi::new(192));

        for gap in [Gap::Half, Gap::Single] {
            let row = Row::Space(gap);
            let small = row.height(&base, 400, false, &mut StubMetrics::default());
            let large = row.height(&doubled, 400, false, &mut StubMetrics::default());
            assert!(small > 0, "a gap must occupy space");
            assert_eq!(
                large,
                small * 2,
                "at 200 % dpi the gap must double, like the rows around it"
            );
        }

        // And the two steps stay distinct at every scale, or naming them was
        // pointless.
        assert!(Gap::Half.px(&base) < Gap::Single.px(&base));
        assert!(Gap::Half.px(&doubled) < Gap::Single.px(&doubled));
    }

    /// A wrapped row's height is whatever measuring the text says, and a
    /// `Notice` is one line however wide the window is.
    ///
    /// The wrapped height used to be an estimate: characters counted, the
    /// average glyph assumed to be 40 % of the row height, lines divided out.
    /// The painter reserved that block and clipped the text to it, so wherever
    /// the estimate came out short the last line was cut off — and it came out
    /// short by roughly half for Chinese, Japanese and Korean, whose glyphs
    /// advance a full em. It is now measured, by the same call the painter
    /// draws with, so "reserved" and "consumed" are the same number.
    ///
    /// A `Notice` is deliberately not measured: it is one line by contract, and
    /// the interval warning is a `Notice` rather than a `Wrapped` precisely so
    /// its height cannot vary with the window width and resize the dialog as
    /// the warning appears and disappears.
    #[test]
    fn wrapped_is_measured_and_notice_is_one_line() {
        let t = Theme::default().scaled(Dpi::new(96));
        let line = t.metrics().row_height;
        let width = t.metrics().settings_width;
        let mut metrics = StubMetrics::default();
        let text = "Check the UPS is connected over USB and seen as a HID device by Windows.";

        let wrapped = Row::Wrapped {
            text: text.into(),
            color: t.colors().text_primary,
        };
        assert_eq!(
            wrapped.height(&t, width, false, &mut metrics),
            metrics.wrapped_height(text, width),
            "a wrapped row reserves exactly what measuring its text returns"
        );
        // Narrower means taller: the height follows the width it flows into
        // rather than being a property of the row alone.
        assert!(
            wrapped.height(&t, width / 3, false, &mut metrics)
                > wrapped.height(&t, width, false, &mut metrics),
            "a narrower column must wrap into more lines"
        );

        let notice = Row::Notice {
            text: "Interval must be between 1000 and 60000 ms".into(),
            color: t.colors().critical,
        };
        assert_eq!(
            notice.height(&t, width, false, &mut metrics),
            line,
            "a notice is always exactly one line, whatever the window width"
        );
        assert_eq!(
            notice.height(&t, width / 4, false, &mut metrics),
            line,
            "and it does not grow when the window is narrow"
        );
    }

    #[test]
    fn identical_rows_compare_equal() {
        // This is what lets `update` skip a repaint when the reading has not
        // changed; if Row ever stops comparing correctly, the panel silently
        // goes back to repainting once a second for nothing.
        let a = vec![
            Row::Header("Battery".into()),
            Row::Pair {
                label: "Charge".into(),
                value: "100 %".into(),
                color: Color::from_rgb(1, 2, 3),
            },
        ];
        let b = vec![
            Row::Header("Battery".into()),
            Row::Pair {
                label: "Charge".into(),
                value: "100 %".into(),
                color: Color::from_rgb(1, 2, 3),
            },
        ];
        assert!(a == b, "identical content must compare equal");

        let c = vec![
            Row::Header("Battery".into()),
            Row::Pair {
                label: "Charge".into(),
                value: "99 %".into(),
                color: Color::from_rgb(1, 2, 3),
            },
        ];
        assert!(a != c, "a changed reading must compare unequal");
    }

    /// Every variant's body height, arm by arm.
    ///
    /// The rest of this module measures relations — a gap doubles with the
    /// scale, a narrower column wraps taller — and relations hold for a great
    /// many tables that are not this one. Two arms could swap their
    /// expressions, or one lose a term, and every relation would still be
    /// true; the window would simply be sized for a layout it does not draw,
    /// which is the fault this table exists to prevent. So the arms are pinned
    /// by value.
    ///
    /// `Wrapped` and `Notice` are absent on purpose: they are the two arms
    /// whose height is not a constant of the theme, and
    /// `wrapped_is_measured_and_notice_is_one_line` says what they are instead.
    #[test]
    fn every_row_asks_for_the_height_its_kind_is_worth() {
        let t = Theme::default().scaled(Dpi::new(96));
        let line = t.metrics().row_height;
        let spacing = t.metrics().spacing;
        let mut m = StubMetrics::default();

        let expected = [
            (Row::Space(Gap::Half), spacing / 2),
            (Row::Space(Gap::Single), spacing),
            (Row::Header("h".into()), line + spacing / 2),
            (
                Row::Pair {
                    label: "a".into(),
                    value: "b".into(),
                    color: Color::TRANSPARENT,
                },
                line,
            ),
            // The buzzer's button overhangs its line symmetrically rather than
            // growing the row, so this row is exactly as tall as a plain pair.
            (
                Row::LabeledButton {
                    label: "a".into(),
                    value: "b".into(),
                    button: "c".into(),
                    color: Color::TRANSPARENT,
                    id: HotspotId::new(1),
                    enabled: true,
                },
                line,
            ),
            (
                Row::TitleButton {
                    title: "a".into(),
                    button: "b".into(),
                    id: HotspotId::new(2),
                },
                line + spacing,
            ),
            (
                Row::Checkbox {
                    label: "a".into(),
                    checked: false,
                    depth: 0,
                    id: HotspotId::new(3),
                },
                line + spacing / 2,
            ),
            // The two controls that fill their line edge to edge buy their own
            // focus ring; nothing else in the layout pays for it.
            (
                Row::Dropdown {
                    label: "a".into(),
                    options: Vec::new(),
                    selected: 0,
                    highlighted: 0,
                    open: false,
                    scroll: 0,
                    id: HotspotId::new(4),
                },
                line + spacing / 2 + FOCUS_RING_OUTSET,
            ),
            (
                Row::Field {
                    label: "a".into(),
                    value: "b".into(),
                    caret: 0,
                    sel_start: 0,
                    sel_end: 0,
                    id: HotspotId::new(5),
                },
                line + spacing / 2 + FOCUS_RING_OUTSET,
            ),
            (
                Row::Buttons {
                    left: "OK".into(),
                    left_id: HotspotId::new(6),
                    right: "Cancel".into(),
                    right_id: HotspotId::new(7),
                },
                line + spacing,
            ),
        ];

        for (row, want) in &expected {
            assert_eq!(row.body_height(&t, 400, &mut m), *want);
        }

        // An open dropdown is the same height as a closed one: the list is an
        // overlay over the rows below and adds nothing to the column.
        let open = Row::Dropdown {
            label: "a".into(),
            options: vec![DropdownItem {
                label: "o".into(),
                native: String::new(),
                font: None,
            }],
            selected: 0,
            highlighted: 0,
            open: true,
            scroll: 0,
            id: HotspotId::new(4),
        };
        assert_eq!(
            open.body_height(&t, 400, &mut m),
            line + spacing / 2 + FOCUS_RING_OUTSET
        );
    }

    /// The lead-in is charged to two rows and to neither of them always.
    ///
    /// A header leads in only when it follows another section — the first one
    /// sits against the window's own top padding, and charging it again would
    /// push the whole panel down. The button row's lead-in is the counterweight
    /// to the bottom margin below it, so the buttons sit centred in their
    /// footer rather than against it.
    #[test]
    fn only_a_repeated_header_and_the_button_row_lead_in() {
        let t = Theme::default().scaled(Dpi::new(96));
        let header = Row::Header("h".into());
        let buttons = Row::Buttons {
            left: "OK".into(),
            left_id: HotspotId::new(1),
            right: "Cancel".into(),
            right_id: HotspotId::new(2),
        };
        let pair = Row::Pair {
            label: "a".into(),
            value: "b".into(),
            color: Color::TRANSPARENT,
        };

        assert_eq!(header.lead_in(&t, false), 0);
        assert_eq!(header.lead_in(&t, true), t.metrics().spacing / 2);
        // The button row's lead-in does not depend on what came before it.
        assert_eq!(buttons.lead_in(&t, false), t.metrics().button_row_lead());
        assert_eq!(buttons.lead_in(&t, true), t.metrics().button_row_lead());
        assert_eq!(pair.lead_in(&t, false), 0);
        assert_eq!(pair.lead_in(&t, true), 0);
    }

    /// `Row::is_button` is the one source of "is this a button", and it must
    /// answer for the button variants and against the rest. The exhaustive
    /// match in the method itself is what forces a new variant to be
    /// classified at all; this pins the classification of the variants that
    /// exist, so a wrong answer is caught rather than shipped as a button with
    /// no hover feedback or a non-button that lights up.
    #[test]
    fn is_button_names_exactly_the_buttons() {
        let buttons = [
            Row::LabeledButton {
                label: "a".into(),
                value: "b".into(),
                color: Color::TRANSPARENT,
                button: "c".into(),
                id: HotspotId::new(1),
                enabled: true,
            },
            Row::TitleButton {
                title: "a".into(),
                button: "b".into(),
                id: HotspotId::new(2),
            },
            Row::Buttons {
                left: "OK".into(),
                left_id: HotspotId::new(3),
                right: "Cancel".into(),
                right_id: HotspotId::new(4),
            },
        ];
        for row in &buttons {
            assert!(row.is_button(), "this variant should be a button");
        }

        let not_buttons = [
            Row::Header("h".into()),
            Row::Pair {
                label: "a".into(),
                value: "b".into(),
                color: Color::TRANSPARENT,
            },
            Row::Wrapped {
                text: "t".into(),
                color: Color::TRANSPARENT,
            },
            Row::Checkbox {
                label: "a".into(),
                checked: false,
                depth: 0,
                id: HotspotId::new(5),
            },
            Row::Field {
                label: "a".into(),
                value: "b".into(),
                caret: 0,
                sel_start: 0,
                sel_end: 0,
                id: HotspotId::new(6),
            },
            Row::Space(Gap::Half),
        ];
        for row in &not_buttons {
            assert!(!row.is_button(), "this variant should not be a button");
        }
    }
}
