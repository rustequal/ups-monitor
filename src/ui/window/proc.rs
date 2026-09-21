use std::cell::Cell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, SetWindowPos, SC_MINIMIZE, SWP_NOACTIVATE, SWP_NOZORDER, WHEEL_DELTA,
    WINDOWPOS, WM_CAPTURECHANGED, WM_CHAR, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCDESTROY, WM_PAINT, WM_SYSCOMMAND, WM_SYSKEYDOWN, WM_TIMER, WM_WINDOWPOSCHANGING,
};

use super::paint::paint;
use super::state::{with_state, with_state_access, Access, Gesture, Hotspot, WindowState};
use super::{
    rebuild_content_and_frame, reset_caret_blink, set_window_icon, stop_caret_blink, MODAL_OWNER,
    WM_MOUSELEAVE,
};
use crate::ui::gdi::PaintDc;
use crate::ui::rect::Rect;
use crate::ui::row::{Edit, HotspotId, Move};
use crate::ui::text::fonts;
use crate::ui::Dpi;
/// What a message handler asks the window to do once it has finished deciding.
///
/// The point of the type is the separation it forces. A handler used to be an
/// arm of `wnd_proc` that read state, changed it, and called `InvalidateRect`,
/// `SetCapture` and `KillTimer` in between — so the only way to find out what a
/// click on a checkbox does was to open a window and click one. Returning
/// effects instead of performing them makes the decision a value, and a value
/// can be asserted on: `on_lbutton_down` is now a function from a state and a
/// point to a new state and a list of effects, with no window anywhere in it.
///
/// The boundary is drawn at *input*. Mouse and keyboard messages go through
/// handlers; `WM_PAINT`, `WM_DPICHANGED`, `WM_CLOSE`, `WM_SYSCOMMAND` and
/// `WM_DESTROY` stay in `wnd_proc` and call Win32 directly, because they are
/// not decisions about state that happen to touch the window — they *are*
/// window operations, and wrapping "paint the window" in an effect would name
/// the same call twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// Repaint the whole client area.
    Invalidate,
    /// Repaint one rectangle — the caret's value box, and nothing else.
    ///
    /// Its own variant rather than a parameter of [`Effect::Invalidate`],
    /// because the distinction is load-bearing: the blink fires twice a second
    /// while a field is focused, and repainting the window for a one-pixel line
    /// would re-measure and redraw every row each time.
    InvalidateRect(Rect),
    /// Hold the mouse until the button is released, so a drag that leaves the
    /// window keeps arriving.
    Capture,
    /// Give the mouse back.
    ReleaseCapture,
    /// Arm `TrackMouseEvent`, so `WM_MOUSELEAVE` arrives when the pointer goes.
    TrackMouseLeave,
    /// Show the caret solid and restart its interval from zero.
    ResetCaretBlink,
    /// Stop the caret timer.
    StopCaretBlink,
    /// Ask the message loop for a pass, so the owner sees what just changed.
    WakeUi,
}

/// Carries out what a handler decided.
///
/// The only place the input path talks to Win32. Order is the order the handler
/// listed them in: a handler that both captures the mouse and asks for a
/// repaint means both, and neither depends on the other.
///
/// Safe: an `Effect` is a value a handler produced, and `hwnd` names a window
/// to the system rather than being read through. The unsafety is in the
/// individual calls, which is where it is now marked.
fn apply(hwnd: HWND, effects: &[Effect]) {
    for effect in effects {
        match effect {
            Effect::Invalidate => {
                // SAFETY: `hwnd` names a window to the system, which validates it; a whole-
                // window invalidation carries no pointer.
                let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
            }
            Effect::InvalidateRect(rect) => {
                // SAFETY: as above, plus `rect` which is a live borrow of the effect.
                let _ = unsafe { InvalidateRect(Some(hwnd), Some(&(*rect).into()), false) };
            }
            Effect::Capture => {
                // SAFETY: `hwnd` names a window to the system.
                unsafe { SetCapture(hwnd) };
            }
            Effect::ReleaseCapture => {
                // SAFETY: releases whatever this thread captured; no arguments.
                let _ = unsafe { ReleaseCapture() };
            }
            Effect::TrackMouseLeave => {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: `track` is a live local with its `cbSize` set above.
                let _ = unsafe { TrackMouseEvent(&mut track) };
            }
            Effect::ResetCaretBlink => reset_caret_blink(hwnd),
            Effect::StopCaretBlink => stop_caret_blink(hwnd),
            Effect::WakeUi => crate::ui::tray_window::wake_ui(),
        }
    }
}

/// Runs `handler` against this window's state and carries out what it decided.
///
/// One `with_state` per message, which is the other half of what the handlers
/// bought: the press arm used to take the state four separate times, and each
/// re-entry was a moment where the state it had just examined could have been
/// replaced by a nested message.
///
/// Safe for the same reason [`apply`] is: the handler runs against a borrow
/// [`with_state`] proved live, and what comes back is a list of values.
fn handle(hwnd: HWND, handler: impl FnOnce(&mut WindowState) -> Vec<Effect>) -> LRESULT {
    let effects = with_state(hwnd, handler).unwrap_or_default();
    apply(hwnd, &effects);
    LRESULT(0)
}

/// The panel's window procedure, called by Windows and by nothing else.
///
/// # Safety
///
/// `unsafe` here is not a choice: a window procedure is called by the system
/// with arguments it constructs, so the obligation belongs to Windows and the
/// keyword only records that. Two messages carry a pointer in `lparam` —
/// `WM_WINDOWPOSCHANGING` a `WINDOWPOS`, `WM_DPICHANGED` a suggested `Rect` —
/// and each is checked for null before it is read.
pub(super) unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_CLOSE: u32 = 0x0010;

    match msg {
        // Background is painted by the double buffer, so erasing first would
        // only flicker.
        WM_ERASEBKGND => LRESULT(1),

        // SAFETY: forwards the message the system delivered, unchanged.
        WM_PAINT => unsafe { on_paint(hwnd, msg, wparam, lparam) },

        // Caret blink. Flips the visible phase and repaints only the field's
        // value box, not the whole window: the timer fires a couple of times
        // a second while a field is focused, and invalidating everything would
        // re-measure and redraw every row for a one-pixel line.
        WM_TIMER if wparam.0 == crate::ui::timers::CARET => handle(hwnd, on_caret_tick),

        // Losing the window's focus stops the blink: no field can be edited
        // while another window is active, so the timer has nothing to drive.
        WM_KILLFOCUS => handle(hwnd, |_| vec![Effect::StopCaretBlink]),

        // Pressing the thumb begins a drag. Only the thumb: every other
        // control in this window acts on release, which is what lets a user
        // press, think better of it, slide off and let go without triggering
        // anything. A thumb cannot work that way — it has to follow the
        // pointer while the button is held — so it is the one place that
        // takes the press.
        //
        // `WM_LBUTTONDBLCLK` is handled here and not apart, because a double
        // click's second press *is* a press. The class carries `CS_DBLCLKS`
        // for the sake of one control — a text field selects itself whole on
        // a double click — and the style applies to the whole window: every
        // second press landing within `GetDoubleClickTime` of the one before
        // is relabelled, wherever it lands. Handled in its own arm, as it
        // was, that press armed no control and its release therefore
        // activated none, so a checkbox clicked at speed toggled on every
        // other click while the same checkbox driven by the space bar toggled
        // on every press. The relabelling is Windows' business; only the
        // field reads anything into it.
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let at = POINT {
                x: i32::from((lparam.0 & 0xFFFF) as i16),
                y: i32::from(((lparam.0 >> 16) & 0xFFFF) as i16),
            };
            let double = msg == WM_LBUTTONDBLCLK;
            // SAFETY: reads one virtual key's state by code.
            let shift = (unsafe { GetKeyState(0x10) } as u16 & 0x8000) != 0;
            handle(hwnd, |state| {
                on_lbutton_down(state, at, double, shift, |state, id, x| {
                    state.caret_at(id, x)
                })
            })
        }

        // Capture lost to someone else. The gesture is over whether or not a
        // release ever arrives.
        //
        // Windows takes the capture away without asking: a `MessageBox` opening
        // on another thread, the task switcher, an accessibility tool, or
        // anything that calls `SetCapture` itself. `WM_LBUTTONUP` then never
        // reaches this window, so the gesture stayed set — and every later move
        // over the window was read as a continuing drag. The thumb followed a
        // pointer with no button held; a text field selected under the same
        // pointer. Nothing recovered it short of another press and release on
        // the same control.
        //
        // No `ReleaseCapture` here: the capture is already gone, and calling it
        // would only generate a second `WM_CAPTURECHANGED`. Clearing the
        // gesture is idempotent, so the message arriving as a *result* of the
        // release path's own `ReleaseCapture` costs nothing.
        WM_CAPTURECHANGED => handle(hwnd, on_capture_lost),

        WM_LBUTTONUP => {
            let at = POINT {
                x: i32::from((lparam.0 & 0xFFFF) as i16),
                y: i32::from(((lparam.0 >> 16) & 0xFFFF) as i16),
            };
            handle(hwnd, |state| on_lbutton_up(state, at))
        }

        // Printable characters, and Backspace, go to the focused field. Only
        // those: every key that means something to the window rather than to
        // the text — Tab, Enter, Escape, Space on anything but a field — is
        // read in `WM_KEYDOWN`, where the control it acts on is known.
        //
        // Those keys still arrive here as control characters, and are dropped
        // by the same filter that has always dropped them: handling them in
        // both places would act on them twice.
        WM_CHAR => handle(hwnd, |state| on_char(state, wparam.0 as u32)),

        // The pointer moving over an open list moves the highlight with it,
        // so the mouse and the arrow keys drive the same indicator rather
        // than two competing ones. Without this the highlight sits wherever
        // the keyboard left it while the pointer hovers somewhere else, and
        // the list gives no feedback about what a click would choose.
        WM_MOUSEMOVE => {
            let at = POINT {
                x: i32::from((lparam.0 & 0xFFFF) as i16),
                y: i32::from(((lparam.0 >> 16) & 0xFFFF) as i16),
            };
            handle(hwnd, |state| {
                on_mouse_move(state, at, WindowState::caret_at)
            })
        }

        // The pointer left the client area. Clear any button hover so a button
        // does not stay lit after the pointer is gone, and disarm the tracker
        // (it is one-shot and has already fired). Repaint only the button that
        // was lit, if any.
        WM_MOUSELEAVE => handle(hwnd, on_mouse_leave),

        // The wheel scrolls an open list. Ignored when no list is open: the
        // panel and the dialog both size themselves to their content, so
        // there is nothing else here that scrolls.
        WM_MOUSEWHEEL => {
            let delta = i32::from(((wparam.0 >> 16) & 0xFFFF) as i16);
            handle(hwnd, |state| on_mouse_wheel(state, delta))
        }

        // Escape closes the window even with nothing focused, matching what
        // every other Windows dialog does.
        // Every key that is not a printable character: the keys an open list
        // claims, navigation between controls, activation of the focused one,
        // and caret movement inside a focused field — in that order, because
        // that is the order of specificity. An open list is the innermost
        // thing on screen, a field is the next, and the rest belong to the
        // window.
        WM_KEYDOWN => {
            // `GetKeyState`'s high bit is set while the key is held.
            let held = Modifiers {
                // SAFETY: reads one virtual key's state by code.
                shift: (unsafe { GetKeyState(0x10) } as u16 & 0x8000) != 0,
                // SAFETY: as above.
                ctrl: (unsafe { GetKeyState(0x11) } as u16 & 0x8000) != 0,
            };
            handle(hwnd, |state| on_key_down(state, wparam.0, held))
        }

        // Alt+Down opens the focused dropdown: the binding Windows dialogs
        // offer beside Space, and the one users who came from them reach for.
        //
        // Alt makes it a *system* key, so it arrives here and not at
        // `WM_KEYDOWN`. Everything else is passed to the default handler,
        // which is what keeps Alt+F4 and the window menu working.
        WM_SYSKEYDOWN => {
            let opened = with_state(hwnd, |state| on_syskey_down(state, wparam.0));
            match opened {
                Some(effects) if !effects.is_empty() => {
                    apply(hwnd, &effects);
                    LRESULT(0)
                }
                // Everything else is passed to the default handler, which is
                // what keeps Alt+F4 and the window menu working.
                // SAFETY: forwards the arguments the system passed in, unchanged.
                _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
        }

        // Closing hides to the tray. The owner sees the flag, drops the Panel,
        // and the window is destroyed for real.
        //
        // The flag is not a message, and `GetMessage` only returns for real
        // messages, so the loop is nudged explicitly. Without this the click
        // on "×" sat in the flag until some unrelated message happened along
        // — up to a full second, which reads to the user as a window that
        // takes a second to close.
        WM_CLOSE => {
            with_state(hwnd, |state| state.close_requested = true);
            crate::ui::tray_window::wake_ui();
            LRESULT(0)
        }

        // Clicking the taskbar button of the active window asks Windows to
        // minimise it, which arrives here as SC_MINIMIZE. Left to the default
        // handler the panel shrinks to a taskbar button — a second place the
        // window lives, alongside the tray icon that is supposed to be the
        // only one.
        //
        // Routed to `close_requested` instead, which is what the "×" already
        // uses and which the owner reads as "hide to the tray". So the
        // taskbar button, the close box and the tray icon all lead to the
        // same state, and the taskbar gets the show/hide toggle every other
        // application has.
        //
        // The low four bits are masked off: Windows uses them internally for
        // the command's accelerator, so a raw equality against SC_MINIMIZE
        // misses the click on some paths.
        WM_SYSCOMMAND => {
            if (wparam.0 & 0xFFF0) == SC_MINIMIZE as usize {
                with_state(hwnd, |state| state.close_requested = true);
                crate::ui::tray_window::wake_ui();
                return LRESULT(0);
            }
            // SAFETY: forwards the arguments the system passed in, unchanged.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        // A disabled owner still receives WM_WINDOWPOSCHANGING when another
        // application or the shell raises it. Stripping the Z-order change
        // is what stops the panel from surfacing above the settings dialog:
        // EnableWindow blocks input, but not restacking.
        WM_WINDOWPOSCHANGING => {
            if MODAL_OWNER.with(Cell::get) == Some(hwnd) {
                let pos = lparam.0 as *mut WINDOWPOS;
                if !pos.is_null() {
                    // SAFETY: `pos` is non-null, checked immediately above, and Windows
                    // documents `lparam` for this message as a `WINDOWPOS` it owns for the
                    // duration of the call.
                    unsafe { (*pos).flags |= SWP_NOZORDER };
                }
            }
            // SAFETY: forwards the arguments the system passed in, unchanged.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        // The window has moved to a monitor of a different DPI — dragged
        // there, or shown for the first time on a secondary monitor whose
        // DPI was not known until now. Per-monitor awareness is what
        // delivers this message at all; without it (the old system-aware
        // declaration) this arm would simply never fire, and the window
        // would carry on drawing at whatever DPI it started at, which is
        // the blurred-text failure this feature exists to close.
        //
        // The suggested rectangle Windows passes in `lparam` is a *linear*
        // scale of the window's previous pixel size — new DPI over old,
        // applied to whatever the frame happened to be — not a re-measure
        // of the content. That is correct for windows sized by their frame
        // alone, but this one is sized by wrapped text and column widths
        // that do not scale linearly in general (a label that wrapped to
        // two lines at one width may fit one line at another). Only its
        // *position* is taken from Windows; the *size* is `rebuild_content_
        // and_frame`'s honest re-measure at the new DPI, the same call
        // `Window::create` makes before this window is ever shown, so a
        // window's size means the same thing whichever path produced it.
        WM_DPICHANGED => {
            // SAFETY: the parameters are the ones the system passed to this
            // window procedure, forwarded unchanged.
            unsafe { on_dpi_changed(hwnd, msg, wparam, lparam) }
        }

        WM_DESTROY => LRESULT(0),

        // The last message a window ever receives, and therefore the last
        // chance to give back what it owns. `Window::drop` normally got there
        // first, in which case this finds nothing and returns — but it is not
        // the only way a window of this class dies. A modal is created with an
        // owner, and Windows destroys owned windows along with their owner, so
        // a panel dropped before its modal takes the modal's window with it and
        // leaves that `Window` holding a dead `HWND`. Releasing here closes that
        // path, the same way `tray_window.rs` closes it for the hidden window.
        WM_NCDESTROY => {
            super::release_window_state(hwnd);
            LRESULT(0)
        }

        // SAFETY: forwards the arguments the system passed in, unchanged.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Moves keyboard focus one step around the ring — Tab, or Shift+Tab when
/// `back` is set — and makes the ring visible.
///
/// Everything a focus move implies happens here rather than at the call site:
/// an open list is dismissed, the ring is shown, a field arriving in focus has
/// its whole value selected, and the caret blink starts or stops with the kind
/// of control now focused. Written once because the two directions differ by
/// one bool and nothing else.
fn step_focus(state: &mut WindowState, back: bool) -> Vec<Effect> {
    let Some(next) = crate::ui::focus::step(&state.content.rows, state.focus, back) else {
        return Vec::new();
    };
    // Tab out of an open list leaves it as it was found. It must not choose:
    // the highlight is wherever arrowing left it, and committing that on the
    // way past would change a setting the user was only looking at.
    if let Some(open) = state.open_list() {
        state.typed.push((open, Edit::Cancel));
    }
    state.focus = Some(next);
    state.focus_visible = true;
    let field = state.is_field(next);
    if field {
        // Arriving by keyboard selects the whole value: the field was reached
        // in order to be replaced, so the first digit typed should replace it
        // rather than extend what is there. Arriving by mouse does not — a
        // press places the caret where it landed, which is what a pointer aimed
        // at one character means.
        state.typed.push((next, Edit::SelectAll));
    }
    let blink = if field {
        Effect::ResetCaretBlink
    } else {
        Effect::StopCaretBlink
    };
    vec![blink, Effect::Invalidate, Effect::WakeUi]
}

/// Records an activation of control `id` — a keyboard press of Space or Enter.
///
/// It reports the same `clicked` a mouse release does, deliberately: a
/// checkbox toggled from the keyboard runs the code that toggles it from the
/// pointer, a dropdown opened by Space runs the code that opens it by click.
/// There is no second meaning of "activate" anywhere for the two to drift
/// apart on, and the owner never learns which device the event came from.
fn activate(state: &mut WindowState, id: HotspotId) -> Vec<Effect> {
    state.clicked = Some(id);
    state.focus_visible = true;
    vec![Effect::Invalidate, Effect::WakeUi]
}

/// Flips the caret's visible phase, and says what to repaint.
///
/// No focused field means the timer should not be running: nothing is flipped
/// and nothing is repainted until it is stopped. The rectangle is the field's
/// value box, recorded by the painter, so the blink costs one small repaint
/// rather than a whole window twice a second.
fn on_caret_tick(state: &mut WindowState) -> Vec<Effect> {
    if state.focused_field().is_none() {
        return Vec::new();
    }
    state.caret_visible = !state.caret_visible;
    match state.caret_rect() {
        Some(rect) => vec![Effect::InvalidateRect(rect), Effect::WakeUi],
        None => Vec::new(),
    }
}

/// The window moved to a monitor with a different scale factor.
///
/// Not routed through [`handle`] like its neighbours: it returns the system's
/// own answer when the message carries no suggested rectangle, and it calls
/// `set_window_icon` after the state borrow has been released rather than
/// inside it.
///
/// # Safety
///
/// Called only from [`wnd_proc`], with the parameters the system passed for
/// `WM_DPICHANGED`: `lparam` is read as the suggested `Rect` the message
/// documents, and `msg`, `wparam` and `lparam` are forwarded unchanged when
/// there is nothing to act on.
unsafe fn on_dpi_changed(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Both halves of `wparam` carry the new DPI (x and y are always
    // equal on Windows); LOWORD is enough.
    let new_dpi = Dpi::new((wparam.0 & 0xFFFF) as u32);
    let suggested = lparam.0 as *const Rect;
    if suggested.is_null() {
        // SAFETY: forwards the arguments the system passed in, unchanged.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `suggested` is non-null, checked immediately above, and Windows
    // documents `lparam` for this message as the suggested `Rect`.
    let (x, y) = unsafe { ((*suggested).left, (*suggested).top) };

    // `base_theme` is cloned out rather than read straight from
    // `state` after this closure returns: `set_window_icon` needs
    // it, and calling `set_window_icon` — which itself calls
    // `with_state` — from *inside* this `with_state` closure would
    // be a second borrow of the same `RefCell` while the first is
    // still held. `with_state` guards that with `try_borrow_mut`
    // rather than panicking, so the inner call would not crash —
    // it would just silently do nothing, which is exactly how the
    // title-bar icon went stale on a DPI change and nowhere else:
    // no error, no test failure, just a title bar that stopped
    // updating. The rebuild happens here, inside the one borrow;
    // the icon is set afterwards, outside it.
    let rebuilt = with_state(hwnd, |state| {
        if state.dpi == new_dpi {
            // Spurious or redundant delivery: nothing to rebuild.
            return None;
        }
        let rows = std::mem::take(&mut state.content.rows);
        // The samples travel with the rows: the new DPI must answer the same
        // question the old one did, and a re-measure without them would size
        // the window to whatever it happens to be showing at that moment.
        let samples = std::mem::take(&mut state.content.samples);
        let (content, size) = rebuild_content_and_frame(
            rows,
            samples,
            &state.base_theme,
            state.family,
            state.min_width,
            new_dpi,
            state.role,
        );
        state.content = content;
        state.theme = state.base_theme.scaled(new_dpi);
        state.dpi = new_dpi;
        // Old-DPI handles are not freed: `SCRIPT_FONTS` is a
        // process-lifetime cache keyed by `(family, points, dpi)`,
        // exactly like every other entry in it (see the note on
        // `SCRIPT_FONTS`), so a monitor the window returns to later
        // is served from cache rather than paying `CreateFontW`
        // again. This is the same trade the cache already makes for
        // every language switch; DPI is one more axis of the same
        // key, not a new lifetime to manage.
        let (font, font_bold) = fonts(super::script_of(&state.base_theme, state.family, new_dpi));
        state.font = font;
        state.font_bold = font_bold;
        Some((size, state.base_theme))
    })
    .flatten();

    if let Some(((w, h), base_theme)) = rebuilt {
        let _ =
            // SAFETY: `hwnd` is the window this message is about; every coordinate is
            // by value.
            unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) };
        // The icon is sized from `state.dpi` (`set_window_icon`
        // reads it fresh), which the closure above already moved
        // to `new_dpi` — so this picks up the new pixel size, not
        // the one the window opened with. Without this call the
        // bitmap set at the old DPI stays exactly as it was, and
        // the title bar — drawn by the DWM, which does know the
        // real DPI — stretches it to fit: the same soft-edge
        // symptom `set_window_icon`'s own `GetSystemMetricsForDpi`
        // fix closes at open time, left open here at runtime.
        set_window_icon(hwnd, &base_theme);
        // SAFETY: `hwnd` is the window this message is about.
        let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
    }
    LRESULT(0)
}

/// Draws the window, or declines in the way the reason calls for.
///
/// Its own function rather than an arm, like [`on_dpi_changed`] and for the
/// same reason: it does not decide something about state and then return
/// effects, it operates the window directly, and the order of its two Win32
/// calls is the whole of what it has to say.
///
/// # Safety
///
/// Called only from [`wnd_proc`], with the parameters the system passed for
/// `WM_PAINT`, which are forwarded unchanged when there is nothing to draw.
unsafe fn on_paint(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The state is taken *before* `BeginPaint`, and that order is the whole of
    // this function.
    //
    // `BeginPaint` validates the update region whether or not anything is
    // drawn. Called first, a pass that then failed to borrow the state drew
    // nothing and told Windows the window was up to date, so the frame was not
    // merely skipped — it was never drawn at all, until something unrelated
    // invalidated the window again. A missed repaint is a defect, not a
    // defence.
    //
    // The two ways a borrow can fail need opposite answers, which is why this
    // asks for [`Access`] rather than an `Option`. `Busy` is temporary: the
    // region is left dirty and the frame arrives with the next message.
    // `Absent` means the window has no state and never will again, so the
    // region is validated by `DefWindowProcW` — left dirty it would be asked
    // for forever.
    //
    // The client rectangle is measured before the borrow, so the painter is
    // never handed a handle it could reach its own state through.
    let Some(client) = super::client_rect(hwnd) else {
        // SAFETY: forwards the arguments the system passed in, unchanged, so
        // the region is validated by the default handler.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let drawn = with_state_access(hwnd, |state| {
        // SAFETY: this is `hwnd`'s own procedure handling `WM_PAINT`, which is
        // the only place `BeginPaint` is defined. The guard ends the paint on
        // every path out, including a panic during `paint`. Nothing it sends
        // the window — `WM_ERASEBKGND`, `WM_NCPAINT` — reaches the state, so
        // beginning the paint inside the borrow cannot nest it.
        let paint_dc = unsafe { PaintDc::begin(hwnd) };
        paint(paint_dc.canvas(), client, paint_dc.dirty(), state);
    });
    match drawn {
        Access::Ran(()) | Access::Busy => LRESULT(0),
        // SAFETY: forwards the arguments the system passed in, unchanged.
        Access::Absent => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Ends whatever gesture was in progress when the capture went away.
///
/// Windows takes the capture without asking: a `MessageBox` on another thread,
/// the task switcher, an accessibility tool. `WM_LBUTTONUP` then never reaches
/// this window, so a gesture left set made every later move over the window
/// read as a continuing drag — the thumb followed a pointer with no button
/// held, a text field selected under it — and nothing recovered short of
/// another press and release on the same control.
///
/// No `ReleaseCapture` in the answer: the capture is already gone, and asking
/// for it would only produce a second `WM_CAPTURECHANGED`. Clearing the gesture
/// is idempotent, so this message arriving as a *result* of the release path's
/// own release costs nothing.
fn on_capture_lost(state: &mut WindowState) -> Vec<Effect> {
    state.gesture = Gesture::None;
    Vec::new()
}

/// Applies a press of the left button at `at`.
///
/// `caret_at` resolves a pixel offset inside a text field to a character index.
/// It is a parameter because that is the one thing here that cannot be decided
/// from state alone — it needs a device context carrying the window's font, so
/// that the pixel-to-character mapping agrees with what was drawn. Passing it
/// in is what leaves the rest of this function testable: a test supplies an
/// index and asserts on the edit that comes out.
///
/// Three kinds of press, in the order they are ruled out:
///
/// 1. **The scrollbar thumb**, which begins a drag. The only control in the
///    window that acts on press. Every other one acts on release, which is what
///    lets a user press, think better of it, slide off and let go without
///    triggering anything; a thumb cannot work that way, because it has to
///    follow the pointer while the button is held.
/// 2. **A text field**, which places the caret and may begin a selection drag —
///    also on press, and for the same reason.
/// 3. **Everything else**, which is merely armed: remembered so the release can
///    check that it landed on the same control.
fn on_lbutton_down(
    state: &mut WindowState,
    at: POINT,
    double: bool,
    shift: bool,
    caret_at: impl Fn(&WindowState, HotspotId, i32) -> usize,
) -> Vec<Effect> {
    if let Some(sb) = state.painted_list().and_then(|l| l.scrollbar) {
        let on_thumb = at.x >= sb.thumb.left
            && at.x < sb.thumb.right
            && at.y >= sb.thumb.top
            && at.y < sb.thumb.bottom;
        if on_thumb {
            // The offset within the thumb, so it stays under the point it was
            // grabbed by rather than jumping its top to the cursor.
            //
            // Fixed for the life of the drag and deliberately not re-derived
            // from the thumb on each move. Every move repaints, and the repaint
            // moves the thumb to the position the move just requested — so a
            // grab offset recomputed against the new thumb would be measured
            // from a reference that had already followed the pointer, and the
            // two would chase each other: the list accelerates away under a
            // pointer moving at a constant speed. Held constant, `y - grab`
            // always names the same point on the thumb, and the mapping stays
            // linear.
            state.gesture = Gesture::Thumb {
                grab: at.y - sb.thumb.top,
            };
            // The press has become a drag, so no release is owed an activation
            // for it. See `pressed`: it is set only for presses a matching
            // release resolves.
            state.pressed = None;
            // Captured, or a drag that leaves the window stops receiving moves
            // and the thumb sticks wherever the pointer crossed the edge —
            // while the button is still held, so it resumes from a stale
            // position when the pointer comes back.
            return vec![Effect::Capture];
        }
    }

    // Not the thumb. Every other press arms a control: it takes keyboard focus
    // if the keyboard can reach it, and is remembered so the release can check
    // that it landed on the same one.
    let hit = state
        .hotspots()
        .iter()
        .rev()
        .find(|h| {
            at.x >= h.rect.left && at.x < h.rect.right && at.y >= h.rect.top && at.y < h.rect.bottom
        })
        // A press on bare background is still a press, and the release matching
        // it is what dismisses an open list. Reported as its own id so the
        // owner can tell it from a press that never happened.
        .map_or(HotspotId::NOTHING, |h| h.id);
    state.pressed = Some(hit);
    // Only a control the keyboard can reach takes focus. An option inside an
    // open list and the list's scrollbar are neither — they belong to the
    // dropdown, which keeps the ring while they are used — and bare background
    // moves focus nowhere at all.
    let takes_focus = crate::ui::focus::contains(&state.content.rows, hit);
    let focus_moved = takes_focus && (state.focus != Some(hit) || !state.focus_visible);
    if takes_focus {
        state.focus = Some(hit);
        state.focus_visible = true;
    }

    let mut effects = Vec::new();
    let placed = if state.is_field(hit) {
        // Either way the field has consumed the press, so no release is owed an
        // activation for it.
        state.pressed = None;
        if double {
            // The digit-field analogue of selecting a word: the whole text. No
            // caret to place and no drag to begin — the selection already covers
            // everything the pointer could extend it to.
            state.typed.push((hit, Edit::SelectAll));
            Some(false)
        } else {
            let index = caret_at(state, hit, at.x);
            state.gesture = Gesture::Text { id: hit };
            // Record the press as a click on the field, exactly as the release
            // path records a click on any other control. This is what lets an
            // open dropdown close when the user clicks into the field: closing
            // overlays is the draft's `on_click` job, keyed off the clicked id,
            // and a field press that only pushed a caret edit — as it used to —
            // never delivered an id, so the list stayed open over a field that
            // had plainly taken focus. The release is consumed as the end of a
            // text drag and records nothing further, so this does not
            // double-fire.
            state.clicked = Some(hit);
            state.typed.push((
                hit,
                Edit::CaretTo {
                    index,
                    extend: shift,
                },
            ));
            Some(true)
        }
    } else {
        None
    };

    // Only a caret drag holds the pointer. A double click has already selected
    // everything there is to select, so there is nothing for a drag to extend
    // and no reason to take the capture.
    if placed == Some(true) {
        effects.push(Effect::Capture);
    }
    if placed.is_some() {
        // The blink follows focus: shown solid at once on the press that
        // focuses the field, its interval restarted from zero so the caret is
        // visible in the moment the user clicks.
        effects.push(Effect::ResetCaretBlink);
    } else if state.focused_field().is_none() {
        // The press moved focus to something that is not a field — or landed on
        // background while nothing was in a field. Either way the timer has
        // nothing to drive and must stop, or an idle window goes on waking the
        // loop twice a second.
        effects.push(Effect::StopCaretBlink);
    }
    // Repainted only for what a press actually changes: the ring moving, or a
    // field taking the caret. A press on a button changes nothing until it is
    // released, and a press on bare background changes nothing at all —
    // invalidating for those would double the repaints of every click for no
    // visible difference.
    if focus_moved || placed.is_some() {
        effects.push(Effect::Invalidate);
        effects.push(Effect::WakeUi);
    }
    effects
}

/// Applies a release of the left button at `at`.
///
/// A drag ends here and consumes the release, whichever drag it was.
///
/// *The thumb*: the button pressed on it must not also arrive at whatever
/// hotspot the pointer happens to be over when it comes up, which after a long
/// drag is routinely an option in the list behind it.
///
/// *A text-field selection*: the caret and any selection were already set on
/// the press and updated on every move, so the release has nothing left to do
/// but let go of the capture; letting it fall through to the hit test would
/// re-report the field as a fresh click and, on a click that happened to
/// travel, reset the caret it just placed.
///
/// The gesture is taken as one value, so the two cases cannot be checked in the
/// wrong order or both be pending at once.
fn on_lbutton_up(state: &mut WindowState, at: POINT) -> Vec<Effect> {
    if std::mem::replace(&mut state.gesture, Gesture::None) != Gesture::None {
        return vec![Effect::ReleaseCapture];
    }

    // Only the control the press armed can be activated by this release. A
    // release that landed anywhere else fires nothing, which is what lets a
    // user press a button, think better of it, slide off and let go. Before
    // this the release alone decided, so a press on OK that ended on Cancel
    // pressed Cancel.
    //
    // Focus is not touched here. It was moved by the press, and it stays there
    // whether or not the release activates anything: changing one's mind about
    // pressing a control is not changing one's mind about having selected it.
    let Some(armed) = state.pressed.take() else {
        return Vec::new();
    };
    // Reverse order: hotspots are pushed as they are painted, so the last
    // pushed is the topmost drawn. An open dropdown registers its options after
    // every ordinary row, and hit testing front-to-back would find the checkbox
    // *under* the list instead of the option on top of it.
    let hit = state
        .hotspots()
        .iter()
        .rev()
        .find(|h| {
            at.x >= h.rect.left && at.x < h.rect.right && at.y >= h.rect.top && at.y < h.rect.bottom
        })
        .map_or(HotspotId::NOTHING, |h| h.id);
    if hit != armed {
        return Vec::new();
    }
    // Including `HotspotId::NOTHING`: a press and release on bare background
    // is a click the owner has to hear about, because an open dropdown must close
    // when the user clicks away from it. Reported as a distinct id so the owner
    // can tell "clicked nothing" from "no click happened" without the window
    // procedure needing to know what a dropdown is.
    state.clicked = Some(armed);
    // Checkbox ticks and button fills change on click, so the window must
    // redraw without waiting for the refresh timer; and the click is recorded
    // in a flag rather than a message, so the loop needs a nudge or the button
    // does nothing until the next timer tick.
    vec![Effect::Invalidate, Effect::WakeUi]
}

/// Applies a movement of the pointer to `at`.
///
/// Four things can follow a move, and only one of them ever does — they are
/// ruled out in order, because a drag owns the pointer and a hover must not
/// happen underneath it. Dragging the thumb across the options otherwise drags
/// the highlight along with it: two indicators moving at once, one of which the
/// user is not pointing at.
fn on_mouse_move(
    state: &mut WindowState,
    at: POINT,
    caret_at: impl Fn(&WindowState, HotspotId, i32) -> usize,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    // Arm the one-shot leave notification the first time the pointer enters the
    // client area. `TrackMouseEvent` fires a single `WM_MOUSELEAVE` and
    // disarms, so this is done once per entry, not per move: re-arming on every
    // move would be a syscall per pixel.
    //
    // The flag is set here rather than on the call succeeding. The call is now
    // an effect and its result is not reported back, which is the one thing
    // this costs; the trade is that a failure re-arms on the next move instead
    // of never, and a failure to arm is not a condition that recovers on its
    // own anyway.
    if !state.leave_tracked {
        state.leave_tracked = true;
        effects.push(Effect::TrackMouseLeave);
    }

    if let Gesture::Thumb { grab } = state.gesture {
        // Only when the offset actually changes.
        //
        // A drag delivers a `WM_MOUSEMOVE` for every pixel the mouse reports,
        // but the list moves one option per `item_h` pixels — so roughly nine
        // moves in ten ask for the offset the list is already at. Repainting on
        // all of them rebuilt every row, remeasured all twenty-four languages
        // against GDI and redrew the whole window to produce an image identical
        // to the one on screen. That is where the CPU went: not one expensive
        // repaint, but a hundred a second of redundant ones.
        //
        // `PaintedList` is the painter's own record of what it drew, so this
        // compares against what is on screen rather than against what the
        // owner's draft happens to hold. Read here, never written: the painter
        // is the only writer, because the question is "would a repaint change
        // the picture" and only the painter knows what the picture is.
        let wanted = (|| {
            let list = state.painted_list()?;
            let sb = list.scrollbar?;
            let span = state.list_span(list.id)?;
            let want = sb.scroll_at(at.y - grab, span);
            (list.scroll != want).then_some((list.id, want))
        })();
        if let Some((id, want)) = wanted {
            state.typed.push((id, Edit::ScrollTo(want)));
            // No invalidation here. This used to invalidate the whole client
            // area immediately, and then the owner's fast path invalidated the
            // list a second time — so every drag event queued a full-window
            // repaint *and* a partial one, strictly more work than before the
            // fast path existed. The repaint is the owner's to request: it
            // holds the draft that has to learn the new offset, and it knows
            // which rectangle actually changed.
            effects.push(Effect::WakeUi);
        }
        return effects;
    }

    // A text-field selection drag: the button is held after a press inside a
    // field, so every move extends the selection to the character now under the
    // pointer. `extend` is always true here — the anchor was fixed on the press
    // and must stay put while the far end follows the cursor.
    if let Gesture::Text { id } = state.gesture {
        let index = caret_at(state, id, at.x);
        state.typed.push((
            id,
            Edit::CaretTo {
                index,
                extend: true,
            },
        ));
        // The caret is at the moving end of the selection, so keep it solid
        // while the pointer drags rather than letting it blink out from under
        // the gesture.
        effects.extend([Effect::ResetCaretBlink, Effect::Invalidate, Effect::WakeUi]);
        return effects;
    }

    // The highlight inside an open list follows the pointer. Same reverse walk
    // as the click path: whatever a click here would choose is what the
    // highlight should show.
    let highlighted = (|| {
        let id = state.open_list()?;
        let hit = state.hotspots().iter().rev().find(|h| hits(h, at))?;
        let (base, index) = hit.id.split_option()?;
        (base == id && state.hovered != Some(index)).then_some((id, index))
    })();
    if let Some((id, index)) = highlighted {
        state.hovered = Some(index);
        state.typed.push((id, Edit::Hover(index)));
        effects.push(Effect::WakeUi);
    }

    // Button hover feedback. The topmost hotspot under the pointer is resolved
    // by the same reverse walk as a click, so an open list's options — pushed
    // last — win over any button they cover, and only an actually exposed
    // button lights up.
    //
    // The repaint fires strictly on an edge crossing: the new button id is
    // compared against the stored one, and if they match — the pointer moved
    // but stayed inside the same button — nothing is invalidated. That is the
    // whole point of storing the id rather than reacting to the raw move.
    let now = state
        .hotspots()
        .iter()
        .rev()
        .find(|h| hits(h, at))
        .filter(|h| h.is_button)
        .map(|h| h.id);
    if now != state.hovered_button {
        // Invalidate just the button being left and the one being entered, not
        // the whole client area: only their fills change.
        let previous = state.hovered_button;
        state.hovered_button = now;
        for target in [previous, now].into_iter().flatten() {
            if let Some(h) = state.hotspots().iter().find(|h| h.id == target) {
                effects.push(Effect::InvalidateRect(h.rect));
            }
        }
    }
    effects
}

/// Whether `at` is inside `hotspot`.
///
/// Half-open on the right and bottom, like every other rectangle test in this
/// module: two hotspots that share an edge must not both claim the pixel on it.
fn hits(hotspot: &Hotspot, at: POINT) -> bool {
    at.x >= hotspot.rect.left
        && at.x < hotspot.rect.right
        && at.y >= hotspot.rect.top
        && at.y < hotspot.rect.bottom
}

/// Clears the button hover when the pointer leaves the client area.
///
/// The tracker is one-shot and has already fired, so the flag comes down with
/// it: the next move over the window arms a fresh one. Only the button that was
/// lit is repainted — nothing else changed.
fn on_mouse_leave(state: &mut WindowState) -> Vec<Effect> {
    state.leave_tracked = false;
    let Some(was) = state.hovered_button.take() else {
        return Vec::new();
    };
    state
        .hotspots()
        .iter()
        .find(|h| h.id == was)
        .map(|h| vec![Effect::InvalidateRect(h.rect)])
        .unwrap_or_default()
}

/// Scrolls the open list by whole wheel detents.
///
/// `delta` is in wheel units, of which [`WHEEL_DELTA`] make one detent.
/// Precision touchpads and free-spinning wheels report less than that per step
/// — 40 is common — so the remainder is kept between messages and acted on when
/// it reaches a whole notch. Dividing each message on its own gave zero every
/// time, and the list did not move at all however long the user scrolled.
///
/// With no list open the remainder is deliberately left standing rather than
/// discarded: a slow scroll that crossed a moment with nothing open should not
/// have to start again from nothing.
fn on_mouse_wheel(state: &mut WindowState, delta: i32) -> Vec<Effect> {
    let Some(id) = state.open_list() else {
        return Vec::new();
    };
    let notch = WHEEL_DELTA as i32;
    state.wheel_remainder += delta;
    let notches = state.wheel_remainder / notch;
    state.wheel_remainder -= notches * notch;
    if notches == 0 {
        return Vec::new();
    }
    state.typed.push((id, Edit::Scroll(notches)));
    vec![Effect::WakeUi]
}

/// Which modifier keys were held when a key went down.
///
/// A pair rather than two `bool` parameters, so a caller cannot swap them: the
/// compiler has nothing to say about `on_key_down(state, key, shift, ctrl)`
/// called with the two the wrong way round, and Shift+Left and Ctrl+Left mean
/// different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Modifiers {
    shift: bool,
    ctrl: bool,
}

/// Applies a printable character, or Backspace, to the focused field.
///
/// Only those. Every key that means something to the window rather than to the
/// text — Tab, Enter, Escape, Space on anything but a field — is read in
/// [`on_key_down`], where the control it acts on is known. Those keys still
/// arrive as control characters and are dropped by the filter here: handling
/// them in both places would act on them twice.
fn on_char(state: &mut WindowState, ch: u32) -> Vec<Effect> {
    let Some(id) = state.focused_field() else {
        return Vec::new();
    };
    let edit = match ch {
        0x08 => Some(Edit::Backspace),
        c => char::from_u32(c)
            .filter(|c| !c.is_control())
            .map(Edit::Insert),
    };
    let Some(edit) = edit else {
        return Vec::new();
    };
    state.typed.push((id, edit));
    // Show the caret at once and restart its blink, so a character typed during
    // the off-phase does not land at an invisible caret. Every edit that
    // reaches this point is a text edit, so there is nothing else this could be
    // true for.
    vec![Effect::ResetCaretBlink, Effect::Invalidate, Effect::WakeUi]
}

/// Applies a key that is not a printable character.
///
/// The order of the four questions below is the order of specificity, and it is
/// the whole design: an open list is the innermost thing on screen, a focused
/// field is the next, and the rest belongs to the window.
fn on_key_down(state: &mut WindowState, key: usize, held: Modifiers) -> Vec<Effect> {
    const VK_TAB: usize = 0x09;
    const VK_RETURN: usize = 0x0D;
    const VK_ESCAPE: usize = 0x1B;
    const VK_SPACE: usize = 0x20;
    const VK_PRIOR: usize = 0x21;
    const VK_NEXT: usize = 0x22;
    const VK_END: usize = 0x23;
    const VK_HOME: usize = 0x24;
    const VK_LEFT: usize = 0x25;
    const VK_UP: usize = 0x26;
    const VK_RIGHT: usize = 0x27;
    const VK_DOWN: usize = 0x28;
    const VK_DELETE: usize = 0x2E;
    const VK_A: usize = 0x41;

    // An open list takes its own keys first. Escape included: it backs out of
    // the list rather than out of the dialog, because that is what the user
    // means by it while a list is showing. Space chooses like Enter, so the key
    // that opened the list also closes it on a choice. Tab is the one exception
    // — it leaves the list *and* moves on, so it is handled with the other
    // focus moves below.
    if let Some(id) = state.open_list() {
        let edit = match key {
            VK_UP => Some(Edit::Highlight(Move::Previous)),
            VK_DOWN => Some(Edit::Highlight(Move::Next)),
            VK_PRIOR => Some(Edit::Highlight(Move::PageBack)),
            VK_NEXT => Some(Edit::Highlight(Move::PageForward)),
            VK_HOME => Some(Edit::Highlight(Move::First)),
            VK_END => Some(Edit::Highlight(Move::Last)),
            VK_RETURN | VK_SPACE => Some(Edit::Commit),
            VK_ESCAPE => Some(Edit::Cancel),
            _ => None,
        };
        if let Some(edit) = edit {
            state.typed.push((id, edit));
            return vec![Effect::WakeUi];
        }
    }

    if key == VK_TAB {
        return step_focus(state, held.shift);
    }

    // Caret and selection keys for a focused text field. These are the keys
    // that never produce a `WM_CHAR` — arrows, Home, End, Delete, Ctrl+A — so
    // unlike printable characters and Backspace they have to be read here.
    if let Some(id) = state.focused_field() {
        let edit = match key {
            VK_LEFT => Some(Edit::CaretLeft { extend: held.shift }),
            VK_RIGHT => Some(Edit::CaretRight { extend: held.shift }),
            VK_HOME => Some(Edit::CaretHome { extend: held.shift }),
            VK_END => Some(Edit::CaretEnd { extend: held.shift }),
            VK_DELETE => Some(Edit::Delete),
            VK_A if held.ctrl => Some(Edit::SelectAll),
            _ => None,
        };
        if let Some(edit) = edit {
            state.typed.push((id, edit));
            // Any caret key shows the caret immediately and restarts the blink,
            // so moving it with the keyboard keeps it visible in the moment
            // rather than possibly during an off-phase. The repaint itself is
            // the owner's, reached through the edit just recorded — the same
            // path as every other field edit.
            return vec![Effect::ResetCaretBlink, Effect::WakeUi];
        }
    }

    // Space acts on the focused control; Enter confirms the dialog. The split
    // is what Windows dialogs and HTML forms both do, and it is what lets Enter
    // mean one thing wherever focus happens to be: press the focused button if
    // focus is on a button, otherwise press the confirming one. A dialog opens
    // focused on its dismissing button, so Enter on an untouched dialog
    // dismisses it without that being a rule of its own.
    //
    // Space is deliberately not read while a field has focus: there it is a
    // character, and characters are `WM_CHAR`'s business. What happens to it
    // after that — the digit filter drops it — is the field's.
    let activated = match key {
        VK_SPACE => match state.focus {
            Some(id) if !state.is_field(id) => Some(id),
            _ => None,
        },
        VK_RETURN => match state.focus {
            Some(id) if state.is_button(id) => Some(id),
            _ => crate::ui::focus::confirm(&state.content.rows),
        },
        _ => None,
    };
    if let Some(id) = activated {
        return activate(state, id);
    }

    // Escape with no list open closes the window, whatever has focus. For a
    // modal that is the same thing as Cancel — the owner discards the draft on
    // close — so there is one way out and not two that have to agree.
    if key == VK_ESCAPE {
        state.close_requested = true;
        return vec![Effect::WakeUi];
    }
    Vec::new()
}

/// Applies Alt+Down: opens the focused dropdown.
///
/// The binding Windows dialogs offer beside Space, and the one users who came
/// from them reach for. Alt makes it a *system* key, so it arrives at
/// `WM_SYSKEYDOWN` and not at `WM_KEYDOWN`.
///
/// An empty answer means "not ours", which is what sends the message to the
/// default handler.
fn on_syskey_down(state: &mut WindowState, key: usize) -> Vec<Effect> {
    const VK_DOWN: usize = 0x28;
    if key != VK_DOWN {
        return Vec::new();
    }
    match state.focus {
        Some(id) if state.is_dropdown(id) => activate(state, id),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::Painted;
    use super::*;
    use crate::testsupport::hot;
    use crate::ui::layout::{Columns, PanelContent};
    use crate::ui::row::DropdownItem;
    use crate::ui::row::{Gap, Row};
    use crate::ui::scrollbar::ScrollbarGeometry;
    use crate::ui::scrollbar::DROPDOWN_MAX_VISIBLE;
    use crate::ui::theme::Theme;
    use crate::ui::window::state::PaintedList;
    use crate::ui::window::Role;
    use windows::Win32::Graphics::Gdi::HFONT;

    /// A window with no rows, no hotspots and nothing focused.
    ///
    /// The whole reason the handlers were extracted: this is a `WindowState`
    /// built in a test, with no `HWND`, no device context and no message loop.
    /// Everything the mouse does to a window can now be asked of it directly.
    ///
    /// The fonts are null handles, which is honest — nothing here draws — and
    /// the two `Theme`s are the default, unscaled, so a pixel in a test is a
    /// pixel at 96 DPI.
    fn window() -> WindowState {
        WindowState {
            family: None,
            content: PanelContent {
                rows: Vec::new(),
                samples: Vec::new(),
                width: 200,
                height: 100,
                columns: Columns {
                    half: 100,
                    wide_single: 200,
                    // There are no rows, so there is no action button; the
                    // floor is what the scan returns for an empty set. Read
                    // from the theme rather than written out, so it cannot
                    // become a second statement of that number.
                    action_button: Theme::default()
                        .scaled(Dpi::new(96))
                        .metrics()
                        .button_min_width,
                },
            },
            theme: Theme::default().scaled(Dpi::new(96)),
            base_theme: Theme::default(),
            min_width: 200,
            dpi: Dpi::new(96),
            font: HFONT::default(),
            font_bold: HFONT::default(),
            clicked: None,
            close_requested: false,
            focus: None,
            focus_visible: false,
            pressed: None,
            hovered: None,
            hovered_button: None,
            leave_tracked: false,
            painted: None,
            gesture: Gesture::None,
            typed: Vec::new(),
            wheel_remainder: 0,
            caret_visible: true,
            role: Role::Panel,
            window_icons: [None, None],
        }
    }

    /// A list drawn by the painter for dropdown `id`, with no scrollbar.
    ///
    /// The keyboard and wheel paths ask only which list is open, but that fact
    /// now travels with the geometry it was drawn beside — which is the point
    /// of the type: there is no way to say "a list is open" without also saying
    /// where the last pass put it. The rectangle is the one the panel's own
    /// lists get; nothing here hit-tests against it.
    fn painted(id: HotspotId) -> PaintedList {
        PaintedList {
            id,
            bounds: Rect {
                left: 0,
                top: 0,
                right: 200,
                bottom: 120,
            },
            scroll: 0,
            scrollbar: None,
        }
    }

    fn at(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    fn button(id: HotspotId, top: i32, bottom: i32) -> Hotspot {
        Hotspot {
            rect: Rect {
                left: 0,
                top,
                right: 100,
                bottom,
            },
            id,
            is_button: true,
        }
    }

    /// No caret can be measured without a device context, so tests that reach
    /// a text field supply the index themselves. That is the seam the handler
    /// takes as a parameter.
    fn caret_at_start(_: &WindowState, _: HotspotId, _: i32) -> usize {
        0
    }

    /// A press arms a control; the release fires it only if it lands on the
    /// same one.
    ///
    /// This used to stand untested, on the grounds that the split of press and
    /// release lives in the window procedure and so needs a live window. It is
    /// the property that matters, because before it existed a press on OK that
    /// slid onto Cancel pressed Cancel.
    #[test]
    fn a_release_elsewhere_fires_nothing() {
        let mut w = window();
        w.painted = Some(Painted {
            hotspots: vec![button(hot(10), 0, 20), button(hot(11), 20, 40)],
            ..Painted::default()
        });

        on_lbutton_down(&mut w, at(5, 5), false, false, caret_at_start);
        assert_eq!(
            w.pressed,
            Some(hot(10)),
            "the press arms the control under it"
        );

        on_lbutton_up(&mut w, at(5, 30));
        assert_eq!(
            w.clicked, None,
            "a release on another control fires nothing"
        );
        assert_eq!(w.pressed, None, "and the arming is spent either way");
    }

    /// The same press and release, landing together, does fire.
    ///
    /// Beside the test above so that suppressing the wrong activation cannot be
    /// mistaken for suppressing activation.
    #[test]
    fn a_release_on_the_armed_control_fires_it() {
        let mut w = window();
        w.painted = Some(Painted {
            hotspots: vec![button(hot(10), 0, 20)],
            ..Painted::default()
        });

        on_lbutton_down(&mut w, at(5, 5), false, false, caret_at_start);
        let effects = on_lbutton_up(&mut w, at(5, 10));

        assert_eq!(w.clicked, Some(hot(10)));
        assert!(
            effects.contains(&Effect::Invalidate) && effects.contains(&Effect::WakeUi),
            "a click must repaint and nudge the loop, or the button does \
             nothing until the next timer tick"
        );
    }

    /// A press and release on bare background is a click, reported as its own
    /// id.
    ///
    /// It is how an open dropdown learns to close. Reported distinctly so the
    /// owner can tell "clicked nothing" from "no click happened" without the
    /// window procedure needing to know what a dropdown is.
    #[test]
    fn a_click_on_background_is_still_a_click() {
        let mut w = window();
        on_lbutton_down(&mut w, at(50, 50), false, false, caret_at_start);
        on_lbutton_up(&mut w, at(50, 50));
        assert_eq!(w.clicked, Some(HotspotId::NOTHING));
    }

    /// Losing the capture ends the gesture.
    ///
    /// Windows takes the capture without asking, and `WM_LBUTTONUP` then never
    /// arrives. A gesture left set made every later move read as a continuing
    /// drag: the thumb followed a pointer with no button held. This used to be
    /// checked by searching the source of `wnd_proc` for the string
    /// `WM_CAPTURECHANGED =>`; it is now checked by taking the state through
    /// it.
    #[test]
    fn losing_the_capture_ends_the_gesture() {
        let mut w = window();
        w.gesture = Gesture::Text {
            id: HotspotId::new(7),
        };

        on_capture_lost(&mut w);
        assert_eq!(w.gesture, Gesture::None);

        // And a move afterwards is an ordinary hover, not a continued drag: a
        // drag would have pushed a caret edit.
        on_mouse_move(&mut w, at(10, 10), caret_at_start);
        assert!(w.typed.is_empty(), "the drag must be over, not merely idle");
    }

    /// A drag owns the pointer: the hover does not move underneath it.
    ///
    /// Otherwise dragging the thumb across the options drags the highlight
    /// along with it — two indicators moving at once, one of which the user is
    /// not pointing at.
    #[test]
    fn a_thumb_drag_suppresses_hover_feedback() {
        let mut w = window();
        w.painted = Some(Painted {
            hotspots: vec![button(hot(10), 0, 20)],
            ..Painted::default()
        });
        w.gesture = Gesture::Thumb { grab: 0 };

        let effects = on_mouse_move(&mut w, at(5, 5), caret_at_start);

        assert_eq!(w.hovered_button, None, "a drag must not light a button");
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::InvalidateRect(_))),
            "and must not repaint one"
        );
    }

    /// Hover feedback fires on crossing a button's edge, not on every move.
    ///
    /// A move within one button costs nothing; a move across an edge costs one
    /// repaint of each rectangle that changed — the button being left and the
    /// one being entered.
    #[test]
    fn hover_repaints_only_when_the_button_under_the_pointer_changes() {
        let mut w = window();
        w.painted = Some(Painted {
            hotspots: vec![button(hot(10), 0, 20), button(hot(11), 20, 40)],
            ..Painted::default()
        });

        let entering = on_mouse_move(&mut w, at(5, 5), caret_at_start);
        assert_eq!(w.hovered_button, Some(hot(10)));
        assert_eq!(
            entering
                .iter()
                .filter(|e| matches!(e, Effect::InvalidateRect(_)))
                .count(),
            1,
            "entering the first button repaints it and nothing else"
        );

        let within = on_mouse_move(&mut w, at(6, 6), caret_at_start);
        assert!(
            !within
                .iter()
                .any(|e| matches!(e, Effect::InvalidateRect(_))),
            "moving inside the same button changes nothing on screen"
        );

        let crossing = on_mouse_move(&mut w, at(5, 25), caret_at_start);
        assert_eq!(w.hovered_button, Some(hot(11)));
        assert_eq!(
            crossing
                .iter()
                .filter(|e| matches!(e, Effect::InvalidateRect(_)))
                .count(),
            2,
            "crossing an edge repaints the button left and the one entered"
        );
    }

    /// The leave tracker is armed once per entry, not once per move.
    ///
    /// `TrackMouseEvent` is one-shot: it fires a single `WM_MOUSELEAVE` and
    /// disarms. Re-arming on every move would be a syscall per pixel.
    #[test]
    fn the_leave_tracker_is_armed_once_per_entry() {
        let mut w = window();

        let first = on_mouse_move(&mut w, at(5, 5), caret_at_start);
        assert!(first.contains(&Effect::TrackMouseLeave));

        let second = on_mouse_move(&mut w, at(6, 6), caret_at_start);
        assert!(!second.contains(&Effect::TrackMouseLeave));

        on_mouse_leave(&mut w);
        let re_entry = on_mouse_move(&mut w, at(5, 5), caret_at_start);
        assert!(
            re_entry.contains(&Effect::TrackMouseLeave),
            "the tracker has fired and disarmed, so re-entry must arm a new one"
        );
    }

    /// A press only reaches a scrollbar the last paint pass actually drew.
    ///
    /// The thumb geometry used to be its own field, cleared only inside the
    /// branch that draws a list — so closing one left the previous frame's
    /// thumb rectangle standing, and a press into that now-empty strip started
    /// a drag of a scrollbar nobody could see, capturing the mouse and
    /// scrolling a list that was no longer on screen. Holding the geometry
    /// inside the same `Option` that says a list was drawn is what makes that
    /// unsayable; this pins the behaviour that follows from it.
    #[test]
    fn a_closed_list_leaves_no_thumb_to_grab() {
        let thumb = Rect {
            left: 180,
            top: 40,
            right: 194,
            bottom: 80,
        };
        let mut list = painted(hot(42));
        list.scrollbar = Some(ScrollbarGeometry {
            bar: Rect {
                left: 180,
                top: 0,
                right: 194,
                bottom: 120,
            },
            up: Rect::default(),
            down: Rect::default(),
            track: Rect {
                left: 180,
                top: 14,
                right: 194,
                bottom: 106,
            },
            thumb,
        });
        let grabbed_at = at(thumb.left + 2, thumb.top + 2);

        let mut w = window();
        w.painted = Some(Painted {
            list: Some(list),
            ..Painted::default()
        });
        let effects = on_lbutton_down(&mut w, grabbed_at, false, false, |_, _, _| 0);
        assert!(
            matches!(w.gesture, Gesture::Thumb { .. }),
            "a thumb that is on screen must be grabbable"
        );
        assert!(
            effects.contains(&Effect::Capture),
            "a thumb drag captures the mouse"
        );

        // The same press, once the list has closed.
        let mut w = window();
        w.painted = Some(Painted::default());
        let effects = on_lbutton_down(&mut w, grabbed_at, false, false, |_, _, _| 0);
        assert_eq!(
            w.gesture,
            Gesture::None,
            "a scrollbar that was not painted cannot be dragged"
        );
        assert!(
            !effects.contains(&Effect::Capture),
            "and it must not capture the mouse either"
        );
    }

    /// Partial wheel movement accumulates instead of being discarded.
    ///
    /// Precision touchpads report less than one detent per step — 40 units is
    /// common — and dividing each message on its own gave zero every time, so
    /// the list did not move at all however long the user scrolled.
    #[test]
    fn three_partial_wheel_steps_make_one_notch() {
        let mut w = window();
        w.painted = Some(Painted {
            list: Some(painted(hot(42))),
            ..Painted::default()
        });

        assert!(on_mouse_wheel(&mut w, 40).is_empty());
        assert!(on_mouse_wheel(&mut w, 40).is_empty());
        assert!(!on_mouse_wheel(&mut w, 40).is_empty());

        assert_eq!(
            w.typed,
            vec![(hot(42), Edit::Scroll(1))],
            "three forty-unit steps are one detent, delivered once"
        );
    }

    /// A wheel message with no list open keeps the remainder.
    ///
    /// A slow scroll that crossed a moment with nothing open should not have to
    /// start again from nothing.
    #[test]
    fn the_wheel_remainder_survives_a_closed_list() {
        let mut w = window();
        w.painted = Some(Painted {
            list: Some(painted(hot(42))),
            ..Painted::default()
        });
        on_mouse_wheel(&mut w, 80);

        w.painted = Some(Painted::default());
        on_mouse_wheel(&mut w, 80);
        assert_eq!(w.wheel_remainder, 80, "the fraction already turned stands");

        w.painted = Some(Painted {
            list: Some(painted(hot(42))),
            ..Painted::default()
        });
        on_mouse_wheel(&mut w, 40);
        assert_eq!(w.typed, vec![(hot(42), Edit::Scroll(1))]);
    }

    /// The caret blink repaints its own box, not the window.
    ///
    /// It fires twice a second while a field is focused; invalidating
    /// everything would re-measure and redraw every row for a one-pixel line.
    #[test]
    fn the_caret_blink_repaints_only_the_value_box() {
        let mut w = window();
        let box_rect = Rect {
            left: 1,
            top: 2,
            right: 3,
            bottom: 4,
        };
        w.content.rows = vec![Row::Field {
            label: "Interval".into(),
            value: "3000".into(),
            caret: 0,
            sel_start: 0,
            sel_end: 0,
            id: HotspotId::new(5),
        }];
        w.focus = Some(hot(5));
        w.painted = Some(Painted {
            caret_rect: Some(box_rect),
            ..Painted::default()
        });

        let effects = on_caret_tick(&mut w);
        assert!(!w.caret_visible, "the phase flips");
        assert_eq!(
            effects,
            vec![Effect::InvalidateRect(box_rect), Effect::WakeUi]
        );
    }

    /// With nothing focused the blink does nothing at all.
    ///
    /// The timer should not be running; until it is stopped, it must not repaint
    /// an idle window twice a second.
    #[test]
    fn the_caret_blink_is_inert_without_a_focused_field() {
        let mut w = window();
        w.painted = Some(Painted {
            caret_rect: Some(Rect::default()),
            ..Painted::default()
        });
        assert!(on_caret_tick(&mut w).is_empty());
        assert!(w.caret_visible, "and does not even flip the phase");
    }

    /// Escape backs out of an open list, not out of the dialog.
    ///
    /// While a list is showing, that is what the user means by it. With no list
    /// open the same key closes the window.
    #[test]
    fn escape_closes_the_list_before_it_closes_the_window() {
        const VK_ESCAPE: usize = 0x1B;
        let mut w = window();
        w.painted = Some(Painted {
            list: Some(painted(hot(42))),
            ..Painted::default()
        });

        on_key_down(&mut w, VK_ESCAPE, Modifiers::default());
        assert_eq!(w.typed, vec![(hot(42), Edit::Cancel)]);
        assert!(!w.close_requested, "the dialog must stay up");

        w.painted = Some(Painted::default());
        w.typed.clear();
        on_key_down(&mut w, VK_ESCAPE, Modifiers::default());
        assert!(w.close_requested, "with nothing open, Escape leaves");
    }

    /// Tab out of an open list leaves it as it was found.
    ///
    /// It must not choose: the highlight is wherever arrowing left it, and
    /// committing that on the way past would change a setting the user was only
    /// looking at.
    #[test]
    fn tab_out_of_a_list_cancels_it_rather_than_choosing() {
        const VK_TAB: usize = 0x09;
        let mut w = window();
        w.content.rows = vec![
            Row::Field {
                label: "Interval".into(),
                value: "3000".into(),
                caret: 0,
                sel_start: 0,
                sel_end: 0,
                id: HotspotId::new(5),
            },
            Row::Space(Gap::Single),
        ];
        w.painted = Some(Painted {
            list: Some(painted(hot(42))),
            ..Painted::default()
        });

        on_key_down(&mut w, VK_TAB, Modifiers::default());
        assert!(
            w.typed.contains(&(hot(42), Edit::Cancel)),
            "the list is dismissed, not committed"
        );
        assert_eq!(w.focus, Some(hot(5)), "and focus moves on");
    }

    /// Shift reaches the edit; the two modifiers are not interchangeable.
    ///
    /// `Modifiers` exists for this: `on_key_down(state, key, shift, ctrl)` with
    /// the two swapped is a call the compiler has nothing to say about, and
    /// Shift+Left and Ctrl+Left mean different things.
    #[test]
    fn shift_extends_the_selection_and_ctrl_does_not() {
        const VK_LEFT: usize = 0x25;
        let mut w = window();
        w.content.rows = vec![Row::Field {
            label: "Interval".into(),
            value: "3000".into(),
            caret: 4,
            sel_start: 4,
            sel_end: 4,
            id: HotspotId::new(5),
        }];
        w.focus = Some(hot(5));

        on_key_down(
            &mut w,
            VK_LEFT,
            Modifiers {
                shift: true,
                ctrl: false,
            },
        );
        assert_eq!(w.typed, vec![(hot(5), Edit::CaretLeft { extend: true })]);

        w.typed.clear();
        on_key_down(
            &mut w,
            VK_LEFT,
            Modifiers {
                shift: false,
                ctrl: true,
            },
        );
        assert_eq!(w.typed, vec![(hot(5), Edit::CaretLeft { extend: false })]);
    }

    /// A press on a control the keyboard cannot reach leaves focus alone.
    ///
    /// An option inside an open list and the list's scrollbar are neither — they
    /// belong to the dropdown, which keeps the ring while they are used.
    #[test]
    fn a_press_on_an_unreachable_control_does_not_move_focus() {
        let mut w = window();
        w.content.rows = vec![Row::Space(Gap::Single)];
        w.painted = Some(Painted {
            hotspots: vec![button(HotspotId::new(42).option(0), 0, 20)],
            ..Painted::default()
        });

        on_lbutton_down(&mut w, at(5, 5), false, false, caret_at_start);
        assert_eq!(w.focus, None);
        assert!(!w.focus_visible);
    }
    /// A list that fits on screen has nowhere to scroll, and says so as zero.
    ///
    /// The number `scroll_at` is driven by used to be derived here as
    /// `total - visible` from a pair. Both are `usize`, the release profile
    /// wraps that subtraction in silence, and the wrapped value went straight
    /// into a `clamp` whose lower bound then exceeded its upper — a panic in
    /// the standard library, under `WM_MOUSEMOVE`, in a process built with
    /// `panic = "abort"`.
    ///
    /// The fix was to stop deriving it, and this is what holds that. Two
    /// options is fewer than a list shows at once, which is the case the
    /// subtraction got wrong; the assertion is that the answer is zero rather
    /// than that no panic occurred, because "it did not crash" is also what a
    /// wrapped `usize` looks like from the outside in a release build.
    #[test]
    fn a_list_that_fits_has_no_span() {
        let id = hot(7);
        let mut w = window();
        w.content.rows = vec![Row::Dropdown {
            label: "Theme".to_owned(),
            options: vec![item("Light"), item("Dark")],
            selected: 0,
            highlighted: 0,
            open: true,
            scroll: 0,
            id,
        }];
        assert!(
            w.content.rows.len() < DROPDOWN_MAX_VISIBLE,
            "the case under test is a list shorter than one screenful"
        );
        assert_eq!(
            w.list_span(id),
            Some(0),
            "a list that fits entirely cannot be scrolled"
        );
    }

    /// One option of a dropdown, with no native-script part.
    fn item(label: &str) -> DropdownItem {
        DropdownItem {
            label: label.to_owned(),
            native: String::new(),
            font: None,
        }
    }
}
