//! The layout arithmetic: how wide the two columns must be, how tall the rows
//! come out, and the palette those numbers are read from.
//!
//! This is the half of the sizing problem that is not a measurement.
//! [`text`](super::text) answers "how wide is this string in this font";
//! everything here is what is done with that answer — the widest label, the
//! widest value, the button that shares a line with one, the floor a
//! full-width row sets, and the memoised result the painter and the window
//! frame both read.
//!
//! [`PanelContent`] is the module's contract with the rest of the program:
//! rows and a size in pixels. [`Columns`] and [`Measured`] are
//! the intermediate terms behind it and stay private to this module tree —
//! the painter is inside that boundary and reads them off the content it was
//! handed, nobody outside it does.

use std::cell::Cell;

use super::row::Row;
use super::scrollbar::scrollbar_width_for;
use super::text::{with_metrics, Script, TextMetrics};
use crate::ui::theme::{ScaledTheme, Theme};
/// What the window should draw. Filled by the caller before each repaint, so
/// the window procedure never reaches back into application state.
#[derive(PartialEq)]
pub(crate) struct PanelContent {
    pub rows: Vec<Row>,
    /// Rows measured but never drawn: the widest text each column has to be
    /// able to hold, whatever the device happens to be reporting.
    ///
    /// Carried rather than passed in again because the window re-measures
    /// itself — at creation against the real monitor, and again on
    /// `WM_DPICHANGED` — and a re-measure without them would answer a
    /// different question than the one that sized the window in the first
    /// place.
    pub(super) samples: Vec<Row>,
    pub width: i32,
    pub height: i32,
    /// The column widths the size above came out of.
    ///
    /// Carried rather than re-derived because the painter needs the same
    /// numbers to place the columns it draws, and it used to answer that by
    /// running the measuring scan a second time behind a cache of its own —
    /// keyed, like the sizing pass's cache, on the text fingerprint. Two caches
    /// answering the same question in two scopes is one too many: this is the
    /// answer, produced once, travelling with the size it produced.
    ///
    /// Private, so this struct can only be built by [`PanelContent::measure`]
    /// and the three numbers cannot come from three different measurements.
    /// `Columns` stays private with it — the module's contract outside is a
    /// size in pixels, not the terms behind it.
    pub(super) columns: Columns,
}

impl PanelContent {
    /// Lays `rows` out and measures the client area they need.
    ///
    /// The only way to build a `PanelContent`, so rows, size and columns are
    /// always one measurement rather than three that happen to agree.
    ///
    /// Width is measured from the strings the window can ever draw rather than
    /// taken from the theme, so a language with compact labels gets a compact
    /// window instead of one sized for the longest translation of any language.
    /// `min_width` is the floor, not the answer.
    ///
    /// `samples` is what makes "can ever draw" different from "is drawing".
    /// Measured from the current strings alone, the width was a function of the
    /// reading: the self-test row's value changes from a dash to "In progress…"
    /// to an outcome, and the window grew and shrank under the pointer as the
    /// test ran. The caller supplies rows carrying the widest value each row
    /// can take — every state name, every outcome, a full-width number — and
    /// the columns are measured over those as well, so the answer moves only
    /// when the language, the theme or the DPI does.
    ///
    /// The real rows are still measured. The samples raise the floor to what
    /// the language can produce; they cannot enumerate what the *device* can —
    /// a serial number, a model string — and those must still fit.
    ///
    /// `theme` and `min_width` both arrive at the 96-DPI baseline and are
    /// scaled here, together, by `dpi`. Together is the point: the width was
    /// once measured at the real DPI while the height was derived from the raw
    /// theme, so a window came out the right width and the wrong height on
    /// every scale but 100 %. A 96-DPI floor compared against an already-scaled
    /// width has the mirror-image fault — it simply never binds above 100 % and
    /// silently does nothing.
    ///
    /// `dpi` is the window's, or 96 when there is no window yet to have one.
    /// That is always correct as a *starting* size: `Window::create` re-measures
    /// against the real monitor's DPI before the window is ever shown, exactly
    /// as `WM_DPICHANGED` re-measures once it is.
    pub(crate) fn measure(
        rows: Vec<Row>,
        samples: Vec<Row>,
        theme: &Theme,
        min_width: i32,
        script: Script,
    ) -> Self {
        let dpi = script.dpi;
        let scaled = theme.scaled(dpi);
        // The floor is a 96-DPI pixel count like every metric, so it is scaled
        // by the same function rather than by a second copy of the arithmetic.
        let floor = dpi.scale_96(min_width);
        let measured = content_size(&rows, &samples, &scaled, script, floor);
        Self {
            rows,
            samples,
            width: measured.width,
            height: measured.height,
            columns: measured.columns,
        }
    }
}

/// The width the content needs, measured in the font that will draw it.
///
/// The panel used to take its width from the theme's `panel_width` alone,
/// which forced one number to serve every language. That number had to fit
/// the widest translation, so the languages with compact scripts — Chinese
/// most visibly — got a window sized for Russian, with their values pushed
/// left and a band of empty background down the right side.
///
/// Here the two columns are measured against the strings actually being
/// drawn: the label column from the widest label, the value column from the
/// widest value, plus whatever the buzzer row's button needs beside its
/// value. `panel_width` survives as a *minimum*, so a theme can still ask for
/// a roomier window and short content will not produce a sliver.
///
/// Measured with the real device context rather than an estimate, because the
/// answer differs by font and by DPI, and an estimate that was close at 96 DPI
/// would drift at 150%. `dpi` is the caller's — the monitor the window is
/// going to (or already does) live on — not read from the device context
/// here; passing the same `dpi` on to `fonts` is what keeps the text measured
/// in the same pixel scale the geometry around it is laid out in. That the
/// geometry *is* in that scale used to be a request made of the caller in this
/// paragraph; it is now the parameter type.
pub(super) fn content_size(
    rows: &[Row],
    samples: &[Row],
    theme: &ScaledTheme,
    script: Script,
    min_width: i32,
) -> Measured {
    // Memoised on the same text fingerprint the painter uses.
    //
    // This is called once per rebuild of the settings rows, which during a
    // thumb drag means once per mouse move — and each call creates a device
    // context, selects fonts into it and measures every string the window
    // draws, twenty-four language names included. The scroll offset changing
    // cannot change any of those widths, so the second and subsequent calls
    // were computing an answer already known.
    //
    // Thread-local rather than a global: this is the UI thread's cache, and
    // everything that reaches here is confined to it by construction, so no
    // lock is needed on the hot path.
    /// Every input the measurement depends on.
    ///
    /// *Every* one — a cache key that covers most of them is a cache that
    /// returns another window's answer. Two of these used to be missing, with
    /// a comment arguing that `dpi` stood in for them: it does not. `dpi` and
    /// the point size are independent (`font_height` combines them), and the
    /// only reason no collision had been observed is that both built-in
    /// themes inherit one `font_points` from `base()`. `Theme` already declares
    /// `font_points` as a field, so a theme carrying its own would have handed
    /// one window the other's measurement — and the symptom is clipped text
    /// with no message anywhere.
    #[derive(Clone, Copy, PartialEq)]
    struct Key {
        /// Fingerprint of the row text and the rounded theme metrics.
        text: u64,
        /// Everything about the font the measurement was made under: the
        /// family non-Latin runs are set in, the scale the fonts were created
        /// at, and their size.
        ///
        /// One field, because they arrive as one value. They used to be three
        /// entries here — a family, a `dpi` the caller passed separately, and a
        /// point size read out of a thread-local and stored by bits — which is
        /// four names for three facts, kept in step by hand.
        script: Script,
        /// The width floor, which decides how wrapped rows break and so how
        /// tall they are.
        min_width: i32,
    }

    /// How many windows the cache serves at once.
    ///
    /// One was one too few. The panel and the settings dialog are measured
    /// alternately — the panel on every tick while the dialog is open — so a
    /// single slot was evicted on each call and the cache did no work at all
    /// in the one situation it was written for. Four covers the panel, a
    /// modal and a confirmation over it with a slot to spare, and the entries
    /// are small enough that the scan below is cheaper than a hash.
    const CACHE_SLOTS: usize = 4;

    thread_local! {
        /// Most-recently-used first; the last slot is the one evicted.
        static CACHE: std::cell::Cell<[Option<(Key, Measured)>; CACHE_SLOTS]> =
            const { std::cell::Cell::new([None; CACHE_SLOTS]) };
    }

    let key = Key {
        text: text_fingerprint(rows, theme) ^ text_fingerprint(samples, theme).rotate_left(1),
        script,
        min_width,
    };

    let mut slots = CACHE.with(Cell::get);
    let hit = slots.iter().enumerate().find_map(|(i, entry)| match entry {
        Some((cached, measured)) if *cached == key => Some((i, *measured)),
        _ => None,
    });
    if let Some((pos, measured)) = hit {
        // Touched entries move to the front, which is what makes the last
        // slot the least recently used one.
        slots.copy_within(..pos, 1);
        slots[0] = Some((key, measured));
        CACHE.with(|c| c.set(slots));
        return measured;
    }

    let measured = with_metrics(script, |m| {
        let (needed, columns) = content_geometry(rows, samples, theme, m);
        let width = needed.max(min_width);
        // The text column, from the same `column_origin` the painter lays
        // rows out with, so a wrapped row is measured at the width it is
        // drawn into rather than at an approximation of it.
        let height = measure_with(rows, theme, text_column_width(width, columns, theme), m);
        Measured {
            width,
            height,
            columns,
        }
    });
    let mut slots = CACHE.with(Cell::get);
    slots.copy_within(..CACHE_SLOTS - 1, 1);
    slots[0] = Some((key, measured));
    CACHE.with(|c| c.set(slots));
    measured
}

/// What one measuring pass produces: the client area the rows need, and the
/// column widths that answer came out of.
///
/// The columns are returned rather than discarded because the painter needs
/// them too, and recomputing them there meant running the scan that measures
/// every string in the window a second time per repaint.
#[derive(Clone, Copy)]
pub(super) struct Measured {
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) columns: Columns,
}

/// The content width and the column widths behind it, in one pass.
///
/// Both come out of the same `column_needs` scan because both are answers to
/// the same question, and that scan measures every string the window draws —
/// twenty-four language names among them. Computing the width in one place and
/// the columns in another meant running it twice per rebuild, which during a
/// thumb drag is twice per mouse move.
///
/// Split from the device context so the arithmetic — which columns exist, what
/// each row contributes, how they combine — can be tested without one. The GDI
/// call is the part that cannot run off Windows; the layout rules are the part
/// worth testing, and they were previously welded to it.
pub(super) fn content_geometry(
    rows: &[Row],
    samples: &[Row],
    theme: &ScaledTheme,
    metrics: &mut dyn TextMetrics,
) -> (i32, Columns) {
    let pad = theme.metrics().padding;

    // Measured but never drawn (see [`PanelContent::measure`]): rows carrying
    // the widest text each column has to be able to hold, so the answer is a
    // property of the language rather than of this instant's reading.
    //
    // The samples raise the floor to the domain; the real rows keep their say
    // for anything the domain cannot enumerate — a serial number, a model name
    // — so nothing that used to fit stops fitting.
    //
    // One scan over both sets, not one scan each folded together afterwards.
    // For the two column widths the two forms are equal — a maximum over a
    // union is the maximum of the maxima — but the shared action-button width
    // is not a per-row quantity: it is one number the whole window is drawn
    // with, and a scan that sees only half the rows can only answer it for that
    // half. Folding two half-answers afterwards gives the right number for the
    // window and the wrong one for every row charge computed on the way there.
    let needs = column_needs(rows.iter().chain(samples), theme, metrics);

    // Both columns are given the same width, and the value column therefore
    // starts exactly on the window's vertical centre line in every language.
    let width = pad * 2 + (needs.half * 2).max(needs.wide_single);
    (width, needs)
}

/// Hash of everything in `rows` that affects the measured *size*.
///
/// Width and height both, because both are now answered by one memoised call
/// ([`content_size`]). While this keyed the width alone it could leave out
/// anything a column was not measured from — a `Wrapped` row's text sets no
/// column, so it was skipped. As a key for the height that is wrong: the
/// wrapped text *is* the height. A panel showing nothing but a warning could
/// then swap one warning for another of the same shape, hit the cached size,
/// and be sized for the text it no longer shows — which is the clipping this
/// measurement exists to prevent, re-entering through the cache.
///
/// Only the *text* and the structural numbers that enter the arithmetic —
/// not scroll offsets, highlights, open flags or tick states, because none
/// of those change how large a row is. That exclusion is the whole value
/// of the cache: dragging a thumb and arrowing through a list alter the rows
/// on every frame while leaving every measurement identical.
///
/// A hash rather than a stored copy of the strings: the settings dialog
/// carries twenty-four option labels plus their native names, and cloning
/// them each repaint to compare would cost more than the measurement being
/// avoided.
pub(super) fn text_fingerprint(rows: &[Row], theme: &ScaledTheme) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();

    // The metrics participate whole, not field by field.
    //
    // They scale the margins, the arrow and the scrollbar, so a theme change
    // must invalidate the cache even with identical text. Six of the nine used
    // to be listed here by hand, which is the same hazard a hand-written
    // field list one screen away in this file already showed: a key that
    // covers most of its inputs returns somebody else's answer for the rest.
    // `corner_radius`, `panel_width` and `settings_width` were the three
    // missing, and they were missing because nobody could see they were — a
    // list beside a struct is a list the next field has to be remembered
    // into.
    //
    // Hashing the struct removes the question. `Metrics` derives `Hash`, so a
    // metric added tomorrow reaches the key without anybody deciding it should.
    // The colours deliberately do not: they change what is *drawn* and never
    // what is *measured*, and a repaint in a new palette must not throw away a
    // measurement that is still correct.
    theme.metrics().hash(&mut h);

    for row in rows {
        std::mem::discriminant(row).hash(&mut h);
        match row {
            Row::Pair { label, value, .. } => {
                label.hash(&mut h);
                value.hash(&mut h);
            }
            Row::Field { label, value, .. } => {
                label.hash(&mut h);
                value.hash(&mut h);
            }
            Row::LabeledButton {
                label,
                value,
                button,
                ..
            } => {
                label.hash(&mut h);
                value.hash(&mut h);
                button.hash(&mut h);
            }
            Row::Dropdown { label, options, .. } => {
                label.hash(&mut h);
                // Every option, because the closed control is sized to the
                // widest of them. `open`, `scroll`, `selected` and
                // `highlighted` are deliberately absent.
                //
                // `font` is here because `TextMetrics::option_width` reads it:
                // the native part is measured in that family, so two option
                // lists with identical strings and different families measure
                // differently. It was missing, and the omission did not show
                // only because the family is chosen by the language and the
                // language names differ — an invariant held by a coincidence
                // in the data rather than by construction, which is the exact
                // shape this cache's key is supposed to rule out.
                for o in options {
                    o.label.hash(&mut h);
                    o.native.hash(&mut h);
                    o.font.hash(&mut h);
                }
            }
            Row::Header(text) => text.hash(&mut h),
            Row::Checkbox { label, depth, .. } => {
                label.hash(&mut h);
                depth.hash(&mut h);
            }
            Row::Buttons { left, right, .. } => {
                left.hash(&mut h);
                right.hash(&mut h);
            }
            Row::TitleButton { title, button, .. } => {
                title.hash(&mut h);
                button.hash(&mut h);
            }
            // Sets no column, but its text is its height: it is the one row
            // whose size is measured from the string rather than derived from
            // the theme.
            Row::Wrapped { text, .. } => text.hash(&mut h),
            // Which step the gap is, since the two are different heights. The
            // discriminant above only says that this row is a spacer.
            Row::Space(gap) => gap.hash(&mut h),
            // Neither its width nor its height depends on its text: it sets no
            // column and is one line by contract.
            Row::Notice { .. } => {}
        }
    }
    h.finish()
}

/// What the two columns and the full-width rows demand of the window, and the
/// one button width the value column was charged for.
///
/// `half` is the width each column gets — the larger of the two sides, since
/// both are drawn at the same width so that the divide falls on the centre.
/// `wide_single` is the floor set by rows that span the window instead of
/// splitting, and so cannot be served by either half.
///
/// Private, like `column_origin`: this is the layout rule's
/// internal arithmetic, and the module's contract with the rest of the
/// program is [`PanelContent`] — rows and a size in pixels — not the
/// intermediate terms behind it. The painter is inside that boundary and does
/// read these, off the content it was handed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Columns {
    pub(super) half: i32,
    pub(super) wide_single: i32,
    /// The shared width of the value-column action buttons, from the scan that
    /// charged the column for them.
    ///
    /// Carried for the same reason the two column widths are: the painter has
    /// to draw the button at the width the window was measured under, and it
    /// used to answer that by measuring the captions again over the rows it
    /// happened to be drawing. Those are not the same set — the sizing pass
    /// also sees the samples, which hold every caption the button can take —
    /// so the two agreed only while the drawn caption happened to be the
    /// widest one. The buzzer's verb alternates between two words of different
    /// length, and the button resized under the pointer each time it was
    /// pressed: the window no longer moved, but the control inside it did.
    pub(super) action_button: i32,
}

/// The width one button needs to hold `caption`: the text plus symmetric
/// padding, never below the theme's minimum.
///
/// The single formula every button in both windows is sized by. It used to be
/// written out at each site, and the copies disagreed — `spacing * 4` where the
/// painter drew, `spacing * 2` where the window was measured — so the window
/// was sized for a button narrower than the one drawn into it.
pub(super) fn button_width_for(
    caption: &str,
    btn_min: i32,
    spacing: i32,
    mut measure: impl FnMut(&str) -> i32,
) -> i32 {
    (measure(caption) + spacing * 4).max(btn_min)
}

/// The shared width of the value-column action buttons — the buzzer and the
/// self-test. Both take the widest caption any of `rows` needs, so they always
/// come out equal rather than each sizing to its own word, which looked ragged
/// when one caption was much longer than the other (Turkish "Öz test" beside a
/// long buzzer verb, say). Floored at `btn_min` by [`button_width_for`], so a
/// panel with a single short caption still gets an aimable button.
///
/// Takes rows as an iterator rather than a slice because the set it must scan
/// is the real rows *and* the samples, and those are two slices. Which captions
/// are in that set is the whole of this function's correctness: over the drawn
/// rows alone the answer is the width of the caption currently showing, which
/// is not a width at all but a reading.
fn action_button_width<'a>(
    rows: impl IntoIterator<Item = &'a Row>,
    btn_min: i32,
    spacing: i32,
    mut measure: impl FnMut(&str) -> i32,
) -> i32 {
    rows.into_iter()
        .filter_map(|r| match r {
            Row::LabeledButton { button, .. } => {
                Some(button_width_for(button, btn_min, spacing, &mut measure))
            }
            _ => None,
        })
        .max()
        .unwrap_or(btn_min)
}

/// The width of *each* button in a dialog's OK/Cancel row.
///
/// The pair is symmetric — a Cancel wider than its OK reads as a mistake — so
/// both take whatever the wider caption needs. The floor is deliberately larger
/// than elsewhere: a dialog's buttons are the window's main controls and are
/// drawn at the theme minimum plus a spacing on each side, which is the width
/// they have always been drawn at.
///
/// What is new is that the width now *grows* with the caption. The painter drew
/// this floor unconditionally and nothing else, so a translation wider than it
/// was clipped; the measurer, meanwhile, charged the window for a button that
/// grew with the text, so the two described different buttons. Neither was
/// wrong on its own terms and nothing failed — the window was simply sized for
/// a layout it did not draw.
pub(super) fn dialog_button_width(
    left: &str,
    right: &str,
    btn_min: i32,
    spacing: i32,
    mut measure: impl FnMut(&str) -> i32,
) -> i32 {
    let fitted = button_width_for(left, btn_min, spacing, &mut measure).max(button_width_for(
        right,
        btn_min,
        spacing,
        &mut measure,
    ));
    fitted.max(btn_min + spacing * 2)
}

/// Measures both columns in one pass.
///
/// Extracted so the painter and the measurer cannot disagree. They ran the
/// same scan in two places before, which is two things to keep in step and
/// one of them to forget: a row kind added to the measurer alone sizes a
/// window for content the painter lays out differently, and the symptom is a
/// clipped value column rather than anything that names its cause.
///
/// (This paragraph spent some time stranded above `button_width_for`, glued
/// to that function\'s own doc comment with no blank line between them, so
/// rustdoc read the pair as one block describing the wrong function. Nothing
/// warns about that, and in a nine-thousand-line file nothing shows it
/// either.)
///
/// `rows` is an iterator, and a `Clone` one, because the scan is two passes
/// over the same set: the action buttons first, because their shared width is
/// a term in what the value column is then charged, and the rows after. A
/// slice would have forced the caller with two slices — the real rows and the
/// samples — to scan each separately and combine the results, which is the one
/// arrangement in which the button width cannot come out right.
pub(super) fn column_needs<'a>(
    rows: impl IntoIterator<Item = &'a Row> + Clone,
    theme: &ScaledTheme,
    metrics: &mut dyn TextMetrics,
) -> Columns {
    // Takes the metrics themselves rather than a `&str -> i32` closure over
    // them. A dropdown option is not measurable through that closure — it is
    // set in two fonts, and only `TextMetrics::option_width` knows which —
    // and a closure that could not express the question is what left this
    // function measuring option text in the wrong typeface.
    let spacing = theme.metrics().spacing;
    let line = theme.metrics().row_height;
    let btn_min = theme.metrics().button_min_width;
    // The shared width of the two value-column buttons, decided before the
    // loop below because it is a term in what that loop charges the column,
    // and returned with the result because it is also the width the painter
    // draws at. One number, one place it is decided.
    let act_btn = action_button_width(rows.clone(), btn_min, spacing, |s| {
        metrics.line_width(s, false)
    });

    let mut label_w = 0;
    let mut value_w = 0;
    let mut wide_single = 0;

    for row in rows {
        match row {
            Row::Pair { label, value, .. } => {
                label_w = label_w.max(metrics.line_width(label, false));
                value_w = value_w.max(metrics.line_width(value, false));
            }
            Row::Dropdown { label, options, .. } => {
                label_w = label_w.max(metrics.line_width(label, false));
                // The widest option, not merely the selected one: the closed
                // control must not be narrower than the list it opens, or the
                // list would overhang its own control.
                let widest = options
                    .iter()
                    .map(|o| metrics.option_width(o))
                    .max()
                    .unwrap_or(0);
                // Plus the box padding, the disclosure arrow, and — only when
                // the list is long enough to need one — the scrollbar.
                //
                // The bar is drawn *inside* the list frame, over the right
                // edge of the option rows, so without this term it eats the
                // last few characters of the longest option. A 4px indicator
                // could be ignored; a full 16px control with two buttons
                // cannot. Charged here rather than added to the window
                // afterwards, because the same measurement decides where the
                // options are clipped: measure without it and the list is
                // exactly as wide as its text, then the bar covers the text.
                //
                // Conditional on `options.len() > DROPDOWN_MAX_VISIBLE`, and
                // that condition is the whole point: a list that fits shows
                // no scrollbar, so charging it for one would pad the window
                // with a strip of nothing. The log-level list has two
                // options and must not widen the dialog by a control it will
                // never draw.
                value_w = value_w
                    .max(widest + spacing * 2 + line + scrollbar_width_for(options.len(), line));
            }
            Row::Field { label, value, .. } => {
                label_w = label_w.max(metrics.line_width(label, false));
                value_w = value_w.max(metrics.line_width(value, false) + spacing * 4);
            }
            Row::LabeledButton { label, value, .. } => {
                label_w = label_w.max(metrics.line_width(label, false));
                // The button belongs to the value column: it sits on the same
                // line, right of the value, so the column has to hold both or
                // the button lands outside the window.
                //
                // Charged against this row's *own* value plus the button, not
                // the widest value of any row. Added as a separate term to the
                // column maximum, the button would stack on top of the widest
                // value from a different row and pad the label column out with
                // empty space.
                //
                // No trailing gap after the button: the right margin is already
                // `origin`. The button is charged at the shared action width —
                // the same one the draw uses — so both value-column buttons come
                // out equal and the column matches what is drawn.
                value_w = value_w.max(metrics.line_width(value, false) + spacing + act_btn);
            }
            // Rows spanning the full width rather than splitting into two
            // columns. They set a floor, not a column.
            Row::Header(text) => wide_single = wide_single.max(metrics.line_width(text, true)),
            Row::Checkbox { label, depth, .. } => {
                wide_single = wide_single.max(
                    theme.metrics().indent * i32::from(*depth)
                        + line
                        + spacing
                        + metrics.line_width(label, false),
                );
            }
            Row::Buttons { left, right, .. } => {
                // Both buttons are the same width, because that is what the
                // painter draws; charging each side its own fitted width made
                // the sum describe a row that never existed.
                let bw = dialog_button_width(left, right, btn_min, spacing, |s| {
                    metrics.line_width(s, false)
                });
                wide_single = wide_single.max(bw * 2 + spacing);
            }
            Row::TitleButton { title, button, .. } => {
                wide_single = wide_single.max(
                    metrics.line_width(title, true)
                        + spacing * 2
                        + button_width_for(button, btn_min, spacing, |s| {
                            metrics.line_width(s, false)
                        }),
                );
            }
            // Wrapped text flows to whatever width it is given, so it cannot
            // ask for one. A notice does not widen the window either: it is
            // contracted to fit the width the rest of the dialog sets, so
            // that an invalid interval does not resize the window as the
            // warning appears and disappears.
            Row::Wrapped { .. } | Row::Notice { .. } | Row::Space(_) => {}
        }
    }

    // The half is whichever side needs more: the labels plus the gap that
    // separates them from the values, or the values plus whatever the buzzer
    // row's button adds beside them. Sizing both halves to the larger of the
    // two is what makes the centre line hold — charging each side only what
    // it happens to need is what let the whole block drift left of centre,
    // because the label column is much narrower than the value column in the
    // compact scripts and much wider in the verbose ones.
    //
    // The gap belongs to the label half, not between the halves: it is the
    // whitespace a label may not encroach on, so counting it on the left is
    // what keeps the value column's left edge at the centre rather than a gap
    // past it.
    Columns {
        half: (label_w + theme.metrics().column_gap).max(value_w),
        wide_single,
        action_button: act_btn,
    }
}

/// Where the two columns begin, and how wide each of them is.
///
/// The layout contract in one place, because the painter and the measurer
/// must agree on it exactly: a window sized by one rule and painted by
/// another draws its values into the wrong half, which is the fault this
/// replaced. Returns `(left_edge, half_width)`, so the label column occupies
/// `left_edge .. left_edge + half` and the value column begins at
/// `left_edge + half` — the centre of the window whenever the columns are
/// what sized it.
///
/// `content` is the full client width. When a full-width row forced the window
/// wider than the columns needed, the surplus goes into the *columns*: each
/// half grows to half the usable width, so the divide lands on the centre of
/// the window and the margins stay at `padding`. `slack` is what is left after
/// that — never more than one pixel, from the odd-width case — and it is what
/// keeps the two margins equal to the pixel.
///
/// The description here used to say the surplus was split between the margins.
/// The observable result is the same, and
/// `a_wide_single_row_keeps_the_columns_centred` pins it, but the mechanism is
/// not: a reader following that account would expect wide columns to be left at
/// `half_need` and the difference spent on the sides, which is a different
/// layout the moment anything reads `half` for itself. The alternative both
/// accounts rule out — spending the surplus on the right alone — is the band of
/// dead background that made the panel look shifted.
pub(super) fn column_origin(content: i32, half_need: i32, theme: &ScaledTheme) -> (i32, i32) {
    let pad = theme.metrics().padding;
    let usable = content - pad * 2;
    // Never below what the columns need; a window narrower than its content
    // clips rather than centres.
    let half = half_need.max(usable / 2);
    let slack = (usable - half * 2).max(0);
    (pad + slack / 2, half)
}

/// The width full-width text is laid into, given the client width and the
/// column widths behind it.
///
/// Not `width - padding * 2` in general: the text column starts wherever
/// [`column_origin`] puts it, which is the padding plus whatever half-pixel of
/// slack the column arithmetic left over. Asking `column_origin` rather than
/// assuming the padding is what keeps this measurement and the painter on the
/// same origin; the estimate it replaces assumed a line wider than the one
/// drawn, which was one of the two reasons it reserved too few lines.
///
/// Takes the columns rather than measuring them, because both callers already
/// have them: `content_size` from the pass that produced the width, and the
/// `Notice` test from the same. Measuring them again here would run the scan
/// that measures every string in the window a second time per rebuild.
pub(super) fn text_column_width(width: i32, needs: Columns, theme: &ScaledTheme) -> i32 {
    let (origin, _) = column_origin(width, needs.half, theme);
    (width - origin * 2).max(1)
}

/// Measures how tall the given rows will be, so the window can be sized to its
/// content once the locale is known rather than to a hardcoded constant.
///
/// `wrap_width` is the text column — what [`text_column_width`] returns for the
/// client width, not the client width itself. Wrapped rows are measured at the
/// width they are laid into, so "reserved" and "drawn" are one number.
pub(crate) fn measure_with(
    rows: &[Row],
    theme: &ScaledTheme,
    wrap_width: i32,
    metrics: &mut dyn TextMetrics,
) -> i32 {
    let pad = theme.metrics().padding;

    // Mirrors the paint arm: the lead-in above a heading is charged for every
    // heading except the first. The per-row height itself lives on `Row`, so
    // the painter and this sum cannot disagree on it; only the running
    // `header_seen` flag is tracked here, exactly as the painter tracks it.
    let mut header_seen = false;

    let mut h = pad * 2 + theme.metrics().bottom_slack();
    for row in rows {
        h += row.height(theme, wrap_width, header_seen, metrics);
        if matches!(row, Row::Header(_)) {
            header_seen = true;
        }
    }
    h
}

#[cfg(test)]
mod tests {
    //! Tests for the layout arithmetic.
    //!
    //! The largest group by far, because this is where a mistake is invisible:
    //! a window sized from one number and painted from another looks like a
    //! clipped column, not like a failure. Most of these are therefore
    //! relations rather than fixed pixel counts — narrower wraps taller, a
    //! longer label yields a wider window, the measured width holds both
    //! columns — which hold for any consistent set of measurements and so
    //! survive `StubMetrics` standing in for GDI.

    use super::super::row::{DropdownItem, Gap, HOT_BEEPER, HOT_SELFTEST, HOT_SETTINGS};
    use super::super::scrollbar::{scrollbar_width, DROPDOWN_MAX_VISIBLE};
    use super::*;
    use crate::color::Color;
    use crate::testsupport::StubMetrics;
    use crate::ui::row::HotspotId;
    use crate::ui::Dpi;

    /// The content width alone, for the tests that are about the width alone.
    fn content_geometry_width(
        rows: &[Row],
        theme: &ScaledTheme,
        metrics: &mut dyn TextMetrics,
    ) -> i32 {
        content_geometry(rows, &[], theme, metrics).0
    }

    /// Every button is measured by the formula it is drawn by.
    ///
    /// The two paths held separate copies and the copies disagreed. The title
    /// button was drawn at `caption + spacing * 4` and measured at
    /// `caption + spacing * 2`, so the window was up to two spacings too narrow
    /// and the Settings button ran into the title beside it. The dialog row was
    /// the mirror image: the painter drew a fixed width and the measurer
    /// charged for one that grew with the caption, so a long translation was
    /// clipped inside a window that had already paid for the room.
    ///
    /// Both formulas live in one function each now, so this checks the property
    /// those functions have to have — the width is fitted to the caption and
    /// never below the floor — rather than re-deriving the arithmetic and
    /// becoming a third copy of it.
    #[test]
    fn button_widths_are_fitted_and_floored() {
        let t = Theme::default().scaled(Dpi::new(96));
        let spacing = t.metrics().spacing;
        let btn_min = t.metrics().button_min_width;
        // A stand-in for GDI: proportional to the caption, so a longer caption
        // is a wider one.
        let mut measure = |s: &str| s.chars().count() as i32 * 7;

        // Short captions sit on the floor.
        let short = button_width_for("OK", btn_min, spacing, &mut measure);
        assert_eq!(short, btn_min, "a short caption gets the theme minimum");

        // A caption too long for the floor widens the button rather than being
        // clipped by it.
        let long = "A caption far longer than any minimum width";
        let wide = button_width_for(long, btn_min, spacing, &mut measure);
        assert!(
            wide > btn_min && wide >= measure(long) + spacing * 4,
            "a long caption must widen its button, not be clipped by it"
        );

        // A dialog pair is symmetric and sits on its own, larger floor.
        let floor = dialog_button_width("OK", "Cancel", btn_min, spacing, &mut measure);
        assert_eq!(
            floor,
            btn_min + spacing * 2,
            "dialog buttons keep the wider floor they have always been drawn at"
        );
        assert_eq!(
            dialog_button_width("Cancel", "OK", btn_min, spacing, &mut measure),
            floor,
            "the pair must not depend on which caption is on which side"
        );
        let grown = dialog_button_width("OK", long, btn_min, spacing, &mut measure);
        assert!(
            grown > floor,
            "one long caption widens both buttons, or the pair is asymmetric"
        );
    }

    /// A dropdown whose list scrolls is measured wider than the same list
    /// without a scrollbar, by exactly the bar.
    ///
    /// The bar is painted inside the list frame, over the right edge of the
    /// option rows. Measured without it, the window is exactly as wide as the
    /// longest option and the bar then covers that option's last characters —
    /// which on the language list means the closing parenthesis of the native
    /// name, and on a long one rather more.
    #[test]
    fn a_scrolling_list_is_measured_wide_enough_for_its_bar() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();

        let option = |n: usize| DropdownItem {
            label: format!("Language {n}"),
            native: String::new(),
            font: None,
        };
        let row = |count: usize| {
            vec![Row::Dropdown {
                label: "Language".into(),
                options: (0..count).map(option).collect(),
                selected: 0,
                highlighted: 0,
                open: false,
                scroll: 0,
                id: HotspotId::new(1_000),
            }]
        };

        let fits = content_geometry_width(&row(DROPDOWN_MAX_VISIBLE), &t, &mut m);
        let scrolls = content_geometry_width(&row(DROPDOWN_MAX_VISIBLE + 1), &t, &mut m);
        let bar = scrollbar_width(t.metrics().row_height);

        assert_eq!(
            scrolls - fits,
            bar * 2,
            "the scrolling list must gain exactly the bar; both halves are \
             sized to the larger, so a column term costs twice"
        );
        assert!(bar >= 12, "a {bar}px bar is too narrow to click");
    }

    /// Long wrapped text must be measured taller than short text.
    ///
    /// The estimate was a flat two lines that ignored the width parameter
    /// entirely — the sizing pass took `width` and discarded it. Any
    /// message needing a third line was clipped, and the warnings are exactly
    /// the rows that run long: an unwritable-config message carrying a full
    /// multi-device warning runs past a hundred characters in Russian.
    #[test]
    fn wrapped_text_is_measured_against_the_available_width() {
        let t = Theme::default().scaled(Dpi::new(96));
        let short = measure_with(
            &[Row::Wrapped {
                text: "Short".into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            350,
            &mut StubMetrics::default(),
        );
        let long = measure_with(
            &[Row::Wrapped {
                text: "Several matching HID devices were found. The first \
                       one is in use, and its readings may belong to a \
                       different power source."
                    .into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            350,
            &mut StubMetrics::default(),
        );
        assert!(
            long > short,
            "a message needing three lines must not be measured as two"
        );

        // And a narrower window must reserve at least as much, never less:
        // the same text wraps into more lines, not fewer.
        let narrow = measure_with(
            &[Row::Wrapped {
                text: "Several matching HID devices were found. The first \
                       one is in use, and its readings may belong to a \
                       different power source."
                    .into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            250,
            &mut StubMetrics::default(),
        );
        assert!(narrow >= long, "narrower means more lines, not fewer");
    }

    /// Short text reserves one line, because one line is what it needs.
    ///
    /// It used to reserve two. That floor was a hedge against the estimate: a
    /// guess that could come out short needed slack, and slack below a message
    /// is invisible while a clipped line is not. With the height measured
    /// rather than guessed there is nothing to hedge against, and the floor
    /// only left a blank line under every short message and made every window
    /// holding one taller than its contents.
    #[test]
    fn short_wrapped_text_reserves_exactly_one_line() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut metrics = StubMetrics::default();
        let h = measure_with(
            &[Row::Wrapped {
                text: "OK".into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            350,
            &mut metrics,
        );
        assert_eq!(
            h - (t.metrics().padding * 2 + t.metrics().bottom_slack()),
            metrics.line_height,
            "a message that fits on one line must occupy one line"
        );
    }

    /// The buzzer row must be exactly as tall as an ordinary metric row.
    ///
    /// It used to advance by `line + spacing` because of the button on it,
    /// which put a visible gap around the buzzer that no other reading had.
    /// The button now overhangs the line instead of growing it.
    #[test]
    fn a_row_with_an_inline_button_is_no_taller_than_a_plain_row() {
        let t = Theme::default().scaled(Dpi::new(96));
        let plain = measure_with(
            &[Row::Pair {
                label: "Buzzer".into(),
                value: "Enabled".into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            350,
            &mut StubMetrics::default(),
        );
        let with_button = measure_with(
            &[Row::LabeledButton {
                label: "Buzzer".into(),
                value: "Enabled".into(),
                color: Color::TRANSPARENT,
                button: "Disable".into(),
                id: HOT_BEEPER,
                enabled: true,
            }],
            &t,
            350,
            &mut StubMetrics::default(),
        );
        assert_eq!(
            plain, with_button,
            "the buzzer row must sit on the same rhythm as every other metric"
        );
    }

    /// A heading gets the same space above it as below, except the first one.
    #[test]
    fn headers_are_spaced_symmetrically_but_not_the_first() {
        let t = Theme::default().scaled(Dpi::new(96));
        let one = measure_with(
            &[Row::Header("Input".into())],
            &t,
            350,
            &mut StubMetrics::default(),
        );
        let two = measure_with(
            &[Row::Header("Input".into()), Row::Header("Output".into())],
            &t,
            350,
            &mut StubMetrics::default(),
        );
        let line = t.metrics().row_height;
        let spacing = t.metrics().spacing;

        // The second heading costs its own line, the trailing gap, and the
        // lead-in that the first one does not get.
        assert_eq!(
            two - one,
            line + spacing / 2 + spacing / 2,
            "a following heading must be charged for the space above it"
        );
        // And the first heading is not, or the whole column is pushed away
        // from the title for no reason.
        assert_eq!(
            one,
            t.metrics().padding * 2 + t.metrics().bottom_slack() + line + spacing / 2,
            "the first heading has nothing above it to be separated from"
        );
    }

    /// The space below the last row must match the space above the title as
    /// the eye sees it, not merely as `pad * 2` claims.
    ///
    /// The title is centred inside a row taller than its own text, which
    /// donates `spacing / 2` of blank space above the words. Without the same
    /// allowance at the bottom the panel looks bottom-tight, which is exactly
    /// the complaint this answers.
    #[test]
    fn bottom_padding_matches_the_visual_top_gap() {
        let t = Theme::default().scaled(Dpi::new(96));
        let pad = t.metrics().padding;
        let spacing = t.metrics().spacing;
        let line = t.metrics().row_height;

        let rows = vec![
            Row::TitleButton {
                title: "UPS Monitor".into(),
                button: "Settings".into(),
                id: HOT_SETTINGS,
            },
            Row::Pair {
                label: "Buzzer".into(),
                value: "Enabled".into(),
                color: Color::TRANSPARENT,
            },
        ];
        let h = measure_with(&rows, &t, 350, &mut StubMetrics::default());

        // Height above the title text, as rendered: the reserved padding plus
        // the half-row the centred title leaves clear.
        let visual_top = pad + spacing / 2;
        // Height below the last row: everything not consumed by the rows.
        let consumed = (line + spacing) + line;
        let visual_bottom = h - pad - consumed;

        assert_eq!(
            visual_top, visual_bottom,
            "the gap under the last row must equal the gap over the title"
        );
    }

    /// The space over the settings buttons must equal the space under them, and
    /// it must hold whether or not the interval warning is showing.
    ///
    /// The row before the buttons leaves the same `spacing / 2` of blank below
    /// its last line of text in both tails — a checkbox does so by its own
    /// trailing half-line, and the wrapped warning is given a matching
    /// `Space(spacing / 2)`. So the last line of text sits at the button row's
    /// start minus `spacing / 2`, the button box sits `button_row_lead` further
    /// down, and the space under the box is the window's bottom margin. This
    /// walks both tails through `Row::height`, the table the painter advances
    /// by, and checks the two gaps match.
    #[test]
    fn button_row_is_vertically_centered() {
        let t = Theme::default().scaled(Dpi::new(96));
        let pad = t.metrics().padding;
        let spacing = t.metrics().spacing;
        let line = t.metrics().row_height;
        let width = t.metrics().settings_width;

        let checkbox = || Row::Checkbox {
            label: "Start minimised".into(),
            checked: false,
            depth: 0,
            id: HotspotId::new(200),
        };
        let button_row = || Row::Buttons {
            left: "OK".into(),
            left_id: HotspotId::new(108),
            right: "Cancel".into(),
            right_id: HotspotId::new(109),
        };

        // Ordinary case: a checkbox immediately precedes the buttons.
        let normal = vec![checkbox(), button_row()];
        // Warning case: the notice, wrapped in the same half-spacers
        // `SettingsDraft::rows` inserts, precedes the buttons. A `Notice` is
        // always one line, as the real dialog uses.
        let warned = vec![
            checkbox(),
            Row::Space(Gap::Half),
            Row::Notice {
                text: "Interval must be between 1000 and 60000 ms".into(),
                color: t.colors().critical,
            },
            Row::Space(Gap::Half),
            button_row(),
        ];

        for rows in [normal, warned] {
            // Walk to the button row's start, then place the box inside it.
            let mut header_seen = false;
            let mut y = pad;
            let mut row_start = None;
            for row in &rows {
                if matches!(row, Row::Buttons { .. }) {
                    row_start = Some(y);
                }
                y += row.height(&t, width, header_seen, &mut StubMetrics::default());
                if matches!(row, Row::Header(_)) {
                    header_seen = true;
                }
            }
            let row_start = row_start.expect("a button row must be present");
            let box_top = row_start + t.metrics().button_row_lead();
            let box_bottom = box_top + line + spacing;
            let height = measure_with(&rows, &t, width, &mut StubMetrics::default());

            // The last line of text ends `spacing / 2` above the button row in
            // either tail, and that is where the eye starts measuring.
            let above = box_top - (row_start - spacing / 2);
            let below = height - box_bottom;
            assert_eq!(
                above, below,
                "the gap above the buttons must equal the gap below them"
            );
            assert_eq!(
                below,
                pad + t.metrics().bottom_slack(),
                "the gap below the buttons is the window's bottom margin"
            );
        }
    }

    /// Every string drawn as a `Notice` must fit the line its own window gives
    /// it, in every language.
    ///
    /// A `Notice` is one line by contract and never wraps, so a translation
    /// wider than its line is clipped rather than flowed. Translators honour
    /// that by *contracting* the wording — the French and Italian interval
    /// warnings are shortened for exactly this reason — and until now nothing
    /// checked that they had. It could not be checked: a width in characters
    /// is not a width in pixels, and any per-character bound tight enough to
    /// be useful is not a bound at all. It is checkable now only because the
    /// layout can measure text.
    ///
    /// Measured against the width the window actually takes **for that
    /// language**, which is the width that rule is about: the room is generous
    /// because the checkbox labels and metric labels set the width, and the
    /// warning is contracted to fit whatever they set. Comparing against the
    /// theme's floor instead would fail on German — 333 px of warning against
    /// a 316 px floor — while the real German dialog is far wider than its
    /// floor, because German checkbox labels are long. That would be a test of
    /// a promise nobody made.
    #[test]
    fn every_notice_fits_one_line_in_every_language() {
        use crate::config::Config;
        use crate::lang::Locale;
        use crate::strings::Key;
        use crate::ui::settings::SettingsDraft;
        use crate::ui::theme::Builtin;

        const FLAGS: [Key; 7] = [
            Key::FlagLowBattery,
            Key::FlagInternalFailure,
            Key::FlagOverload,
            Key::FlagVoltageOutOfRange,
            Key::FlagFrequencyOutOfRange,
            Key::FlagRuntimeLimitExpired,
            Key::FlagBoost,
        ];

        let unscaled = Theme::default();
        let t = unscaled.scaled(Dpi::new(96));

        let themes = Builtin::ALL;
        let languages = Locale::available();

        with_metrics(
            Script {
                family: None,
                dpi: Dpi::new(96),
                points: crate::ui::DEFAULT_FONT_POINTS,
            },
            |m| {
                for language in languages {
                    let locale = Locale::by_code(language.code).value;

                    // The dialog as it stands with the warning showing: an
                    // interval outside the accepted range puts the `Notice` in
                    // the rows, and the rows are what set the width.
                    let mut draft = SettingsDraft::from_config(&Config::default());
                    draft.error = Some(locale.t1(Key::SettingsInvalidInterval, "1000"));
                    let rows = draft.rows(locale, &unscaled, &themes, languages);
                    let (needed, needs) = content_geometry(&rows, &[], &t, m);
                    let width = needed.max(t.metrics().settings_width);
                    let line = text_column_width(width, needs, &t);

                    for row in &rows {
                        if let Row::Notice { text, .. } = row {
                            let w = m.line_width(text, false);
                            assert!(
                                w <= line,
                                "{}: the interval warning is {w} px against a {line} px line; \
                             it must be contracted, not wrapped: {text:?}",
                                language.code
                            );
                        }
                    }

                    // The panel carries the fault flags, also as one-line
                    // notices. Their line is the panel's, which the metric
                    // labels set.
                    let panel_rows = panel_rows_with_every_flag(locale, &unscaled);
                    let (needed, needs) = content_geometry(&panel_rows, &[], &t, m);
                    let width = needed.max(t.metrics().panel_width);
                    let line = text_column_width(width, needs, &t);

                    for key in FLAGS {
                        let text = locale.t(key);
                        let w = m.line_width(text, false);
                        assert!(
                            w <= line,
                            "{}: the flag {:?} is {w} px against a {line} px line; a Notice \
                         does not wrap, so it would be clipped: {text:?}",
                            language.code,
                            key.label()
                        );
                    }
                }
            },
        );
    }

    /// The panel as it stands with every fault raised at once, which is when
    /// the flag notices are on screen and is the widest the Status block gets.
    fn panel_rows_with_every_flag(locale: crate::lang::Locale, t: &Theme) -> Vec<Row> {
        use crate::hid::{Beeper, Reading};
        use crate::ui::panel::{build, PanelData};

        let reading = Reading {
            ac_present: Some(true),
            below_capacity_limit: Some(true),
            internal_failure: Some(true),
            overload: Some(true),
            voltage_out_of_range: Some(true),
            frequency_out_of_range: Some(true),
            runtime_limit_expired: Some(true),
            boost: Some(true),
            // A mode that was read, so the buzzer row is the one with a
            // button rather than the reserved dash.
            beeper: Some(Beeper::Enabled),
            ..Default::default()
        };
        build(
            locale,
            t,
            &PanelData {
                reading: Some(&reading),
                presence: crate::app::Presence::Open,
                beeper: crate::app::BeeperView::Current(Beeper::Enabled),
                ..crate::testsupport::panel_data()
            },
        )
    }

    /// The size cache's key must see everything the size depends on.
    ///
    /// The fingerprint was written to key a *width* and later became the key of
    /// [`content_size`], which answers width and height together. Two things
    /// change the height without changing any column: the text of a `Wrapped`
    /// row, and which step a `Space` is. Left out of the key, a panel whose only
    /// variable row is a warning would swap one warning for another and be
    /// handed the previous one's height.
    #[test]
    fn the_size_fingerprint_sees_what_the_height_depends_on() {
        let unscaled = Theme::default();
        let t = unscaled.scaled(Dpi::new(96));

        let wrapped = |text: &str| {
            vec![Row::Wrapped {
                text: text.into(),
                color: t.colors().text_primary,
            }]
        };

        assert_ne!(
            text_fingerprint(&wrapped("Device not found."), &t),
            text_fingerprint(
                &wrapped(
                    "Several matching HID devices were found. The first is used, \
                     and the readings may belong to another power source."
                ),
                &t
            ),
            "a wrapped row's text is its height, so it must reach the key"
        );

        assert_ne!(
            text_fingerprint(&[Row::Space(Gap::Half)], &t),
            text_fingerprint(&[Row::Space(Gap::Single)], &t),
            "the two gap steps are different heights"
        );

        // The metrics reach the key, and a scaled theme is a different theme.
        //
        // One assertion is enough now, which it was not before: the key used to
        // list six metrics of nine by hand, so covering it meant naming every
        // field and hoping the next one was remembered. `Metrics` derives
        // `Hash` and is hashed whole, so "every field participates" is the
        // derive's guarantee and not something a test can usefully restate.
        // What is worth pinning is that the metrics are in the key at all.
        let rows = wrapped("Device not found.");
        assert_ne!(
            text_fingerprint(&rows, &t),
            text_fingerprint(&rows, &unscaled.scaled(Dpi::new(144))),
            "the same text at another DPI is another size; the key must say so"
        );

        // Colours, by contrast, must *not* reach it. They change what is drawn
        // and never what is measured, and invalidating a good measurement on a
        // palette change would re-measure every string in the window for
        // nothing.
        let mut repainted = unscaled;
        repainted.colors.text_primary = Color::from_rgb(0x12, 0x34, 0x56);
        let recoloured = repainted.scaled(Dpi::new(96));
        assert_eq!(
            text_fingerprint(&rows, &t),
            text_fingerprint(&rows, &recoloured),
            "a colour cannot change a measurement, so it must not evict one"
        );
    }

    /// The value column starts after the widest label, not at a fixed offset.
    ///
    /// This is the contract that makes a measured window work: shortening
    /// every label must move the values left and the right edge with them.
    /// Regression test for a panel that sized itself for the longest
    /// translation of any language, which left compact scripts — Chinese
    /// worst — with their values pushed against the labels and a band of
    /// empty background down the right side.
    #[test]
    fn narrow_labels_yield_a_narrow_window() {
        let t = Theme::default().scaled(Dpi::new(96));
        let long = vec![
            Row::Pair {
                label: "Umgebungstemperatur".into(),
                value: "220 V".into(),
                color: Color::from_rgb(255, 255, 255),
            },
            Row::Pair {
                label: "Eingangsspannung".into(),
                value: "230 V".into(),
                color: Color::from_rgb(255, 255, 255),
            },
        ];
        let short = vec![
            Row::Pair {
                label: "输入电压".into(),
                value: "220 V".into(),
                color: Color::from_rgb(255, 255, 255),
            },
            Row::Pair {
                label: "额定输入".into(),
                value: "230 V".into(),
                color: Color::from_rgb(255, 255, 255),
            },
        ];
        // Values are identical in both, so any difference is the label column.
        // A stand-in measurer: proportional to character count, which is the
        // shape of the real font's answer.
        let mut w = StubMetrics::default();
        let short_w = content_geometry_width(&short, &t, &mut w);
        let long_w = content_geometry_width(&long, &t, &mut w);
        assert!(
            short_w < long_w,
            "shorter labels must give a narrower window ({short_w} vs {long_w})"
        );
    }

    /// A dropdown is measured by its widest option, not its current value.
    ///
    /// The closed control must not be narrower than the list it opens, or the
    /// list would overhang the control it belongs to.
    #[test]
    fn a_dropdown_is_as_wide_as_its_longest_option() {
        let t = Theme::default().scaled(Dpi::new(96));
        let item = |s: &str| DropdownItem {
            label: s.into(),
            native: String::new(),
            font: None,
        };
        let mk = |opts: Vec<DropdownItem>| {
            vec![Row::Dropdown {
                label: "L".into(),
                options: opts,
                selected: 0,
                highlighted: 0,
                open: false,
                scroll: 0,
                id: HotspotId::new(1),
            }]
        };
        let narrow = mk(vec![item("A"), item("B")]);
        let wide = mk(vec![item("A"), item("A considerably longer option")]);
        let mut m = StubMetrics::default();
        assert!(
            content_geometry_width(&wide, &t, &mut m) > content_geometry_width(&narrow, &t, &mut m),
            "the unselected long option must still count"
        );
    }

    /// The painter's columns and the width measurement agree.
    ///
    /// They are two expressions of one layout rule, and if they drift the
    /// window is sized to something other than what is drawn into it —
    /// values clipped at the right edge, or a band of dead space. Both now
    /// call `column_needs`, so what this pins is the arithmetic around it:
    /// at the width the measurer returns, each column gets its half and the
    /// values still get every pixel they asked for.
    #[test]
    fn the_measured_width_leaves_room_for_both_columns() {
        let t = Theme::default().scaled(Dpi::new(96));
        let rows = vec![Row::Pair {
            label: "Label".into(),
            value: "A rather longer value".into(),
            color: Color::from_rgb(255, 255, 255),
        }];
        let mut m = StubMetrics::default();

        let width = content_geometry_width(&rows, &t, &mut m);

        let needs = column_needs(&rows, &t, &mut m);
        let (origin, half) = column_origin(width, needs.half, &t);

        let values = width - origin - half - origin;
        assert!(
            values >= m.line_width("A rather longer value", false),
            "values need {} px but only {values} remain",
            m.line_width("A rather longer value", false)
        );
        assert!(
            half >= m.line_width("Label", false) + t.metrics().column_gap,
            "the label column never shrinks below what the labels need"
        );
    }

    /// The buzzer's button is charged against its own value, not stacked on
    /// the widest value in the panel. The button width is the shared action
    /// width (here just this one button's caption), and the charge is
    /// `value + spacing + width`; the column takes whichever row needs the most.
    #[test]
    fn the_buzzer_button_is_charged_against_its_own_value() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();

        let spacing = t.metrics().spacing;
        let btn_min = t.metrics().button_min_width;

        // A long value on another row, a short one on the buzzer row.
        let rows = vec![
            Row::Pair {
                label: "Rated power".into(),
                value: "1350 VA / 810 W".into(),
                color: Color::from_rgb(255, 255, 255),
            },
            Row::LabeledButton {
                label: "Buzzer".into(),
                value: "Disabled".into(),
                button: "Enable".into(),
                color: Color::from_rgb(255, 255, 255),
                id: HotspotId::new(1),
                enabled: true,
            },
        ];

        let act_btn = action_button_width(&rows, btn_min, spacing, |s| m.line_width(s, false));
        let needs = column_needs(&rows, &t, &mut m);
        // The button is charged at the action width against its own short value.
        let own = m.line_width("Disabled", false) + spacing + act_btn;
        // Not stacked on the unrelated long nameplate value.
        let stacked = m.line_width("1350 VA / 810 W", false) + spacing + act_btn;

        assert!(
            needs.half <= own.max(m.line_width("Buzzer", false) + t.metrics().column_gap),
            "the column is wider than the buzzer row needs: {}",
            needs.half
        );
        assert!(
            needs.half < stacked,
            "the button must not be stacked on another row's value: {} vs {stacked}",
            needs.half
        );
    }

    /// The two value-column buttons — the buzzer and the self-test — share one
    /// width: the widest of the two captions. The shorter button is widened to
    /// match the longer, so they are never different sizes on adjacent rows.
    #[test]
    fn the_two_action_buttons_share_the_widest_caption() {
        let mut m = StubMetrics::default();
        let spacing = 8;
        let btn_min = 0; // isolate the caption-driven width from the floor

        let rows = vec![
            Row::LabeledButton {
                label: "Buzzer".into(),
                value: "Passed".into(),
                button: "Aç".into(), // short
                color: Color::from_rgb(255, 255, 255),
                id: HOT_BEEPER,
                enabled: true,
            },
            Row::LabeledButton {
                label: "Last test".into(),
                value: "Passed".into(),
                button: "Self-test".into(), // longer
                color: Color::from_rgb(255, 255, 255),
                id: HOT_SELFTEST,
                enabled: true,
            },
        ];

        let shared = action_button_width(&rows, btn_min, spacing, |s| m.line_width(s, false));
        // The width is driven by the wider caption, not the narrower one.
        assert_eq!(shared, m.line_width("Self-test", false) + spacing * 4);
        assert!(
            shared > m.line_width("Aç", false) + spacing * 4,
            "short button is widened to match"
        );
    }

    /// The buzzer button keeps one width while its verb alternates.
    ///
    /// The verb is the caption: pressing the button swaps "Enable" for
    /// "Disable", two words of different length. Measured over the rows being
    /// drawn, the shared action width was therefore a function of the state —
    /// the control resized under the pointer each time it was pressed, which
    /// is the same fault the samples already fixed for the window itself, one
    /// level in.
    ///
    /// Pinned through `content_geometry` rather than `action_button_width`,
    /// because the fault was never in the formula: it was in which rows the
    /// formula was applied to. The samples carry every caption the button can
    /// take, and it is the scan reaching them that this asserts.
    ///
    /// The self-test caption is deliberately the shortest of the three, so a
    /// scan of the drawn rows alone would produce two different answers rather
    /// than being masked by a long caption that dominates either way.
    #[test]
    fn the_action_button_keeps_one_width_across_its_captions() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();

        let buzzer = |verb: &str| Row::LabeledButton {
            label: "Buzzer".into(),
            value: "Disabled".into(),
            button: verb.into(),
            color: Color::from_rgb(255, 255, 255),
            id: HOT_BEEPER,
            enabled: true,
        };
        let self_test = || Row::LabeledButton {
            label: "Last test".into(),
            value: "Passed".into(),
            button: "Test".into(),
            color: Color::from_rgb(255, 255, 255),
            id: HOT_SELFTEST,
            enabled: true,
        };
        // Both verbs are long enough to clear `button_min_width`, so what is
        // compared below is the width the captions ask for rather than the
        // floor they would otherwise both be flattened to.
        let (on_verb, off_verb) = ("Enable buzzer", "Disable buzzer");
        // What the panel measures but never draws: both verbs, whichever one
        // the device's current mode puts on the button.
        let samples = vec![buzzer(on_verb), buzzer(off_verb), self_test()];

        let enabled = vec![buzzer(off_verb), self_test()];
        let disabled = vec![buzzer(on_verb), self_test()];

        let (_, on) = content_geometry(&enabled, &samples, &t, &mut m);
        let (_, off) = content_geometry(&disabled, &samples, &t, &mut m);

        assert_eq!(
            on.action_button, off.action_button,
            "the button resizes when its verb changes"
        );
        // And it is the width of the wider verb, which is the one *not* on the
        // button in the `enabled` state. Both terms are above the floor, so
        // this is an assertion about the captions and not about `btn_min`.
        let widest = m.line_width(off_verb, false) + t.metrics().spacing * 4;
        assert!(widest > t.metrics().button_min_width);
        assert_eq!(on.action_button, widest);
    }

    /// A panel with no action button falls back to the minimum width rather
    /// than a zero-width or panicking max over an empty set.
    #[test]
    fn action_width_without_buttons_is_the_minimum() {
        let rows = vec![Row::Pair {
            label: "Model".into(),
            value: "CP1350".into(),
            color: Color::from_rgb(255, 255, 255),
        }];
        let w = action_button_width(&rows, 96, 8, |s| s.chars().count() as i32 * 7);
        assert_eq!(w, 96);
    }

    /// The value column starts exactly on the window's centre line.
    ///
    /// This is the whole point of the halves rule, and it has to hold for a
    /// script with short labels and a script with long ones alike: the
    /// panel drifted left of centre precisely because each column was
    /// charged only what it happened to need, and the label column is much
    /// narrower in Chinese than in German.
    #[test]
    fn the_value_column_starts_at_the_centre_line() {
        let t = Theme::default().scaled(Dpi::new(96));

        // CJK counted double, which is the shape of the real font's answer.
        let mut m = StubMetrics::wide_aware();
        let rows = |label: &str| {
            vec![Row::Pair {
                label: label.into(),
                value: "1350 VA / 810 W".into(),
                color: Color::from_rgb(255, 255, 255),
            }]
        };
        for label in ["输入电压", "Low-charge threshold", "Eingangsspannung"] {
            let r = rows(label);
            let width = content_geometry_width(&r, &t, &mut m);
            let needs = column_needs(&r, &t, &mut m);
            let (origin, half) = column_origin(width, needs.half, &t);
            let centre = width / 2;
            let value_x = origin + half;
            assert!(
                (value_x - centre).abs() <= 1,
                "{label}: values start at {value_x}, centre is {centre}"
            );
        }
    }

    /// Slack from a full-width row is split between the margins, not left
    /// to pile up on the right.
    ///
    /// A title row wider than the two columns need is what produced the
    /// band of dead background beside the values: the window grew, the
    /// columns did not move, and the whole block sat left of centre.
    #[test]
    fn a_wide_single_row_keeps_the_columns_centred() {
        let t = Theme::default().scaled(Dpi::new(96));

        let pad = t.metrics().padding;
        // Columns need little; the window is much wider than that.
        let half_need = 40;
        let width = pad * 2 + 400;
        let (origin, half) = column_origin(width, half_need, &t);
        assert_eq!(origin + half, width / 2, "values must begin at the centre");
        assert_eq!(origin, width - (origin + half * 2), "margins must be equal");
    }

    /// A window with short labels is still narrower than one with long
    /// labels.
    ///
    /// The halves rule costs width — both columns are charged the larger of
    /// the two — and the risk it carries is that every language arrives at
    /// the same number, which is the fault the per-language measurement
    /// exists to prevent. The label column still varies by language, so the
    /// window still does.
    #[test]
    fn the_halves_rule_does_not_flatten_the_per_language_width() {
        let t = Theme::default().scaled(Dpi::new(96));
        // CJK counted double, which is the shape of the real font's answer.
        let mut m = StubMetrics::wide_aware();
        let pair = |label: &str| {
            vec![Row::Pair {
                label: label.into(),
                value: "1350 VA / 810 W".into(),
                color: Color::from_rgb(255, 255, 255),
            }]
        };
        let short = content_geometry_width(&pair("输入电压"), &t, &mut m);
        let long = content_geometry_width(&pair("Napięcie wejściowe znamionowe"), &t, &mut m);
        assert!(
            short < long,
            "short labels must still give a narrower window ({short} vs {long})"
        );
    }

    #[test]
    fn measure_grows_with_rows() {
        let t = Theme::default().scaled(Dpi::new(96));
        let empty = measure_with(&[], &t, 460, &mut StubMetrics::default());
        let one = measure_with(
            &[Row::Pair {
                label: "a".into(),
                value: "b".into(),
                color: Color::from_rgb(255, 255, 255),
            }],
            &t,
            460,
            &mut StubMetrics::default(),
        );
        assert!(one > empty, "adding a row must increase height");
    }

    /// Text that genuinely wraps reserves more than the single line a `Pair`
    /// does — and text that does not wrap reserves exactly as much.
    ///
    /// The second half is the part that changed. The height follows the text
    /// now, so "more than one line" is a property of a long message rather
    /// than of the variant.
    #[test]
    fn measure_counts_wrapped_by_the_lines_it_needs() {
        let t = Theme::default().scaled(Dpi::new(96));
        let pair = measure_with(
            &[Row::Pair {
                label: "a".into(),
                value: "b".into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            460,
            &mut StubMetrics::default(),
        );
        let long = measure_with(
            &[Row::Wrapped {
                text: "A sentence long enough that it cannot possibly fit on \
                       one line of a window this narrow, and so must wrap onto \
                       a second and a third."
                    .into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            460,
            &mut StubMetrics::default(),
        );
        assert!(
            long > pair,
            "a message that wraps reserves more than one line"
        );

        let short = measure_with(
            &[Row::Wrapped {
                text: "Short".into(),
                color: Color::TRANSPARENT,
            }],
            &t,
            460,
            &mut StubMetrics::default(),
        );
        assert_eq!(short, pair, "a message that does not wrap is one line tall");
    }

    /// `measure_with` and the painter now read one height table —
    /// `Row::height` — so they cannot drift. This checks the sum
    /// `measure_with` builds is exactly the per-row heights plus the fixed
    /// padding, which is the contract the painter also advances by: if
    /// `measure_with` ever grew its own arithmetic again, this would catch the
    /// divergence.
    #[test]
    fn measure_is_the_sum_of_row_heights() {
        let t = Theme::default().scaled(Dpi::new(96));
        let rows = [
            Row::Header("Input".into()),
            Row::Pair {
                label: "a".into(),
                value: "b".into(),
                color: Color::TRANSPARENT,
            },
            Row::Header("Output".into()),
            Row::Pair {
                label: "c".into(),
                value: "d".into(),
                color: Color::TRANSPARENT,
            },
        ];
        let width = 460;

        let pad = t.metrics().padding;
        let mut expected = pad * 2 + t.metrics().bottom_slack();
        let mut seen = false;
        for row in &rows {
            expected += row.height(&t, width, seen, &mut StubMetrics::default());
            if matches!(row, Row::Header(_)) {
                seen = true;
            }
        }

        assert_eq!(
            measure_with(&rows, &t, width, &mut StubMetrics::default()),
            expected
        );
    }

    /// A pair row with a short label and a short value, plus the label under
    /// test. Everything else in these three tests is held constant so the one
    /// number that moves is the one being asserted.
    fn pair(label: &str, value: &str) -> Row {
        Row::Pair {
            label: label.into(),
            value: value.into(),
            color: Color::TRANSPARENT,
        }
    }

    /// A list of `count` options, the first of which is `long`.
    ///
    /// The native name is filled in, which is what makes the option
    /// distinguishable from its label: a measurer asked for the label alone
    /// returns a different number from one asked for the whole option, and
    /// that difference is the assertion in
    /// `a_dropdown_is_charged_for_the_whole_option`.
    fn language_list(long: &str, count: usize) -> Row {
        let mut options = vec![DropdownItem {
            label: long.into(),
            native: "native name".into(),
            font: None,
        }];
        options.extend((1..count).map(|i| DropdownItem {
            label: format!("o{i}"),
            native: String::new(),
            font: None,
        }));
        Row::Dropdown {
            label: "L".into(),
            options,
            selected: 0,
            highlighted: 0,
            open: false,
            scroll: 0,
            id: HotspotId::new(1),
        }
    }

    /// The label column is charged for the longest label, not the last one or
    /// the first one.
    ///
    /// Pinned by value rather than by comparison, because the failure this
    /// guards against is arithmetic: `column_needs` folds every label into one
    /// number with `max`, and `min` in its place yields a column sized to the
    /// *shortest* label — which compiles, measures, and clips every other row's
    /// text at the gap.
    #[test]
    fn the_label_column_takes_the_longest_label() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();
        let rows = vec![
            pair("short", "v"),
            pair("the longest label here", "v"),
            pair("mid label", "v"),
        ];

        let needs = column_needs(&rows, &t, &mut m);

        // 22 characters at 7 px, plus the gap the labels may not encroach on.
        assert_eq!(needs.half, 22 * 7 + t.metrics().column_gap);
        assert_eq!(needs.wide_single, 0, "no full-width row is present");
    }

    /// The value column is charged for the widest value, and a value wider
    /// than the labels wins the half outright.
    #[test]
    fn the_value_column_takes_the_widest_value() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();
        let rows = vec![
            pair("L", "narrow"),
            pair("L", "a decidedly wider value"),
            pair("L", "middling"),
        ];

        let needs = column_needs(&rows, &t, &mut m);

        // 23 characters at 7 px. The label half — one character plus the gap —
        // is smaller, so this is what the half comes out as.
        assert_eq!(needs.half, 23 * 7);
    }

    /// A dropdown is charged for the whole option — label *and* native name —
    /// not for the label alone.
    ///
    /// The two are separate methods on [`TextMetrics`] because an option is set
    /// in two fonts, and measuring one through the other is the defect that
    /// method split fixed: the column came back in the interface font and the
    /// text was then painted in another. Here the option carries a native name
    /// the label does not, so the two answers differ by construction and the
    /// assertion can tell which one was asked for.
    #[test]
    fn a_dropdown_is_charged_for_the_whole_option() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();
        let spacing = t.metrics().spacing;
        let line = t.metrics().row_height;
        let rows = vec![language_list("English", 3)];

        let needs = column_needs(&rows, &t, &mut m);

        // "English" + " ()" + "native name" at 7 px each, then the box
        // padding and the disclosure arrow. Three options fit, so no scrollbar.
        let option = (7 + 3 + 11) * 7;
        assert_eq!(needs.half, option + spacing * 2 + line);
    }

    /// The scrollbar is charged only when the list is long enough to draw one.
    ///
    /// Both directions in one test, because either half alone passes on a
    /// function that has lost the condition: charging always makes the short
    /// list wrong, charging never makes the long list wrong, and only the pair
    /// says the term is conditional. The difference is exactly one bar.
    #[test]
    fn only_a_scrolling_list_is_charged_for_its_scrollbar() {
        let t = Theme::default().scaled(Dpi::new(96));
        let mut m = StubMetrics::default();
        let line = t.metrics().row_height;

        let fits = column_needs(
            &[language_list("English", DROPDOWN_MAX_VISIBLE)],
            &t,
            &mut m,
        );
        let scrolls = column_needs(
            &[language_list("English", DROPDOWN_MAX_VISIBLE + 1)],
            &t,
            &mut m,
        );

        assert_eq!(scrolls.half - fits.half, scrollbar_width(line));
    }
}
