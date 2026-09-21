use windows::Win32::Graphics::Gdi::FillRect;

use super::FOCUS_RING_OUTSET;
use crate::color::Color;
use crate::ui::gdi::{Brush, Canvas};
use crate::ui::rect::Rect;
use crate::ui::row::HotspotId;
use crate::ui::theme::ScaledTheme;
/// Fills a rectangle with one colour and no border.
///
/// Used for the parts of an open list that sit inside its frame — selected
/// and highlighted rows, the scrollbar track and its thumb — where a second
/// border would draw a line through the middle of the list.
pub(super) fn fill_solid(canvas: Canvas<'_>, rect: &Rect, fill: Color) {
    let Some(brush) = Brush::solid(fill) else {
        return;
    };
    // SAFETY: `canvas` proves the context is live, `rect` is a live local of
    // the caller, and the brush outlives the call — it is dropped one line
    // below, after `FillRect` has returned.
    unsafe { FillRect(canvas.raw(), &(*rect).into(), brush.raw()) };
}

/// The fill a button takes right now: its hover fill when the pointer is over
/// it, its resting fill otherwise.
///
/// One place decides this so every button — buzzer, self-test, Settings, OK,
/// Cancel — reads its state the same way. The resting fill is always
/// `surface`; the hover fill is the theme's `hover_fill` of it, which lightens
/// on the dark theme and darkens on the light one (see `Colors::hover_fill`).
pub(super) fn button_fill(hovered: Option<HotspotId>, id: HotspotId, theme: &ScaledTheme) -> Color {
    if hovered == Some(id) {
        theme.colors().hover_surface()
    } else {
        theme.colors().surface
    }
}

/// Fills a rect and strokes a two-layer border.
///
/// Buttons, fields and closed dropdowns are hard to pick out against a surface
/// of a similar tone, so they are given a visible frame. The frame is two
/// layers to read as a closed, slightly rounded shape without a region or path
/// object:
///
/// - a continuous 1px underlay in `border_soft`, corners included, drawn first;
/// - the four sides in the stronger `border`, each shortened by a small inset
///   so the corners stay the soft tone.
///
/// The inset fakes the rounded corner: a shorter side leaves more of the soft
/// underlay showing at the corner, which reads as curvature. It is half the
/// radius, so the accented sides reach most of the way to the corner and the
/// visible rounding stays small.
///
/// `border` and `border_soft` both come from the active theme, so each theme
/// picks its own flank/corner pair — a prominent side over a quieter corner —
/// tuned to its own ground.
pub(super) fn fill_bordered(
    canvas: Canvas<'_>,
    rect: &Rect,
    fill: Color,
    border: Color,
    border_soft: Color,
    radius: i32,
) {
    // The three calls below take the same borrowed canvas, so each of them
    // proves the context live for itself; there is nothing left to justify.
    fill_solid(canvas, rect, fill);
    // Clamped because the rounding is drawn by shortening straight edges: past
    // a few pixels the sides stop reading as a frame and start reading as four
    // dashes.
    let corner = radius.clamp(0, 6);
    // Underlay first: a full closed frame that fills the corners.
    stroke_frame(canvas, rect, border_soft, 0);
    // Accented sides on top, shortened by half the corner so the underlay
    // shows through at the corners as the rounding.
    stroke_frame(canvas, rect, border, corner / 2);
}

/// Draws the four edges of `rect` one pixel wide, each shortened by `inset` at
/// both ends so the corners are left to whatever is underneath.
///
/// `inset == 0` draws a closed frame — four edges that meet — which is what a
/// border's underlay and the focus ring both are. A positive inset opens the
/// corners, which is how the accented flanks let the underlay show through as
/// rounding.
///
/// Shared by the control borders and the focus ring so there is one definition
/// of what a one-pixel frame is, and one place that gets the half-open
/// rectangle arithmetic right.
pub(super) fn stroke_frame(canvas: Canvas<'_>, rect: &Rect, colour: Color, inset: i32) {
    let Some(brush) = Brush::solid(colour) else {
        return;
    };
    let strokes = [
        Rect {
            left: rect.left + inset,
            top: rect.top,
            right: rect.right - inset,
            bottom: rect.top + 1,
        },
        Rect {
            left: rect.left + inset,
            top: rect.bottom - 1,
            right: rect.right - inset,
            bottom: rect.bottom,
        },
        Rect {
            left: rect.left,
            top: rect.top + inset,
            right: rect.left + 1,
            bottom: rect.bottom - inset,
        },
        Rect {
            left: rect.right - 1,
            top: rect.top + inset,
            right: rect.right,
            bottom: rect.bottom - inset,
        },
    ];
    for stroke in &strokes {
        // SAFETY: `canvas` proves the context is live, each `stroke` is an
        // element of a live local array, and the brush outlives the whole loop.
        unsafe { FillRect(canvas.raw(), &(*stroke).into(), brush.raw()) };
    }
}

/// Draws the keyboard focus ring around `control`, when `control` is the one
/// the keyboard is aimed at and the ring is being shown.
///
/// Every focusable control calls this with its own rectangle, so the width and
/// the standoff come from one place rather than from each arm's idea of them.
///
/// A **closed** frame, not one with its corners opened like the control
/// borders below it. A marker has to be unmistakably one shape; four separate
/// strokes read as an artefact of the drawing, especially at the small sizes
/// a checkbox's box comes in. The corner pixels sitting a hair outside a
/// rounded button's corner is the price, and it is not visible at one pixel.
///
/// `focused` and `visible` are separate arguments because they are separate
/// facts: a dialog opens focused on its dismissing button with no ring drawn,
/// so that Enter dismisses it without a marker pointing at a button the user
/// never chose.
pub(super) fn draw_focus_ring(
    canvas: Canvas<'_>,
    control: &Rect,
    focused: bool,
    visible: bool,
    theme: &ScaledTheme,
) {
    if !focused || !visible {
        return;
    }
    let ring = Rect {
        left: control.left - FOCUS_RING_OUTSET,
        top: control.top - FOCUS_RING_OUTSET,
        right: control.right + FOCUS_RING_OUTSET,
        bottom: control.bottom + FOCUS_RING_OUTSET,
    };
    // `stroke_frame` takes the same borrowed canvas and is a safe function;
    // `ring` is a live local.
    stroke_frame(canvas, &ring, theme.colors().focus_ring, 0);
}
