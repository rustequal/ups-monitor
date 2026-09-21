use std::cell::RefCell;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HFONT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, GWLP_USERDATA, HICON,
};

use super::Role;
use crate::ui::layout::PanelContent;
use crate::ui::rect::Rect;
use crate::ui::row::{DropdownItem, Edit, HotspotId, Row};
use crate::ui::scrollbar::{last_scroll_offset, ScrollbarGeometry};
use crate::ui::theme::{ScaledTheme, Theme};
use crate::ui::Dpi;
/// What the left mouse button is dragging while it is held.
///
/// The two drags this window supports are mutually exclusive: a press either
/// lands on the scrollbar thumb or inside a text field, and the thumb is
/// tested first and returns. Expressing that as one value rather than two
/// `Option` fields is the point — with two fields the exclusion is a claim in
/// a comment, and the release path has to check them in an order that is only
/// correct because both are never set at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gesture {
    /// The button is not held on anything draggable.
    None,
    /// The scrollbar thumb, carrying the offset from the thumb's top edge to
    /// where it was grabbed.
    ///
    /// Storing the grab offset rather than only a "dragging" flag is what
    /// makes the thumb stay under the point the user took hold of. Without it,
    /// the first mouse move snaps the thumb's top to the pointer, which jumps
    /// the list by up to a thumb's height the instant a drag begins.
    Thumb { grab: i32 },
    /// Drag-selection inside the text field with this id. The anchor was fixed
    /// on the press; every move extends the selection to the character now
    /// under the pointer.
    Text { id: HotspotId },
}

/// One clickable region, resolved at paint time and hit-tested on click.
#[derive(Clone, Copy)]
pub(super) struct Hotspot {
    pub(super) rect: Rect,
    pub(super) id: HotspotId,
    /// True for the raised, clickable controls that take pointer-hover
    /// feedback — buzzer, self-test, Settings, OK, Cancel.
    ///
    /// Not decided here: it is `Row::is_button()` of the row this hotspot came
    /// from, snapped in at push time. The window procedure walks the painted
    /// frame, not the rows — it never reaches into application state — so the fact has
    /// to travel with the hotspot; but its *value* has one source, the row
    /// variant, so a new kind of button cannot be misclassified by a
    /// hand-typed literal here.
    pub(super) is_button: bool,
}

/// The open list exactly as the last paint pass drew it.
///
/// Four facts the input path resolves gestures against, held as one value with
/// one writer — the painter — and set or cleared on every pass. As four
/// separate fields they had four lifetimes, and they diverged: `scrollbar` was
/// cleared only inside the branch that draws a list, so closing one left last
/// frame's thumb rectangle behind, and a press into that empty strip started a
/// drag of a scrollbar that was no longer on screen. The other three were
/// cleared when the open list *changed*, which is a different moment again.
/// One `Option`, assigned once per pass, cannot go out of step with itself.
///
/// Written by the painter and read by the handlers, never the reverse: the
/// question the input path is asking is "where is this on screen", and only the
/// pass that put it there knows. A frame counter — the other remedy the audit
/// offered — would answer a question this shape does not leave open, since a
/// pass that draws no list writes `None` rather than leaving the previous
/// answer standing.
pub(super) struct PaintedList {
    /// Hotspot id of the dropdown this list belongs to.
    ///
    /// Mirrored from the rows so the window procedure can route arrow keys and
    /// the wheel without walking the row list on every message.
    pub(super) id: HotspotId,
    /// Bounding rectangle, frame included, so a scroll can invalidate this
    /// area alone. Taken from the painter rather than recomputed: an
    /// invalidation rectangle that disagrees with where the list was actually
    /// drawn leaves a stale strip on screen.
    pub(super) bounds: Rect,
    /// The scroll offset as *drawn*, which is the requested one clamped to the
    /// last page.
    ///
    /// A drag consults it to drop mouse moves that would not change the
    /// picture, so it has to be what is on screen rather than what the owner's
    /// draft intends.
    pub(super) scroll: usize,
    /// The scrollbar, or `None` when the list fits and has none.
    ///
    /// A drag is resolved against the geometry that was actually drawn;
    /// recomputing it from the rows on every mouse move would be a second
    /// implementation of the same arithmetic — the classic way for a thumb to
    /// end up half a pixel from where it can be grabbed.
    pub(super) scrollbar: Option<ScrollbarGeometry>,
}

/// One text field exactly as a frame drew it.
///
/// The rectangle lives in the frame's hotspots; what is here is the *ruler* —
/// where every character of the value ended, in pixels from `text_left`. The
/// painter measures that anyway, once, to place the selection band and the
/// caret, so recording it costs a move rather than a measurement.
///
/// It is here rather than measured again on the click for the reason the whole
/// of [`Painted`] is one value: the answer has to be about the text that is on
/// screen. Measured again from `content.rows`, the ruler came from the rows
/// waiting to be drawn while the rectangle came from the rows already drawn —
/// two generations, one caret. It also needed a device context with the
/// window's font selected into it, which is a precondition the caller had to
/// meet and could quietly fail to; a recorded ruler has no precondition at all.
pub(super) struct PaintedField {
    pub(super) id: HotspotId,
    /// Left edge of the text, in client pixels.
    pub(super) text_left: i32,
    /// Where each character ends, in pixels from `text_left`.
    pub(super) ends: Vec<i32>,
}

/// One frame, as it was drawn.
///
/// Everything the input path needs to know about *what is on screen*, held as
/// one value with one writer and replaced whole on every pass. The three
/// records here — where the controls are, where the caret box is, what the
/// open list looks like — used to be three fields of `WindowState` beside
/// `content`, which is the rows that will be drawn *next*. Nothing said which
/// of the two any given method was reading, and they can be a generation
/// apart: `set_content` runs once a second while the panel is open, and
/// `WM_PAINT` is the lowest-priority message there is, so every mouse and key
/// message between the two saw a new model over an old picture.
///
/// One value also makes the empty case say what it means. A pass that draws
/// nothing — a zero-sized client, a back buffer that could not be created —
/// leaves `None` here, so no click can be resolved against a layout that is no
/// longer on screen. As three fields it left the previous frame's rectangles
/// standing, because the line that cleared them came after the early returns.
#[derive(Default)]
pub(super) struct Painted {
    /// Where every control ended up, in the order they were drawn.
    pub(super) hotspots: Vec<Hotspot>,
    /// The focused field's value box, so the blink timer can invalidate that
    /// rectangle alone. `None` when no field was focused.
    pub(super) caret_rect: Option<Rect>,
    /// The open list, or `None` when this pass drew none.
    pub(super) list: Option<PaintedList>,
    /// The text fields this pass drew, with the ruler each was drawn against.
    pub(super) fields: Vec<PaintedField>,
}

impl Painted {
    /// The dropdown this frame drew open, if it drew one.
    ///
    /// Asked of a frame rather than of the window, because both sides of the
    /// question the painter puts — which list was open before, which is open
    /// now — are frames, and asking them the same way is what keeps the answer
    /// comparable. Mirroring the id into a field of its own would be a second
    /// thing to clear, and the pair going out of step is precisely how the
    /// input path came to resolve gestures against a list no longer on screen.
    pub(super) fn open_list(&self) -> Option<HotspotId> {
        self.list.as_ref().map(|list| list.id)
    }

    /// Resolves a click at client `x` to a caret character index in field
    /// `id`, against the ruler that field was drawn with.
    ///
    /// The index is the boundary nearest the pointer: each character's own
    /// midpoint decides whether the caret lands before or after it, which is
    /// what makes a click feel like it goes where the eye aimed rather than
    /// always snapping to one side.
    ///
    /// `None` when this frame drew no such field — the id names something
    /// else, or the rows changed since. The caller puts the caret at the start,
    /// which is the only answer available and the same one it gave before.
    pub(super) fn caret_at(&self, id: HotspotId, x: i32) -> Option<usize> {
        let field = self.fields.iter().find(|f| f.id == id)?;
        let target = x - field.text_left;
        if target <= 0 {
            return Some(0);
        }
        let mut start = 0;
        for (i, end) in field.ends.iter().enumerate() {
            if target <= (start + end) / 2 {
                return Some(i);
            }
            start = *end;
        }
        Some(field.ends.len())
    }
}

/// Everything the painter needs about one open list.
///
/// Grouped rather than passed as six parameters: they are one concept, they
/// are always read together, and a painter taking ten positional arguments is
/// a call site nobody can check by eye.
pub(super) struct DropdownState<'a> {
    pub(super) id: HotspotId,
    pub(super) options: &'a [DropdownItem],
    pub(super) selected: usize,
    pub(super) highlighted: usize,
    pub(super) scroll: usize,
}

/// Attaches `state` to `hwnd`, which owns it from here on.
///
/// Called once, immediately after the window is created and before anything
/// asks it to paint or answer a click. Until it runs, `GWLP_USERDATA` is zero
/// and every handler reads `None` — which is the right answer for the
/// `WM_NCCREATE` and `WM_CREATE` that `CreateWindowExW` delivers before it
/// returns a handle to attach to.
pub(super) fn attach_state(hwnd: HWND, state: WindowState) {
    let state = Box::into_raw(Box::new(RefCell::new(state)));
    // SAFETY: `hwnd` belongs to this program's own window class, so its
    // `GWLP_USERDATA` slot is ours to use, and it is zero until now — this
    // runs once per window, from `create`.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
}

/// What [`detach_state`] found.
///
/// Three outcomes, kept apart because they call for three different responses.
/// Collapsing them into an `Option` would hide the third, which is the only one
/// that is a defect.
pub(super) enum Detached<T = WindowState> {
    /// The window had a state and it is now the caller's.
    Owned(T),
    /// The window had none. It was never attached — which `create` no longer
    /// allows to happen silently — so there is nothing to release.
    Absent,
    /// The state is borrowed right now, so it was left where it is: freeing a
    /// value that a live `&mut` still points into is undefined behaviour, and
    /// leaking one window's state is the lesser of those two. It means a window
    /// is being dropped from inside a [`with_state`] closure, which is a defect
    /// in this module rather than a condition to absorb.
    Blocked,
}

impl Detached {
    /// Reduces the state to whatever the caller actually has to release,
    /// leaving the other two outcomes as they are.
    ///
    /// The map is on the value, not at the call site, because `Absent` and
    /// `Blocked` mean the same thing whatever the caller wanted out of the
    /// state — and a `match` at each call site to say so would be three arms
    /// written again for two of them to do nothing.
    pub(super) fn map<T>(self, f: impl FnOnce(WindowState) -> T) -> Detached<T> {
        match self {
            Self::Owned(state) => Detached::Owned(f(state)),
            Self::Absent => Detached::Absent,
            Self::Blocked => Detached::Blocked,
        }
    }
}

/// Takes `hwnd`'s state back and hands it to the caller.
///
/// The slot is zeroed, so any message that still reaches the window procedure
/// afterwards — and `DestroyWindow` sends two — finds nothing and does nothing.
pub(super) fn detach_state(hwnd: HWND) -> Detached {
    // SAFETY: the slot holds either zero or the pointer `attach_state` made
    // with `Box::into_raw` for this window. Nothing else stores or copies it.
    unsafe {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<WindowState>;
        if state.is_null() {
            return Detached::Absent;
        }
        // Asked before the box is taken, and the answer is the whole reason
        // this is not a bare `Box::from_raw`: a borrow outstanding here is a
        // pointer into memory this call is about to free.
        if (*state).try_borrow_mut().is_err() {
            return Detached::Blocked;
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        Detached::Owned(Box::from_raw(state).into_inner())
    }
}

/// Runs `f` against the state belonging to `hwnd`, if it still has one.
///
/// `None` has two causes and they are not alike. The window has no state — it
/// is being created, or was destroyed while a message about it was still in
/// flight — and that is the ordinary path: nothing to run `f` against, nothing
/// to report. Or the borrow failed, which means this call is nested inside
/// another `with_state` **on the same window**, and *that* is a defect in this
/// module: the inner call silently does nothing, so whatever it was going to
/// do — set the title-bar icon, rebuild a row — simply never happens. That
/// exact failure is how the title bar went stale on a DPI change (see the note
/// in the `WM_DPICHANGED` arm), and it leaves a line in the log now, in the
/// release build that is the only one shipped.
///
/// "On the same window" is what the cell being per-window buys. The state used
/// to live in one thread-local `RefCell<Vec<(isize, WindowState)>>` shared by
/// every window on the thread, so a perfectly ordinary reach into the panel
/// from inside a settings-dialog handler collided with a borrow that had
/// nothing to do with it. Now only true self-nesting can fail. The lookup went
/// with it: the window holds its own state, so there is no vector to scan, and
/// no way for a recycled `HWND` value to be handed a dead window's rows.
pub(super) fn with_state<R>(hwnd: HWND, f: impl FnOnce(&mut WindowState) -> R) -> Option<R> {
    match with_state_access(hwnd, f) {
        Access::Ran(value) => Some(value),
        Access::Absent => None,
        Access::Busy => {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "internal: window state was already borrowed; a UI update was skipped",
            );
            None
        }
    }
}

/// What [`with_state_access`] found, for the one caller that must tell the two
/// failures apart.
///
/// `WM_PAINT` is that caller. `BeginPaint` consumes the update region whether
/// or not anything is drawn, so a pass that cannot paint has to decide between
/// two opposite responses: a window with no state is going away and its region
/// must be validated, or the loop spins on a message nothing will ever answer;
/// a window whose state is momentarily borrowed will be able to paint in a
/// moment, and its region must be left dirty so the frame arrives with the next
/// message instead of being lost. Collapsed into `Option`, both looked like
/// "skipped", and the second one silently cost a frame — for as long as it took
/// something unrelated to invalidate the window again.
pub(super) enum Access<R> {
    /// The closure ran, and this is what it returned.
    Ran(R),
    /// The window has no state: it is being created, or was destroyed while a
    /// message about it was still in flight.
    Absent,
    /// The state is borrowed by an outer call on this same window.
    Busy,
}

/// Runs `f` against the state belonging to `hwnd`, saying which of the two
/// failures happened when it does not run.
///
/// The whole of [`with_state`]'s mechanism lives here so there is one lookup
/// and one borrow, not a probe followed by an attempt: two of those can
/// disagree, and the second would be reasoning about the answer to the first.
pub(super) fn with_state_access<R>(hwnd: HWND, f: impl FnOnce(&mut WindowState) -> R) -> Access<R> {
    // SAFETY: as `detach_state`. The pointer is null or live, and it stays live
    // for this call because only `detach_state` frees it — and it refuses to
    // while the borrow taken just below is outstanding.
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<WindowState> };
    if state.is_null() {
        return Access::Absent;
    }
    // SAFETY: as above — the pointer is non-null and live for this call,
    // because only `detach_state` frees it and it refuses to while the borrow
    // taken here is outstanding.
    let Ok(mut borrowed) = (unsafe { &*state }).try_borrow_mut() else {
        return Access::Busy;
    };
    Access::Ran(f(&mut borrowed))
}

/// Everything one window remembers between messages.
///
/// There is no widget tree here and no retained scene: the panel is painted
/// from scratch on every `WM_PAINT`, out of `content` and the interaction state
/// below it. That is what an immediate-mode painter costs and buys — a control
/// has no object to hold its own hover flag, so every such flag lives in this
/// one struct, and in exchange there is exactly one place where the window's
/// idea of itself can be wrong.
///
/// Roughly four groups, though they are laid out in the order a reader meets
/// them rather than sorted. What to draw: `content`, and the fonts and theme it
/// is measured against. What the monitor is: `dpi`, `theme` scaled to it, and
/// `base_theme` kept unscaled for the next `WM_DPICHANGED`. What the pointer
/// and keyboard are doing: `hotspots` and the four fields naming which one is
/// pressed, hovered, focused or clicked. And what the last paint produced that
/// the next message needs — `painted_list`, the open dropdown's geometry,
/// which hit-testing consults and which nothing else can reconstruct.
///
/// Owned by the window through `SetWindowLongPtrW`, reached only through
/// [`with_state`], and freed by `release_window_state` — which both
/// `Window::drop` and `WM_NCDESTROY` call, because an owned modal can be
/// destroyed by Windows before its own `Window` is dropped. Every field is
/// `pub(super)` because the window procedure, the painter and the layout all
/// read them; nothing outside `ui::window` sees this type at all.
///
/// `hovered` and `hovered_button` are neighbours and are not the same question:
/// the first is an *index* into an open dropdown's options, the second the
/// [`HotspotId`] of a button under the cursor. They used to be the same type,
/// and telling them apart was the reader's job; now the types differ and the
/// compiler will not let one be assigned from the other.
pub(super) struct WindowState {
    pub(super) content: PanelContent,
    /// The theme scaled to `dpi` — the geometry this window is actually laid
    /// out and painted against, not the 96-DPI baseline from `theme.rs`. Every
    /// reader of `state.theme` (`paint`, hit-testing, the scrollbar) gets
    /// device pixels for free because the scaling happened once, here, rather
    /// than at each read site.
    ///
    /// The emphasis this sentence used to carry — *already scaled* — is now the
    /// type: the field beside it holds a `Theme` and this one cannot.
    pub(super) theme: ScaledTheme,
    /// Font family the interface language needs, or `None` for Segoe UI.
    ///
    /// A fact about this window rather than about the process, for the reason
    /// the theme beside it is one: it is read at every measurement and every
    /// repaint, and a window is measured and painted with the family it was
    /// given, not with whatever the last locale change happened to leave in a
    /// cell. The static it replaces was written once at startup and once per
    /// language change, and read from the bottom of a call chain that named it
    /// nowhere.
    pub(super) family: Option<&'static str>,
    /// The theme as `apply_style`'s caller last passed it, at the 96-DPI
    /// baseline, kept verbatim rather than reconstructed by dividing `theme`
    /// back out — which is now not merely wasteful but impossible, since
    /// `ScaledTheme` offers no way back. `WM_DPICHANGED` needs exactly this:
    /// the source to re-scale to the new DPI, the same source `theme` was
    /// itself scaled from, so a monitor-to-monitor move rescales from the
    /// original rather than compounding rounding onto whatever the previous
    /// DPI already left in `theme`.
    pub(super) base_theme: Theme,
    /// The width floor `content.width` was clamped against, at the 96-DPI
    /// baseline — `theme.metrics.panel_width` or `theme.metrics.settings_width`
    /// depending on which kind of window this is. Kept as a plain number rather
    /// than a stored `Role`: this is the one fact a content rebuild needs from
    /// that distinction, and a number does not oblige `WindowState` to know
    /// what a `Role` is or import the type that names it.
    pub(super) min_width: i32,
    /// DPI this window is currently drawn at, read from `GetDpiForWindow`
    /// when the window is created and refreshed on `WM_DPICHANGED`. Kept
    /// alongside `theme` and the fonts because all three change together:
    /// a DPI change is what triggers rescaling the theme and rebuilding the
    /// fonts below, and this is the value that rescaling is done *from*.
    pub(super) dpi: Dpi,
    pub(super) font: HFONT,
    pub(super) font_bold: HFONT,
    pub(super) clicked: Option<HotspotId>,
    pub(super) close_requested: bool,
    /// Hotspot id of the control holding keyboard focus, if any.
    ///
    /// One field for every kind of control, not one for text fields alone:
    /// a window has exactly one keyboard focus, and giving fields their own
    /// notion of it is what made "focus" mean two different things depending
    /// on which control had it. Which control this names is a question for
    /// [`crate::ui::focus`]; whether it is a text field is answered by looking
    /// it up in the rows, not by a second field here.
    pub(super) focus: Option<HotspotId>,
    /// Whether the focus ring is drawn.
    ///
    /// Separate from `focus` because a window opens with a meaningful focus
    /// and no ring: the settings dialog starts on Cancel so that Enter
    /// dismisses it, but showing a ring there before the user has touched the
    /// keyboard would point at a button they never chose. The ring appears on
    /// the first Tab or the first press on a control, and stays from then on.
    pub(super) focus_visible: bool,
    /// The control the left button was pressed on, until it is released.
    ///
    /// A press arms a control and the matching release fires it; a release
    /// somewhere else fires nothing. Without this the window acted purely on
    /// the release and activated whatever the pointer happened to be over at
    /// the time, so pressing OK, thinking better of it and sliding onto Cancel
    /// pressed Cancel. `HotspotId::NOTHING` for a press on bare background, which is
    /// still a press that a release must match — it is how an open list learns
    /// to close.
    ///
    /// Set only for presses a release is going to resolve. The two presses
    /// that become drags instead — the scrollbar thumb and a text field —
    /// clear it as they take the gesture, so a release that merely ends a drag
    /// cannot also be read as an activation of whatever it happens to be over.
    pub(super) pressed: Option<HotspotId>,
    /// Option the pointer is currently over, so a move that stays within the
    /// same option does not queue an event on every pixel.
    pub(super) hovered: Option<usize>,
    /// Id of the button the pointer is currently over, or `None`.
    ///
    /// Separate from `hovered` (which is a list option) because buttons exist
    /// in both windows and outside any open list. The window repaints only
    /// when this *changes* — that is, when the pointer crosses a button's
    /// edge, not on every move within one — so hover feedback costs one
    /// repaint on entry and one on exit, never one per mouse-move message.
    pub(super) hovered_button: Option<HotspotId>,
    /// Whether a `TrackMouseEvent` is armed to deliver `WM_MOUSELEAVE`.
    ///
    /// `TrackMouseEvent` is one-shot: it fires a single leave message and
    /// disarms. Re-arming on every mouse move would be a syscall per pixel, so
    /// it is armed once when the pointer first enters the client area and this
    /// records that it is live; the leave message clears it, ready to arm again
    /// on the next entry.
    pub(super) leave_tracked: bool,
    /// The last frame as it was drawn, or `None` when nothing has been drawn
    /// since the window was created or since a pass gave up before drawing.
    ///
    /// See [`Painted`] for why this is one value and not three fields.
    pub(super) painted: Option<Painted>,
    /// What the left button is currently dragging, if anything.
    ///
    /// One field rather than two `Option`s. The thumb drag and the text
    /// selection drag are mutually exclusive — the press that starts either
    /// one returns before the other can be considered — but nothing said so,
    /// and `WM_LBUTTONUP` had to check them in the right order and take
    /// whichever was set. A state with both set would have consumed one
    /// release and left the other holding capture for the rest of the
    /// session; the type permitted it, so the exclusion lived in the reading
    /// order of two `if`s. Now it is the shape of the value.
    pub(super) gesture: Gesture,
    /// Characters typed since the owner last collected them, paired with the
    /// field they were aimed at. Edits are applied by the owner rather than
    /// here, so the window procedure never reaches into application state.
    pub(super) typed: Vec<(HotspotId, Edit)>,
    /// Wheel movement that has not yet added up to a whole notch.
    ///
    /// `WHEEL_DELTA` is 120 per detent on a notched wheel, and every message
    /// used to be divided by it and the remainder thrown away. Precision
    /// touchpads and free-spinning wheels report smaller steps — 40 is common —
    /// so integer division gave zero every time and the list did not move at
    /// all, however long the user scrolled. Windows documents the accumulation
    /// this field exists for: keep the remainder between messages and act when
    /// it reaches a whole notch.
    ///
    /// Per window rather than global, because two windows of this class can be
    /// open at once and a partial scroll of one is not a partial scroll of the
    /// other.
    pub(super) wheel_remainder: i32,
    /// Whether the caret is currently in its visible blink phase. Flipped by
    /// the blink timer; only consulted while a field has focus.
    pub(super) caret_visible: bool,
    /// What kind of window this is, so `wnd_proc` can compute a frame
    /// allowance for the styles this window was actually created with.
    /// The window handle carries the same fact in its type, but
    /// `WM_DPICHANGED` arrives at the window procedure, which has no handle
    /// to ask.
    pub(super) role: Role,
    /// The title-bar icons this window rendered for itself, `[small, big]`.
    /// `WM_SETICON` stores the handle rather than copying it, so the window owns
    /// these and must destroy the previous pair before setting a new one —
    /// otherwise every theme or DPI change leaked two `HICON`s. A slot is `None`
    /// only when rendering failed and `set_window_icon` fell back to the exe's
    /// icon resource, which the module owns and the window must not destroy.
    pub(super) window_icons: [Option<HICON>; 2],
}

impl WindowState {
    /// Where the controls of the last painted frame are, in the order they
    /// were drawn.
    ///
    /// An empty slice when no frame has been drawn, which is the honest answer
    /// rather than a convenience: nothing is on screen, so no click can land on
    /// anything. Every hit test in the window procedure goes through here, so
    /// there is one place that decides what "not painted yet" means.
    pub(super) fn hotspots(&self) -> &[Hotspot] {
        self.painted.as_ref().map_or(&[], |p| p.hotspots.as_slice())
    }

    /// The open list exactly as the last pass drew it, or `None` when it drew
    /// none.
    pub(super) fn painted_list(&self) -> Option<&PaintedList> {
        self.painted.as_ref().and_then(|p| p.list.as_ref())
    }

    /// The focused field's value box as last painted, for the blink timer to
    /// invalidate.
    pub(super) fn caret_rect(&self) -> Option<Rect> {
        self.painted.as_ref().and_then(|p| p.caret_rect)
    }

    /// Resolves a click at client `x` to a caret index in field `id`.
    ///
    /// Answered from the frame on screen, so the ruler and the rectangle it is
    /// measured from are the same generation. `0` when no frame has been drawn
    /// or the frame drew no such field — see [`Painted::caret_at`].
    pub(super) fn caret_at(&self, id: HotspotId, x: i32) -> usize {
        self.painted
            .as_ref()
            .and_then(|p| p.caret_at(id, x))
            .unwrap_or(0)
    }

    /// Which dropdown has its list open, as of the last paint.
    ///
    /// The window's copy of [`Painted::open_list`], which is where the answer
    /// is defined: a frame that drew no list has none open, and a window that
    /// has drawn no frame is in the same position.
    pub(super) fn open_list(&self) -> Option<HotspotId> {
        self.painted.as_ref().and_then(Painted::open_list)
    }

    /// How far the list `id` can scroll: the number of its options that do
    /// not fit on screen at once.
    ///
    /// The drag needs this to turn a pointer position into a scroll offset,
    /// and it has to get it from the rows rather than from the owner: the
    /// window procedure never reaches into application state, and this is the
    /// one place a mouse message needs to know something about the list's
    /// contents. The rows are already here and are the same rows the painter
    /// measured, so the arithmetic cannot disagree with what is on screen.
    ///
    /// One number, not the `(total, visible)` pair this used to return.
    /// [`ScrollbarGeometry::scroll_at`] used only their difference, and a pair
    /// travelling to a subtraction is a pair that can arrive the wrong way
    /// round — which, being `usize`, wraps in release rather than failing.
    /// `saturating_sub` is the honest spelling of the same question here: a
    /// list that fits entirely has nowhere to scroll, which is a span of zero.
    pub(super) fn list_span(&self, id: HotspotId) -> Option<usize> {
        self.content.rows.iter().find_map(|r| match r {
            Row::Dropdown {
                options, id: rid, ..
            } if *rid == id => Some(last_scroll_offset(options.len())),
            _ => None,
        })
    }

    /// The focused control when it is a text field, `None` otherwise.
    ///
    /// The three places that care about typing — the caret keys, `WM_CHAR`
    /// and the blink timer — ask this rather than the raw `focus`, so "a field
    /// is focused" is decided once instead of three times.
    pub(super) fn focused_field(&self) -> Option<HotspotId> {
        self.focus.filter(|id| self.is_field(*id))
    }

    /// What kind of control `id` is, asked of the rows through
    /// [`crate::ui::focus`].
    ///
    /// Read from `content.rows` — the rows that will be drawn next — and not
    /// from [`Self::painted`], deliberately. What kind of control an id names
    /// is a property of the model, and a row that has just been rebuilt is the
    /// model. The questions that must be answered from the picture instead are
    /// the geometric ones — where a control is, where its caret goes — and
    /// those are the ones the painted frame answers.
    ///
    /// Which control holds focus is one fact, and what kind of control it is
    /// follows from the rows — not from a second flag here and not from the
    /// hotspots, which do not exist until the window has painted. These three
    /// forward rather than answer so that every such question in the program
    /// has one implementation.
    pub(super) fn is_field(&self, id: HotspotId) -> bool {
        crate::ui::focus::is_field(&self.content.rows, id)
    }

    pub(super) fn is_dropdown(&self, id: HotspotId) -> bool {
        crate::ui::focus::is_dropdown(&self.content.rows, id)
    }

    pub(super) fn is_button(&self, id: HotspotId) -> bool {
        crate::ui::focus::is_button(&self.content.rows, id)
    }
}

#[cfg(test)]
mod tests {
    //! Tests for what the window remembers about the frame on screen.
    //!
    //! [`Painted`] is the one part of the paint path that needs no device: it
    //! is a record, and every question asked of it is arithmetic over that
    //! record. That is why these are here rather than in [`super::paint`].

    use super::*;
    use crate::testsupport::hot;

    /// A frame that drew one field, with a ruler of `ends`.
    fn frame_with(id: HotspotId, text_left: i32, ends: Vec<i32>) -> Painted {
        Painted {
            hotspots: Vec::new(),
            caret_rect: None,
            list: None,
            fields: vec![PaintedField {
                id,
                text_left,
                ends,
            }],
        }
    }

    /// A click puts the caret at the boundary nearest the pointer.
    ///
    /// Nine mutations lived in these six lines and no test noticed any of
    /// them: the subtraction that moves the click into the field's own
    /// coordinates could become an addition, the midpoint could become a
    /// product, the comparison could invert. Every one of those still returns
    /// *a* caret position, which is why nothing failed — a caret one character
    /// off is a bug reported as "typing goes in the wrong place", not as a
    /// crash.
    ///
    /// Three characters ten pixels wide, drawn from x = 100, so the midpoints
    /// are at 105, 115 and 125 and each assertion below names the side of one
    /// of them.
    #[test]
    fn a_click_lands_at_the_nearest_character_boundary() {
        let id = hot(1);
        let frame = frame_with(id, 100, vec![10, 20, 30]);

        // Left of the text, and exactly at its left edge.
        assert_eq!(frame.caret_at(id, 0), Some(0));
        assert_eq!(frame.caret_at(id, 100), Some(0));

        // Either side of the first character's midpoint.
        assert_eq!(frame.caret_at(id, 105), Some(0));
        assert_eq!(frame.caret_at(id, 106), Some(1));

        // And of the second's.
        assert_eq!(frame.caret_at(id, 115), Some(1));
        assert_eq!(frame.caret_at(id, 116), Some(2));

        // Past the last character: after all of them.
        assert_eq!(frame.caret_at(id, 400), Some(3));

        // A field this frame did not draw. The caller puts the caret at the
        // start, but that decision is the caller's and this must say `None`.
        assert_eq!(frame.caret_at(hot(2), 110), None);
    }

    /// An empty field accepts a click and answers zero.
    ///
    /// The loop has nothing to iterate, so the answer comes from the line
    /// after it. Worth its own case because that line is the only one the
    /// walk above never reaches.
    #[test]
    fn a_click_in_an_empty_field_is_the_start_of_it() {
        let id = hot(3);
        let frame = frame_with(id, 40, Vec::new());
        assert_eq!(frame.caret_at(id, 0), Some(0));
        assert_eq!(frame.caret_at(id, 40), Some(0));
        assert_eq!(frame.caret_at(id, 900), Some(0));
    }
}
