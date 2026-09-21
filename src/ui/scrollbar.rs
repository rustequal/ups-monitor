//! The scrollbar of an open dropdown: where its parts are, and what a click
//! or a drag on them means.
//!
//! Split out of [`window`](super::window) because it is geometry, not
//! painting. [`ScrollbarGeometry::layout`] answers "where is each part" and
//! [`ScrollbarGeometry::scroll_at`] answers the inverse, "which offset puts
//! the thumb here" — and those two have to stay exact inverses of each other
//! or a drag runs out of list before the pointer runs out of track. Keeping
//! them beside each other, away from six thousand lines of window procedure,
//! is what makes that pairing visible.
//!
//! The drawing at the end of the file is the one exception, and a deliberate
//! one: a stepper arrow is drawn from the rectangle the geometry above
//! produced, proportioned by the same constants.

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::Polygon;

use crate::color::Color;
use crate::ui::gdi::{Brush, Canvas, Pen, Selection};
use crate::ui::rect::Rect;
use crate::ui::row::HotspotId;
/// Hotspot ids for the parts of an open list's scrollbar.
///
/// In the `9_9xx` band with `HotspotId::NOTHING`: above every real control id, below
/// the option band at `10_000`, and therefore unable to collide with either.
/// Only one list is open at a time, so these do not need to name which one —
/// the owner already knows, because it is the list it opened.
pub(crate) const HOT_SCROLL_UP: HotspotId = HotspotId::new(9_990);
pub(crate) const HOT_SCROLL_DOWN: HotspotId = HotspotId::new(9_991);
/// The track above the thumb: a page back, like every platform scrollbar.
pub(crate) const HOT_SCROLL_PAGE_UP: HotspotId = HotspotId::new(9_992);
/// The track below the thumb: a page forward.
pub(crate) const HOT_SCROLL_PAGE_DOWN: HotspotId = HotspotId::new(9_993);
/// The thumb. Clicking it starts a drag rather than scrolling by itself.
pub(crate) const HOT_SCROLL_THUMB: HotspotId = HotspotId::new(9_994);

/// Most options shown at once before the list scrolls.
///
/// A cap rather than a computed fit: with 24 languages an uncapped list is
/// taller than the dialog behind it, and a list that runs off the screen
/// bottom cannot be reached with the mouse. Twelve fills a comfortable
/// column and leaves the list obviously scrollable rather than obviously
/// truncated.
pub(crate) const DROPDOWN_MAX_VISIBLE: usize = 12;

/// The largest scroll offset that still fills the window of an open list.
///
/// The inverse of [`ListWindow::of`], asked by the wheel, the scrollbar and
/// the keyboard, and written as the one subtraction both sides share so they
/// cannot drift apart. `saturating_sub` is the honest spelling: a list that
/// fits entirely has nowhere to scroll, which is an offset of zero, and a
/// plain subtraction would wrap in release rather than fail.
pub(crate) const fn last_scroll_offset(len: usize) -> usize {
    len.saturating_sub(DROPDOWN_MAX_VISIBLE)
}

/// Which options of an open list are on screen, at a given scroll offset.
///
/// The answer used to be computed in four places — the painter, the frame
/// record beside it, `scroll_list` and `list_span` — out of the same two
/// ingredients each time, and the painter then walked its result with an index
/// that three lines had to agree about. Four spellings of one number is the
/// shape this project treats as a defect rather than as thoroughness: they
/// cannot be kept equal, only found unequal.
///
/// The window is a slice, not a pair of indices, so the painter's loop has no
/// index to get wrong and needs no `else { break }` for the case where the
/// list turned out shorter than the frame drawn for it. That case is now
/// unrepresentable rather than handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListWindow<'a, T> {
    /// Index in the full list of the first option shown. This is `scroll`
    /// clamped into range: scrolling past the end shows the last page rather
    /// than a short one or an empty one.
    pub(crate) first: usize,
    /// The options that fit, in order. Empty only when the list is.
    pub(crate) shown: &'a [T],
}

impl<'a, T> ListWindow<'a, T> {
    /// The window `scroll` puts over `options`.
    ///
    /// Total by construction and by spelling: `first` is clamped against
    /// [`last_scroll_offset`], and both slices are taken with `get`, so
    /// nothing here can index out of range. The fallback of the second `get`
    /// is the ordinary case rather than a guard — a list shorter than the
    /// window keeps all of it.
    pub(crate) fn of(options: &'a [T], scroll: usize) -> Self {
        let first = scroll.min(last_scroll_offset(options.len()));
        let tail = options.get(first..).unwrap_or_default();
        Self {
            first,
            shown: tail.get(..DROPDOWN_MAX_VISIBLE).unwrap_or(tail),
        }
    }
}

/// Width of the scrollbar drawn inside the list, in pixels at the reference
/// `row_height` of 22. Scaled with `row_height` at paint time like every
/// other metric.
///
/// Wide enough to click, which the previous 4px mark was not. It was drawn as
/// an indicator only — position and extent, no hit target — so the wheel was
/// the sole way to move a twenty-four language list. That excludes anyone
/// without a wheel, and it is not what any other list on the platform does.
/// Sixteen at the reference height is the same order as the system scrollbar
/// (`SM_CXVSCROLL`, 17 at 96 DPI), so the control lands where the hand
/// expects it.
pub(super) const DROPDOWN_SCROLLBAR: i32 = 16;

/// How far one page moves: a screenful less a line.
///
/// The overlapping line stays on screen and carries the eye across the jump,
/// which is what every platform scrollbar does. Named once because two things
/// page: the scrollbar's track, which moves the view, and Page Up / Page Down,
/// which move the highlight. Two literals spelling the same rule would be two
/// rules the day one of them was tuned.
pub(crate) const DROPDOWN_PAGE: usize = DROPDOWN_MAX_VISIBLE - 1;

/// Reference `row_height` the unscaled scrollbar metrics are quoted against.
pub(super) const SCROLLBAR_REF_ROW: i32 = 22;

/// Shortest the thumb is allowed to get, in pixels at the reference height.
///
/// A proportional thumb over a long list shrinks toward nothing, and a mark
/// two pixels tall cannot be grabbed. Below this floor the thumb stops
/// shrinking and only its travel keeps encoding position — the same
/// compromise every platform scrollbar makes.
pub(super) const SCROLLBAR_MIN_THUMB: i32 = 18;

/// The parts of a dropdown's scrollbar, in window coordinates.
///
/// One struct produced by one function because three separate readers need
/// the identical geometry: the painter, the hit test that turns a click into
/// a line step or a page jump, and the drag that maps pointer movement back
/// onto a scroll offset. Computed independently in three places, they agree
/// until someone adjusts a `+ 1` in one of them, and the symptom is a thumb
/// that renders half a pixel from where it can be grabbed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ScrollbarGeometry {
    /// Full bar, buttons included.
    pub(super) bar: Rect,
    /// The step-up button at the top.
    pub(super) up: Rect,
    /// The step-down button at the bottom.
    pub(super) down: Rect,
    /// The area the thumb travels in, between the two buttons.
    pub(super) track: Rect,
    /// The thumb itself, at the current scroll offset.
    pub(super) thumb: Rect,
}

impl ScrollbarGeometry {
    /// Lays out the scrollbar for a list of `total` options showing `visible`
    /// of them at offset `scroll`, inside `frame`.
    ///
    /// Returns `None` when everything fits: a scrollbar over a list with
    /// nothing to scroll is a control that cannot do anything, and it would
    /// also steal width from the options for no purpose.
    ///
    /// Pure arithmetic over plain integers so the geometry can be tested
    /// without a window — which is the only way the invariants below get
    /// checked at all, since the painter needs a live device context.
    pub(super) fn layout(
        frame: Rect,
        width: i32,
        total: usize,
        visible: usize,
        scroll: usize,
    ) -> Option<Self> {
        if total <= visible || visible == 0 {
            return None;
        }
        // Normalised here rather than trusted from the caller.
        //
        // The old comment said "`scroll` is clamped by the caller" and left it
        // at that. But the geometry is computed from one set of numbers and
        // `scroll` arrives from another — the desynchronisation the notes on
        // `PaintedList` already describe — and an offset past the end put the
        // thumb's top below the bottom of the track, drawn outside the bar it
        // belongs to. This function is declared as pure arithmetic testable
        // without a window; a function like that owes itself its own
        // preconditions instead of holding its callers to an agreement.
        //
        // `total > visible` is guaranteed by the guard above, so the span is
        // at least one and the clamp is well defined.
        let span = total - visible;
        let scroll = scroll.min(span);

        // Inside the border on all four sides, like the option rows.
        let bar = Rect {
            left: frame.right - 1 - width,
            top: frame.top + 1,
            right: frame.right - 1,
            bottom: frame.bottom - 1,
        };
        // Square buttons, and identical to each other.
        //
        // Derived from the bar's *actual* width rather than from the `width`
        // argument: the two differ once the frame has been rounded, and using
        // the argument gave a button whose height did not match its own drawn
        // width — visibly a squat rectangle rather than a square. Both
        // buttons take this one value, so they cannot come out different
        // sizes from one another either.
        let bar_w = bar.right - bar.left;
        let bar_h = bar.bottom - bar.top;
        // Capped at a third of the bar each, or on a short list the two would
        // meet in the middle and leave no track between them.
        let button = bar_w.min(bar_h / 3).max(1);
        let up = Rect {
            bottom: bar.top + button,
            ..bar
        };
        let down = Rect {
            top: bar.bottom - button,
            ..bar
        };
        let track = Rect {
            top: up.bottom,
            bottom: down.top,
            ..bar
        };

        let track_h = (track.bottom - track.top).max(1);
        let min_thumb = (SCROLLBAR_MIN_THUMB * width / DROPDOWN_SCROLLBAR).clamp(1, track_h);
        let thumb_h = (track_h * visible as i32 / total as i32).clamp(min_thumb, track_h);
        let travel = track_h - thumb_h;
        let top = track.top + travel * scroll as i32 / span as i32;
        let thumb = Rect {
            top,
            bottom: top + thumb_h,
            ..track
        };

        Some(Self {
            bar,
            up,
            down,
            track,
            thumb,
        })
    }

    /// The scroll offset that puts the thumb's *top* at `y`.
    ///
    /// The inverse of the placement in `layout`, and it has to be exactly
    /// that: a drag computed against the track's full height rather than the
    /// thumb's travel runs out of list before the pointer runs out of track,
    /// so the last few options are unreachable by dragging even though the
    /// thumb still has somewhere to go.
    ///
    /// `span` is how far the list can scroll — the count of options that do
    /// not fit — and is taken directly rather than derived here from a total
    /// and a visible count. Derived, it was `total - visible`: a `usize`
    /// subtraction that the release profile wraps in silence, giving a
    /// negative `span` and a `clamp(0, span)` with `min > max` on the next
    /// line, which panics inside the standard library's `clamp`. The trace led
    /// to `cmp.rs` and named nothing about the pair that produced it, and the
    /// panic sat under `WM_MOUSEMOVE` in an `extern "system"` window
    /// procedure, where `panic = "abort"` makes it the end of the process. The
    /// two numbers were never both needed here; only their difference was, and
    /// a difference cannot be inconsistent with itself.
    pub(super) fn scroll_at(&self, y: i32, span: usize) -> usize {
        let track_h = (self.track.bottom - self.track.top).max(1);
        let thumb_h = self.thumb.bottom - self.thumb.top;
        let travel = (track_h - thumb_h).max(1);
        let span = span as i32;
        let offset = (y - self.track.top).clamp(0, travel);
        // Rounded, not truncated: at the bottom of the travel, truncation
        // leaves the last option one step out of reach.
        (((offset * span) + travel / 2) / travel).clamp(0, span) as usize
    }
}

/// Width of a dropdown's scrollbar at the given row height, or zero when a
/// list of `total` options does not need one.
///
/// One function for the painter and the measurer, because the two must agree
/// exactly: the bar is drawn inside the list frame, so a window measured
/// without room for a bar the painter then draws loses the right-hand
/// characters of its longest option. Expressed as a predicate over `total`
/// rather than as two separate comparisons — `total > visible` in one place
/// and `options.len() > MAX` in the other — which are equal only because
/// `visible` happens to be `total.min(MAX)`, an equivalence that holds today
/// and is nowhere written down.
///
/// Zero when everything fits. A list that does not scroll shows no scrollbar
/// at all: an inert control taking width from the options is worse than no
/// control, and the log-level list — two options — must not widen the dialog
/// by a bar it will never draw.
pub(super) fn scrollbar_width_for(total: usize, row_height: i32) -> i32 {
    if total > DROPDOWN_MAX_VISIBLE {
        scrollbar_width(row_height)
    } else {
        0
    }
}

/// Width of a dropdown's scrollbar at the given row height.
///
/// Sized from `row_height` rather than from `SM_CXVSCROLL` so it scales with
/// the theme like every other metric here, and so the measurement stays
/// testable off Windows.
pub(super) fn scrollbar_width(row_height: i32) -> i32 {
    (DROPDOWN_SCROLLBAR * row_height / SCROLLBAR_REF_ROW).max(8)
}

/// Where an open list's parts land, decided from numbers alone.
///
/// The painter used to work this out inline, between the fill calls, and
/// therefore only ever under a live device context — which is why none of it
/// was under test. Every decision here is arithmetic over a rectangle and two
/// counts: how tall the list is, whether it hangs below its control or above
/// it, which pixels each option row occupies, and how far the invalidation
/// rectangle reaches past the border. The painter now asks and draws.
///
/// Sits beside [`ScrollbarGeometry`] rather than in a file of its own because
/// it is the same subject: where the parts of an open dropdown are. Splitting
/// "where the frame is" from "where the bar inside it is" across two modules
/// would put one layout in two places, and the bar is positioned from the
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ListLayout {
    /// The list's border and everything inside it.
    pub(super) frame: Rect,
    /// Width of the scrollbar drawn inside the frame, zero when the list
    /// fits. Charged out of the option rows, because that is where it is
    /// drawn.
    pub(super) bar: i32,
    /// Height of one option row, kept so [`Self::option`] does not need it
    /// passed back in.
    item_h: i32,
}

impl ListLayout {
    /// Places a list of `visible` rows under — or over — the control at
    /// `anchor`, inside a window `window_height` tall.
    ///
    /// `total` is the whole list and `visible` the part that fits; the two
    /// differ exactly when there is a scrollbar to charge for.
    pub(super) fn of(
        anchor: Rect,
        window_height: i32,
        item_h: i32,
        row_height: i32,
        total: usize,
        visible: usize,
    ) -> Self {
        // One pixel of border top and bottom, outside the rows.
        let h = item_h * visible as i32 + 2;
        // Below the control if it fits, otherwise above it. Preferring
        // downward matches every other list on the platform; flipping only
        // when it would not fit keeps the common case predictable. A list
        // taller than the window itself is pinned to the top rather than
        // allowed to start off-screen, where its first options would be
        // unreachable.
        let top = if anchor.bottom + h <= window_height {
            anchor.bottom
        } else {
            (anchor.top - h).max(0)
        };
        Self {
            frame: Rect {
                top,
                bottom: top + h,
                ..anchor
            },
            bar: scrollbar_width_for(total, row_height),
            item_h,
        }
    }

    /// The rectangle of the `slot`-th row on screen, counting from the top of
    /// the list rather than from the top of the options.
    ///
    /// Inside the border on every side, and short of the scrollbar on the
    /// right: the bar is drawn over this edge, so a row drawn its full width
    /// would have its last characters covered.
    pub(super) fn option(&self, slot: usize) -> Rect {
        let slot = slot as i32;
        Rect {
            left: self.frame.left + 1,
            top: self.frame.top + 1 + self.item_h * slot,
            right: self.frame.right - 1 - self.bar,
            bottom: self.frame.top + 1 + self.item_h * (slot + 1),
        }
    }

    /// The area a scroll has to invalidate to erase this list.
    ///
    /// A pixel of slack on each side, which covers the anti-aliased corners of
    /// the rounded border — without it a faint outline is left behind where
    /// the list used to be.
    pub(super) fn bounds(&self) -> Rect {
        Rect {
            left: self.frame.left - 1,
            top: self.frame.top - 1,
            right: self.frame.right + 1,
            bottom: self.frame.bottom + 1,
        }
    }
}

/// What one visible option is, to the eye.
///
/// Three states and not two, deliberately: the highlight is where the keyboard
/// is, the selection is what is currently in effect, and an ordinary row is
/// neither. Collapsing the first two would make arrowing through the list look
/// as though it had already changed the setting, which it does not do until
/// Enter.
///
/// The palette is the painter's — this says only which of the three a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OptionRole {
    /// Where the keyboard is.
    Highlighted,
    /// What the setting currently holds.
    Selected,
    /// Neither.
    Plain,
}

impl OptionRole {
    /// Which of the three the option at `index` is.
    ///
    /// The highlight wins when the two coincide, and that ordering is the
    /// whole of the rule: on a freshly opened list they *do* coincide — the
    /// list opens with the highlight on the current choice — so the other
    /// order would draw the accent as a foreground colour on the background
    /// fill, and an open list would show no highlight at all until the first
    /// arrow key.
    pub(super) fn of(index: usize, selected: usize, highlighted: usize) -> Self {
        if index == highlighted {
            Self::Highlighted
        } else if index == selected {
            Self::Selected
        } else {
            Self::Plain
        }
    }
}

/// Draws a stepper triangle centred in `rect`.
///
/// Distinct from `draw_arrow` and named for the difference rather than
/// sharing a name with it: that one places its triangle against the *right
/// edge* of a box it is given, because it marks a dropdown whose text
/// occupies the rest of the box. This one centres in both axes, because a
/// stepper button contains nothing else. Two contracts under one name is
/// exactly the trap `wide.rs` was split up to avoid — the call site cannot
/// see which placement it is getting.
pub(super) fn draw_stepper(canvas: Canvas<'_>, rect: &Rect, up: bool, color: Color) {
    let points = stepper_points(rect, up).map(|(x, y)| POINT { x, y });
    let (Some(brush), Some(pen)) = (Brush::solid(color), Pen::hairline(color)) else {
        return;
    };
    let _fill = Selection::new(canvas, &brush);
    let _outline = Selection::new(canvas, &pen);
    // SAFETY: `canvas` proves the context is live, the brush and pen are
    // selected by the guards above and outlive the call, and `points` is a
    // slice `Polygon` reads for its own length.
    let _ = unsafe { Polygon(canvas.raw(), &points) };
}

/// The three corners of a stepper triangle centred in `rect`, apex first.
///
/// Split from the drawing so the proportions can be asked about without a
/// device context. They are worth asking about: this glyph was once exactly
/// twice as wide as it was tall, a flattened wedge rather than an arrowhead,
/// and nothing about a `Polygon` call says what shape reached it.
///
/// Plain `(x, y)` pairs rather than a point type. A type would be used at this
/// one call and read back in one test, and a name that appears twice is a
/// rename, not an abstraction — the same reason this project keeps rejecting
/// wrappers around single calls. `POINT` itself is not returned because the
/// arithmetic here is this program's own geometry, which is the argument
/// [`Rect`] was introduced on.
fn stepper_points(rect: &Rect, up: bool) -> [(i32, i32); 3] {
    // Half-width of the base. The triangle is sized off the smaller side so
    // it stays inside the button whatever the button's shape.
    let half_w = (rect.width().min(rect.height()) / 4).max(2);
    // Height of the triangle, close to its full base width rather than to
    // half of it.
    //
    // The base spans `half_w * 2`, so pairing it with a height of `half_w`
    // made the glyph exactly twice as wide as it was tall — a flattened
    // wedge rather than an arrowhead. Roughly equilateral is what every
    // platform scrollbar draws and what the eye reads as a stepper.
    let height = (half_w * 2 - half_w / 3).max(3);
    let cx = (rect.left + rect.right) / 2;
    let cy = (rect.top + rect.bottom) / 2;
    // Apex toward the direction the button steps, base on the far side, and
    // the whole glyph centred on the button's middle. The two terms of the
    // height are split unevenly on purpose: an odd height cannot be halved
    // twice, and giving the remainder to the base keeps the glyph's centre of
    // area on the button's centre rather than a pixel above it.
    let half_h = height / 2;
    let (apex, base) = if up {
        (cy - half_h, cy + (height - half_h))
    } else {
        (cy + half_h, cy - (height - half_h))
    };
    [(cx, apex), (cx - half_w, base), (cx + half_w, base)]
}

#[cfg(test)]
mod tests {
    //! Tests for the scrollbar geometry.
    //!
    //! `frame()` is the fixture every one of them starts from: a plausible
    //! list frame in pixels. The properties under test are all relations
    //! between the parts `layout` produces — they tile the bar, the thumb
    //! reaches both ends, and `scroll_at` inverts the placement exactly — so
    //! they are checked against one frame rather than many.

    /// The window over a list, by value at every offset that matters.
    ///
    /// Three claims at once, and each is a way the old four-way arithmetic
    /// could have been wrong: a list shorter than the cap shows all of it and
    /// starts at zero; a longer one shows exactly the cap; and an offset past
    /// the end is pulled back to the last full page rather than yielding a
    /// short page or an empty one. The offsets are asserted, not just the
    /// lengths, because starting at the wrong option is the failure that looks
    /// like a working list.
    #[test]
    fn the_window_over_a_list_is_pinned_by_value() {
        let short: Vec<usize> = (0..5).collect();
        let w = ListWindow::of(&short, 0);
        assert_eq!(w.first, 0);
        assert_eq!(w.shown, &short[..]);
        // Nowhere to scroll: the whole list already fits.
        assert_eq!(ListWindow::of(&short, 4).first, 0);
        assert_eq!(last_scroll_offset(short.len()), 0);

        let long: Vec<usize> = (0..DROPDOWN_MAX_VISIBLE + 3).collect();
        let w = ListWindow::of(&long, 0);
        assert_eq!(w.first, 0);
        assert_eq!(w.shown.len(), DROPDOWN_MAX_VISIBLE);
        assert_eq!(w.shown.first(), Some(&0));

        let w = ListWindow::of(&long, 2);
        assert_eq!(w.first, 2);
        assert_eq!(w.shown.first(), Some(&2));
        assert_eq!(w.shown.len(), DROPDOWN_MAX_VISIBLE);

        // Past the end: the last full page, not a short one.
        assert_eq!(last_scroll_offset(long.len()), 3);
        let w = ListWindow::of(&long, 99);
        assert_eq!(w.first, 3);
        assert_eq!(w.shown.len(), DROPDOWN_MAX_VISIBLE);
        assert_eq!(w.shown.last(), long.last());

        // An empty list has an empty window and no offset to be wrong about.
        let none: Vec<usize> = Vec::new();
        let w = ListWindow::of(&none, 7);
        assert_eq!(w.first, 0);
        assert!(w.shown.is_empty());
    }

    use super::*;

    /// The list frame the geometry tests lay a scrollbar into.
    fn frame() -> Rect {
        Rect {
            left: 100,
            top: 50,
            right: 400,
            bottom: 50 + 12 * 26 + 2,
        }
    }

    /// Where the list hangs, and where its rows fall inside it.
    ///
    /// The frame arithmetic was written between the fill calls of the painter,
    /// so it could only ever run under a live device context and nothing
    /// asked about it. It decides three things a user sees: how tall the list
    /// is, whether it drops below its control or flips above it, and which
    /// pixels each row owns.
    #[test]
    fn an_open_list_hangs_below_its_control_and_tiles_its_rows() {
        // A control 24 px tall at y = 100, in a window with room below it.
        let anchor = Rect::new(40, 100, 240, 124);
        let l = ListLayout::of(anchor, 600, 20, 22, 3, 3);

        // Two rows of 20 plus a pixel of border top and bottom, hung off the
        // bottom edge of the control and keeping its two sides.
        assert_eq!(l.frame, Rect::new(40, 124, 240, 124 + 3 * 20 + 2));
        // Three options fit, so no bar is charged and the rows run to the
        // inside of the right border.
        assert_eq!(l.bar, 0);
        assert_eq!(l.option(0), Rect::new(41, 125, 239, 145));
        assert_eq!(l.option(1), Rect::new(41, 145, 239, 165));
        // Rows tile: each begins exactly where the one above it ended.
        assert_eq!(l.option(2).top, l.option(1).bottom);
        // The invalidation area reaches a pixel past the border on every side,
        // which is where the rounded corners are anti-aliased.
        assert_eq!(l.bounds(), Rect::new(39, 123, 241, l.frame.bottom + 1));
    }

    /// A list with no room below its control opens upwards instead.
    ///
    /// Preferring downwards is what every other list on the platform does, so
    /// flipping has to be conditional rather than clever: a list that always
    /// opened upwards would be as wrong as one that never did, and both look
    /// correct in the case the developer happens to try.
    #[test]
    fn a_list_with_no_room_below_opens_upwards() {
        let anchor = Rect::new(40, 500, 240, 524);
        // Twelve rows of 20 need 242 px; only 76 remain below the control.
        let l = ListLayout::of(anchor, 600, 20, 22, 12, 12);
        assert_eq!(l.frame.bottom, anchor.top);
        assert_eq!(l.frame.top, anchor.top - (12 * 20 + 2));
    }

    /// A list too tall for the window is pinned to the top of it.
    ///
    /// Neither direction fits, and the upward flip is the one that runs off
    /// the screen: a negative top would put the first options above the
    /// desktop, where the pointer cannot reach them and the keyboard scrolls
    /// to nothing visible.
    #[test]
    fn a_list_taller_than_the_window_starts_at_the_top_of_it() {
        let anchor = Rect::new(40, 200, 240, 224);
        let l = ListLayout::of(anchor, 300, 20, 22, 12, 12);
        assert_eq!(l.frame.top, 0);
    }

    /// A scrolling list gives the bar its width out of the option rows.
    ///
    /// The bar is drawn inside the frame, over the right edge of the rows, so
    /// a row drawn its full width loses its last characters underneath it.
    /// Charged here and in `column_needs` from the same function, which is
    /// what keeps the window wide enough for a list that is narrow enough.
    #[test]
    fn a_scrolling_list_gives_up_row_width_to_its_bar() {
        let anchor = Rect::new(40, 100, 240, 124);
        let fits = ListLayout::of(
            anchor,
            600,
            20,
            22,
            DROPDOWN_MAX_VISIBLE,
            DROPDOWN_MAX_VISIBLE,
        );
        let scrolls = ListLayout::of(
            anchor,
            600,
            20,
            22,
            DROPDOWN_MAX_VISIBLE + 1,
            DROPDOWN_MAX_VISIBLE,
        );

        assert_eq!(fits.bar, 0);
        assert_eq!(scrolls.bar, scrollbar_width(22));
        assert_eq!(fits.frame, scrolls.frame, "the bar does not move the frame");
        assert_eq!(
            fits.option(0).right - scrolls.option(0).right,
            scrollbar_width(22),
            "the row gives up exactly the bar's width and no more"
        );
    }

    /// Which of the three an option is, and which wins when two coincide.
    ///
    /// They coincide the moment a list opens — it opens with the highlight on
    /// the current choice — so the order is not a tie-break for a rare case,
    /// it is the ordinary one. Answered the other way round, a freshly opened
    /// list shows no highlight at all until the first arrow key, and the
    /// keyboard appears not to have arrived anywhere.
    #[test]
    fn the_highlight_outranks_the_selection_it_sits_on() {
        assert_eq!(OptionRole::of(3, 3, 3), OptionRole::Highlighted);
        assert_eq!(OptionRole::of(3, 3, 5), OptionRole::Selected);
        assert_eq!(OptionRole::of(3, 1, 3), OptionRole::Highlighted);
        assert_eq!(OptionRole::of(3, 1, 5), OptionRole::Plain);
    }

    /// The stepper triangle: centred, pointing the way it steps, and roughly
    /// equilateral.
    ///
    /// The last of those is the one that was wrong. The height used to be
    /// `half_w`, against a base of `half_w * 2`, so the glyph came out exactly
    /// twice as wide as it was tall — a flattened wedge, not an arrowhead —
    /// and a `Polygon` call says nothing about the shape that reached it.
    #[test]
    fn a_stepper_points_the_way_it_steps() {
        // A 16x16 button, so the half-width is 4 and the height 7.
        let button = Rect::new(200, 60, 216, 76);
        let (cx, cy) = (208, 68);

        let [apex, left, right] = stepper_points(&button, true);
        assert_eq!(apex, (cx, cy - 3), "the apex is above the centre");
        assert_eq!(left, (cx - 4, cy + 4));
        assert_eq!(right, (cx + 4, cy + 4));

        let [apex, left, right] = stepper_points(&button, false);
        assert_eq!(apex, (cx, cy + 3), "and below it for a down stepper");
        assert_eq!(left, (cx - 4, cy - 4));
        assert_eq!(right, (cx + 4, cy - 4));

        // Base and height within a third of each other, at every size the
        // theme can scale a stepper to.
        for side in [8, 12, 16, 20, 24, 40] {
            let r = Rect::new(0, 0, side, side);
            let [apex, left, right] = stepper_points(&r, true);
            let base = right.0 - left.0;
            let height = left.1 - apex.1;
            assert!(
                base * 2 <= height * 3 && height * 2 <= base * 3,
                "{side}px: base {base} against height {height} is not an arrowhead"
            );
        }
    }

    /// A stepper stays inside its button whatever shape the button is.
    ///
    /// Two claims that only look like one. The floors — a half-width of at
    /// least two, a height of at least three — exist because the theme scales
    /// the dialog down as well as up and a triangle two pixels tall is not an
    /// arrow; they must not push the glyph out of a button too small to hold
    /// them. And the triangle is sized off the button's *smaller* side, which
    /// is what keeps it inside a button that is wider than it is tall — a
    /// shape the scrollbar produces whenever the bar is wide and the list
    /// short, since the buttons are capped at a third of the bar's height.
    #[test]
    fn a_stepper_fits_whatever_shape_its_button_is() {
        for r in [
            Rect::new(0, 0, 6, 6),
            Rect::new(0, 0, 16, 16),
            Rect::new(0, 0, 40, 10),
            Rect::new(0, 0, 10, 40),
        ] {
            for up in [true, false] {
                for (x, y) in stepper_points(&r, up) {
                    assert!(
                        (r.left..=r.right).contains(&x) && (r.top..=r.bottom).contains(&y),
                        "{r:?} up={up}: ({x}, {y}) is outside the button"
                    );
                }
            }
        }
    }

    /// Every rectangle `layout` produces, for one frame, by value.
    ///
    /// The other tests in this file check *relations* — the thumb is inside
    /// the track, the buttons are square, a drag moves the list — and mutation
    /// testing showed what that costs. Nineteen mutants of `layout` survived
    /// the whole suite, every one of them a swapped arithmetic operator in the
    /// derivation of these five rectangles: `frame.right - 1 - width` becoming
    /// a division, `bar_h / 3` becoming a remainder, `travel * scroll / span`
    /// multiplying where it divided. Each produced a different scrollbar, and
    /// every relation the suite asserted remained true of it, because a wrong
    /// bar is still inside its own frame and still has square buttons.
    ///
    /// So this pins the numbers. It is one test rather than nineteen because
    /// the fault they each describe is one fault — the geometry came out
    /// different — and one failure naming the changed number says more than
    /// nineteen failures naming nineteen relations. The frame is the same
    /// deterministic one the rest of the file uses; the scroll offset is 5 of
    /// a possible 12, chosen away from both ends so the thumb's placement
    /// exercises the multiplication and the division rather than landing on
    /// the value either bound would give.
    ///
    /// When a deliberate change to the layout makes it fail, the numbers are
    /// what to re-derive — by hand, from the new rule — not what to paste from
    /// the failure message. Pasting the output back in is how a value test
    /// stops testing anything, and it is the exact reason the relations above
    /// were not enough.
    #[test]
    fn the_geometry_is_pinned_by_value() {
        let sb = ScrollbarGeometry::layout(frame(), 16, 24, 12, 5)
            .expect("24 options in a list of 12 scrolls, so there is a bar");

        let expect = |name: &str, got: Rect, want: (i32, i32, i32, i32)| {
            assert_eq!(
                (got.left, got.top, got.right, got.bottom),
                want,
                "{name} is not where the layout rule puts it"
            );
        };
        expect("bar", sb.bar, (383, 51, 399, 363));
        expect("up", sb.up, (383, 51, 399, 67));
        expect("down", sb.down, (383, 347, 399, 363));
        expect("track", sb.track, (383, 67, 399, 347));
        expect("thumb", sb.thumb, (383, 125, 399, 265));
    }

    /// A list that fits gets no scrollbar at all.
    ///
    /// This is the requirement, not an optimisation: an inert bar next to a
    /// two-option log-level list is a control that looks operable and does
    /// nothing, and it takes width from the options to do it. The bar appears
    /// only when there is something off screen to reach.
    #[test]
    fn a_list_that_fits_has_no_scrollbar() {
        for total in 1..=DROPDOWN_MAX_VISIBLE {
            let visible = total.min(DROPDOWN_MAX_VISIBLE);
            assert!(
                ScrollbarGeometry::layout(frame(), 16, total, visible, 0).is_none(),
                "{total} options fit in {visible} slots and must not draw a scrollbar"
            );
            assert_eq!(
                scrollbar_width_for(total, 22),
                0,
                "{total} options must not be charged for a scrollbar"
            );
        }

        let over = DROPDOWN_MAX_VISIBLE + 1;
        assert!(
            ScrollbarGeometry::layout(frame(), 16, over, DROPDOWN_MAX_VISIBLE, 0).is_some(),
            "one option too many must produce a scrollbar"
        );
        assert!(
            scrollbar_width_for(over, 22) > 0,
            "and the window must be measured wide enough for it"
        );
    }

    /// The painter and the measurer must ask the same question about whether
    /// a bar is needed. They used to ask it in two different forms — `total >
    /// visible` in one, `options.len() > MAX` in the other — which agree only
    /// because `visible` is derived from `total`, an equivalence written down
    /// nowhere.
    #[test]
    fn the_painter_and_the_measurer_agree_on_when_a_bar_exists() {
        for total in 1..40 {
            let visible = total.min(DROPDOWN_MAX_VISIBLE);
            let painted = ScrollbarGeometry::layout(frame(), 16, total, visible, 0).is_some();
            let measured = scrollbar_width_for(total, 22) > 0;
            assert_eq!(
                painted, measured,
                "{total} options: painter says {painted}, measurer says {measured}"
            );
        }
    }

    /// Every part of the scrollbar is where the others are not, and all of
    /// them are inside the frame.
    ///
    /// Overlap here is not cosmetic. The hit test runs in reverse over the
    /// hotspots, so a button that overlaps the track would swallow the page
    /// jump beneath it, and a track that escaped the frame would put a
    /// clickable region over the dialog behind the list.
    #[test]
    fn the_scrollbar_parts_tile_the_bar_without_overlapping() {
        let f = frame();
        let sb = ScrollbarGeometry::layout(f, 16, 24, 12, 5).expect("24 options must scroll");

        assert!(sb.bar.left >= f.left && sb.bar.right <= f.right);
        assert!(sb.bar.top >= f.top && sb.bar.bottom <= f.bottom);
        assert_eq!(sb.up.top, sb.bar.top, "the up button caps the bar");
        assert_eq!(sb.down.bottom, sb.bar.bottom, "the down button ends it");
        assert_eq!(sb.track.top, sb.up.bottom, "no gap below the up button");
        assert_eq!(sb.track.bottom, sb.down.top, "no gap above the down button");
        assert!(
            sb.up.bottom <= sb.track.top,
            "buttons must not eat the track"
        );
        assert!(
            sb.thumb.top >= sb.track.top && sb.thumb.bottom <= sb.track.bottom,
            "the thumb must stay inside its track"
        );
    }

    /// The thumb reaches both ends of its travel, and only at the ends.
    ///
    /// A thumb that stops short of the bottom tells the user there is more
    /// list below when there is not, and one that reaches the bottom early
    /// says the opposite. Both are the scrollbar lying about position, which
    /// is the only thing it is for.
    #[test]
    fn the_thumb_reaches_both_ends_and_nowhere_past_them() {
        let f = frame();
        let (total, visible) = (24, 12);
        let last = total - visible;

        let top = ScrollbarGeometry::layout(f, 16, total, visible, 0).unwrap();
        assert_eq!(
            top.thumb.top, top.track.top,
            "at scroll 0 the thumb sits at the top of the track"
        );

        let bottom = ScrollbarGeometry::layout(f, 16, total, visible, last).unwrap();
        assert_eq!(
            bottom.thumb.bottom, bottom.track.bottom,
            "at the last screenful the thumb sits at the bottom"
        );

        // And it advances monotonically in between, or the mark does not
        // track the list it is describing.
        let mut previous = top.thumb.top;
        for scroll in 1..=last {
            let sb = ScrollbarGeometry::layout(f, 16, total, visible, scroll).unwrap();
            assert!(
                sb.thumb.top >= previous,
                "the thumb went backwards at scroll {scroll}"
            );
            previous = sb.thumb.top;
        }
    }

    /// Dragging is the exact inverse of drawing.
    ///
    /// `scroll_at` maps a pointer position back onto a scroll offset, and it
    /// has to undo what `layout` did or the thumb drifts away from the
    /// cursor. The round trip is the invariant: place the thumb for a given
    /// offset, ask what offset its own position means, get the same number.
    #[test]
    fn a_drag_round_trips_through_the_thumb_position() {
        let f = frame();
        let (total, visible) = (24, 12);
        for scroll in 0..=(total - visible) {
            let sb = ScrollbarGeometry::layout(f, 16, total, visible, scroll).unwrap();
            assert_eq!(
                sb.scroll_at(sb.thumb.top, total - visible),
                scroll,
                "dragging the thumb to where it was drawn moved the list"
            );
        }
    }

    /// A drag past either end of the track clamps rather than running off.
    #[test]
    fn a_drag_past_the_ends_clamps() {
        let f = frame();
        let (total, visible) = (24, 12);
        let sb = ScrollbarGeometry::layout(f, 16, total, visible, 0).unwrap();
        assert_eq!(sb.scroll_at(sb.track.top - 500, total - visible), 0);
        assert_eq!(
            sb.scroll_at(sb.track.bottom + 500, total - visible),
            total - visible
        );
    }

    /// The thumb never shrinks below a grabbable size, however long the list.
    ///
    /// Proportional sizing alone drives it to a couple of pixels on a long
    /// list, and a two-pixel target is one no pointer can reliably hit — the
    /// control becomes decorative again, which is the fault this whole
    /// scrollbar replaces.
    #[test]
    fn the_thumb_stays_grabbable_on_a_long_list() {
        let f = frame();
        for total in [24, 60, 500, 5_000] {
            let sb = ScrollbarGeometry::layout(f, 16, total, 12, 0).unwrap();
            let h = sb.thumb.bottom - sb.thumb.top;
            assert!(
                h >= 12,
                "a thumb {h}px tall over {total} options cannot be grabbed"
            );
            assert!(
                sb.thumb.bottom <= sb.track.bottom,
                "the minimum size must not push the thumb out of the track"
            );
        }
    }

    /// A drag survives the repaints that happen during it.
    ///
    /// Every mouse move repaints, and the repaint re-lays the scrollbar at
    /// the offset that move just requested — so the thumb the next move sees
    /// is not the thumb that was grabbed. The drag therefore fixes its grab
    /// offset at the press and resolves against the *track*, which does not
    /// move. Had it re-derived the offset from the current thumb each time,
    /// the reference would follow the pointer and the two would chase each
    /// other: the list accelerates away under a pointer moving steadily.
    ///
    /// The check is that a pointer moved in equal steps produces the same
    /// offsets whether or not the geometry was rebuilt along the way.
    #[test]
    fn a_drag_is_unaffected_by_repaints_midway_through() {
        let f = frame();
        let (total, visible) = (24, 12);

        let start = ScrollbarGeometry::layout(f, 16, total, visible, 0).unwrap();
        // Grabbed a third of the way down the thumb.
        let grab = (start.thumb.bottom - start.thumb.top) / 3;
        let press_y = start.thumb.top + grab;

        let mut scroll = 0usize;
        let mut with_repaints = Vec::new();
        let mut without = Vec::new();
        for step in 1..=20 {
            let y = press_y + step * 3;
            // What the live code does: resolve against the geometry as last
            // painted, which reflects the previous move.
            let painted = ScrollbarGeometry::layout(f, 16, total, visible, scroll).unwrap();
            scroll = painted.scroll_at(y - grab, total - visible);
            with_repaints.push(scroll);
            // What it would be with no repaint at all.
            without.push(start.scroll_at(y - grab, total - visible));
        }

        assert_eq!(
            with_repaints, without,
            "the thumb moving under the drag changed where the drag pointed"
        );
        // And it genuinely moved, or the comparison above is vacuous.
        assert!(
            with_repaints.last() > with_repaints.first(),
            "the drag did not move the list at all: {with_repaints:?}"
        );
    }
}
