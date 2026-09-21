//! The rectangle this program's own geometry is expressed in.
//!
//! # Why not `windows::Win32::Foundation::RECT`
//!
//! It was, and the cost showed up as a trait disappearing. `RECT` stopped
//! deriving `Eq` in `windows` 0.62, and because two of this program's own types
//! carried one inside, they lost `Eq` as well — `ScrollbarGeometry` and
//! `Effect`, neither of which has anything to do with Win32. Both grew a
//! comment explaining that the missing trait was somebody else's decision.
//!
//! That is the wrong dependency to have. What traits a rectangle implements is
//! a property of this program's geometry, not something a minor release of a
//! binding crate gets to change. The layout, the scrollbar and the hit-test
//! table are domain code: they describe where things are on screen, a question
//! that exists whether or not Windows does.
//!
//! # What it is not
//!
//! Not a wrapper that adds behaviour — the fields are the fields, and the
//! conversions are the two `From` impls at the bottom. A type that merely
//! renamed `RECT` would be the "abstraction for its own sake" this project
//! rejects elsewhere. This one earns its place by owning its own trait
//! implementations and by keeping `use windows::` out of the modules that
//! reason about layout.
//!
//! Conversion happens where the geometry meets the API: the painter and the
//! window procedure turn a `Rect` into a `RECT` at the call that needs one. In
//! both directions it is a field-for-field copy of four `i32`s, which the
//! optimiser removes.

use windows::Win32::Foundation::RECT;

/// A rectangle in device pixels, `right` and `bottom` exclusive.
///
/// The same convention Win32 uses, deliberately: this type is converted to and
/// from `RECT` at every drawing call, and two rectangle types that disagreed
/// about whether their edges are inclusive would be a defect that renders as a
/// one-pixel seam and reads as correct in both files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    /// The rectangle spanning `left..right` by `top..bottom`.
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Width, never negative.
    ///
    /// A rectangle whose right edge is left of its left edge has no width
    /// rather than a negative one. Win32 produces such rectangles routinely —
    /// an empty update region is one — and a negative width propagated into a
    /// size calculation is how a window ends up asking GDI for a bitmap it
    /// cannot make.
    pub const fn width(&self) -> i32 {
        if self.right > self.left {
            self.right - self.left
        } else {
            0
        }
    }

    /// Height, never negative. As [`Rect::width`].
    pub const fn height(&self) -> i32 {
        if self.bottom > self.top {
            self.bottom - self.top
        } else {
            0
        }
    }

    /// Whether the rectangle encloses any pixels at all.
    pub const fn is_empty(&self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// Whether `(x, y)` falls inside, with the right and bottom edges excluded.
    ///
    /// Exclusive because the edges are: a point at `right` belongs to whatever
    /// is drawn next along, and counting it here is what makes two adjacent
    /// controls both claim the same column of pixels.
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// Top-left corner of a `w` by `h` box centred in this rectangle.
    ///
    /// The rectangle is a monitor's work area, so it is not at the origin —
    /// the taskbar takes a strip off one side, and a secondary display starts
    /// wherever the virtual desktop puts it. Centring is therefore `left +
    /// (width - w) / 2` and not `(width - w) / 2`, and the difference is a
    /// window that opens on the primary monitor when it was meant for the one
    /// the mouse is on.
    ///
    /// A box larger than the rectangle gets a negative offset, and that is the
    /// intended answer rather than an oversight: it overhangs by the same
    /// amount on both sides, which keeps the middle of it — where the content
    /// is — on screen. Clamping to zero here would push the whole overhang
    /// onto one edge instead.
    pub const fn centred_origin(&self, w: i32, h: i32) -> (i32, i32) {
        (
            self.left + (self.width() - w) / 2,
            self.top + (self.height() - h) / 2,
        )
    }
}

impl From<RECT> for Rect {
    fn from(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

impl From<Rect> for RECT {
    fn from(r: Rect) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backwards rectangle has no size rather than a negative one.
    ///
    /// Win32 hands out inverted rectangles as a matter of course — an empty
    /// update region arrives as one — and the arithmetic downstream is sizes
    /// and offsets. A negative width reaching `Bitmap::compatible` is the
    /// difference between "nothing to paint" and a refused allocation.
    #[test]
    fn an_inverted_rectangle_measures_zero() {
        let backwards = Rect::new(10, 10, 4, 4);
        assert_eq!(backwards.width(), 0);
        assert_eq!(backwards.height(), 0);
        assert!(backwards.is_empty());
    }

    /// The right and bottom edges belong to the next control along.
    ///
    /// This is the rule that keeps two adjacent hotspots from both claiming
    /// the same column of pixels, which shows up as a click landing on the
    /// wrong one of two touching buttons.
    #[test]
    fn the_far_edges_are_outside() {
        let r = Rect::new(0, 0, 10, 4);
        assert!(r.contains(0, 0), "the near corner is inside");
        assert!(r.contains(9, 3), "the last pixel is inside");
        assert!(!r.contains(10, 3), "the right edge is not");
        assert!(!r.contains(9, 4), "the bottom edge is not");
    }

    /// A window is centred on the rectangle it is given, wherever that
    /// rectangle happens to start.
    ///
    /// The offset is the whole of it. The rectangle is a monitor work area, so
    /// on a taskbar-on-the-left desktop it starts at x = 60, and on a second
    /// display it starts wherever the virtual desktop puts that display —
    /// which may be a negative coordinate. Dropping `left` and `top` from the
    /// sum is the mistake that opens every window on the primary monitor no
    /// matter which one was chosen, and it is invisible on the single-monitor,
    /// taskbar-at-the-bottom desktop most testing happens on.
    #[test]
    fn a_window_is_centred_on_the_area_it_is_given() {
        // A 1920x1080 display with a 60px taskbar down its left edge.
        let work = Rect::new(60, 0, 1980, 1080);
        assert_eq!(work.centred_origin(400, 300), (820, 390));

        // A second display left of the primary one, so its coordinates are
        // negative. The window still lands in the middle of it.
        let secondary = Rect::new(-1280, 0, 0, 720);
        assert_eq!(secondary.centred_origin(400, 300), (-840, 210));
    }

    /// A window larger than the work area overhangs both edges equally.
    ///
    /// The alternative — clamping the offset at zero — puts the whole overhang
    /// on the right and bottom, so the title bar and the first column of
    /// content stay visible but everything the window grew by is off screen at
    /// once. Split evenly, the middle of the window is on screen, which is
    /// where its content is.
    #[test]
    fn a_window_too_large_for_the_area_overhangs_evenly() {
        let small = Rect::new(0, 0, 100, 100);
        assert_eq!(small.centred_origin(300, 200), (-100, -50));
    }

    /// An inverted rectangle centres on its own top-left rather than sending
    /// the window somewhere arbitrary: its size is zero, so the offset is
    /// half the window, back from the corner.
    #[test]
    fn centring_in_a_backwards_rectangle_uses_its_corner() {
        let backwards = Rect::new(50, 50, 10, 10);
        assert_eq!(backwards.centred_origin(20, 40), (40, 30));
    }

    /// Converting through `RECT` and back changes nothing.
    ///
    /// The conversion is the whole of the boundary this type exists to create,
    /// so it is worth one assertion that it is a copy and not a reinterpretation
    /// — the two types agree that the far edges are exclusive, and a round trip
    /// is the cheapest way to say so.
    #[test]
    fn a_round_trip_through_the_win32_rectangle_is_the_identity() {
        let original = Rect::new(-3, 7, 11, 40);
        let there: RECT = original.into();
        let back: Rect = there.into();
        assert_eq!(original, back);
    }

    /// `Eq` is derived, which is the point of owning the type.
    ///
    /// The predecessor lost it when `windows` 0.62 stopped deriving `Eq` on
    /// `RECT`, and two of this program's own types lost it with them. Pinned so
    /// that a future edit that reaches for the foreign type again fails here.
    #[test]
    fn the_type_is_eq_without_asking_anyone() {
        fn requires_eq<T: Eq>(_: &T) {}
        requires_eq(&Rect::default());
    }
}
