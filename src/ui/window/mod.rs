//! The panel and dialog windows: creation, ownership, and the message loop
//! that drives them.
//!
//! # Why this is a directory
//!
//! It was one file of three and a half thousand lines holding four unrelated
//! things: window ownership, message routing, painting, and the drawing
//! primitives painting is built from. Nothing linked them but the fact that
//! all four were written while building the same window. The split follows the
//! boundary this module tree already draws — *what to show* versus *how to draw
//! it* — applied once more inside "how":
//!
//! * [`mod@state`] — `WindowState` and the per-window bookkeeping.
//! * [`mod@proc`] — the window procedure and its handlers.
//! * [`mod@paint`] — the two painting passes, panel and dropdown list.
//! * [`mod@draw`] — the primitives both painting passes are made of.
//!
//! What stays here is what owns a window: [`Window`], [`Role`], placement, and
//! class registration.
//!
//! The module keeps its name. The audit proposed `ui/win/`, but every path in
//! the program spells `ui::window::` and renaming it would have been churn
//! across a dozen files in exchange for three characters.

use std::cell::Cell;
use std::marker::PhantomData;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
// `AdjustWindowRectExForDpi` lives here, not under `WindowsAndMessaging`
// with its DPI-agnostic counterpart `AdjustWindowRectEx`: the DPI-aware
// overload is a distinct import from a distinct module, and saying so is
// what keeps a future caller from reaching for the wrong one out of habit.
// `GetSystemMetricsForDpi` is the same story and is imported at its one
// call site rather than here.
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
// EnableWindow lives under input rather than window management: disabling a
// window is defined as an input-focus operation, not a styling one.
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyIcon, DestroyWindow, FlashWindow, GetCaretBlinkTime, GetSystemMetrics,
    GetWindowRect, KillTimer, LoadCursorW, RegisterClassW, SetForegroundWindow, SetTimer,
    SetWindowPos, ShowWindow, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, HICON, HWND_TOP, IDC_ARROW,
    SM_CXICON, SM_CXSCREEN, SM_CXSMICON, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WS_CAPTION,
    WS_EX_DLGMODALFRAME, WS_EX_TOOLWINDOW, WS_MINIMIZEBOX, WS_SYSMENU, WS_VISIBLE,
};

use super::layout::PanelContent;
use super::row::{Edit, HotspotId, Row};
use super::scrollbar::last_scroll_offset;
use super::text::fonts;
use crate::ui::rect::Rect;
use crate::ui::text::Script;
use crate::ui::theme::Theme;
use crate::ui::Dpi;
/// Fixed style: caption, close button and a minimise box. No thick frame, no
/// maximise.
///
/// `WS_MINIMIZEBOX` is present for the taskbar's sake, not for the button it
/// puts in the caption. The shell only offers the click-to-toggle behaviour
/// every other application has — click the taskbar button of the active
/// window and it goes away, click again and it comes back — for windows that
/// declare themselves minimisable. Without the style the shell has nothing to
/// toggle, so it merely re-activates an already-active window and the click
/// appears to do nothing.
///
/// The minimise itself is intercepted in `WM_SYSCOMMAND` and turned into a
/// hide-to-tray, so the window never actually shrinks to a taskbar button:
/// the style advertises a capability, and the handler substitutes the
/// behaviour this utility wants for it. That keeps the tray icon the only
/// place the panel lives while still giving the taskbar its usual toggle.
const PANEL_STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0);

/// `WM_MOUSELEAVE`, defined locally.
///
/// The constant lives in the `Win32_UI_Controls` feature of windows-rs, which
/// is otherwise unused here; pulling that whole feature in for one stable
/// message value would cost compile time and binary size for nothing. The
/// value (0x02A3) has been fixed since Windows 95 and will not change.
pub(super) const WM_MOUSELEAVE: u32 = 0x02A3;

/// How far outside a control the focus ring is drawn, in pixels: one pixel of
/// clear space, then the one-pixel frame itself.
///
/// Not scaled with DPI. The ring is a hairline by intent — the same one-pixel
/// frame at every scale, like the caret and the selection edge — and a ring
/// three pixels thick at 200 % would read as a border, not as a marker.
///
/// Only [`Row::Dropdown`] and [`Row::Field`] charge for it in the height
/// table. Everywhere else the room is already there: a checkbox's box is
/// inset inside its line, the buzzer and self-test buttons overhang a line
/// whose neighbours are text with slack above and below it, and the title and
/// dialog button rows sit against the window's own margins.
pub(crate) const FOCUS_RING_OUTSET: i32 = 2;

mod draw;
mod paint;
mod proc;
mod state;

use proc::wnd_proc;
use state::{attach_state, detach_state, with_state, Detached, Gesture, WindowState};

/// Which kind of window this handle owns.
///
/// The status panel is one ordinary top-level window. Every *modal* surface —
/// the settings dialog and the self-test confirmation — is the other kind:
/// a dialog-framed, tool-styled window that disables its owner while it is up.
/// They are one `Role` because they are one window mechanism; what differs
/// between settings and a confirmation is only the rows drawn inside, which the
/// window neither knows nor needs to. Sharing one window with the panel is what
/// the old design got wrong: closing the modal had to decide what to put back
/// in its place, so it resurrected a panel the user had never asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    Panel,
    Modal,
}

impl Role {
    /// The window style and extended style windows of this role are created
    /// with.
    ///
    /// One definition, and it has to be one: `AdjustWindowRectExForDpi` turns a
    /// client size into a window size by adding the frame those exact two
    /// values describe, so a frame allowance computed from a *different* pair
    /// than `CreateWindowExW` receives is wrong by however much the two pairs
    /// disagree. They disagreed here. The allowance was computed from
    /// `PANEL_STYLE` with no extended style at all three call sites, while a
    /// modal is created as `WS_EX_DLGMODALFRAME | WS_EX_TOOLWINDOW`: a tool
    /// window has the short caption, not the full one, and a dialog frame is
    /// thicker than a plain one. So every Settings and confirmation window got
    /// a client area a few pixels too tall and a few pixels too narrow — and
    /// both errors grow with DPI, because caption height and border width are
    /// exactly what `ForDpi` scales.
    ///
    /// The minimise box belongs to the panel alone. On a modal it would be a
    /// control that hides a *modal* window while leaving its owner disabled —
    /// a panel the user cannot click and no visible dialog explaining why. The
    /// modal is also a tool window with no taskbar button, so the toggle the
    /// style exists to enable has nothing to toggle from.
    fn styles(self) -> (WINDOW_STYLE, WINDOW_EX_STYLE) {
        match self {
            Role::Panel => (PANEL_STYLE, WINDOW_EX_STYLE(0)),
            Role::Modal => (
                WINDOW_STYLE(PANEL_STYLE.0 & !WS_MINIMIZEBOX.0),
                WS_EX_DLGMODALFRAME | WS_EX_TOOLWINDOW,
            ),
        }
    }

    /// Outer window size for a client area of `client` at `dpi`.
    ///
    /// The `ForDpi` variant, always: the plain `AdjustWindowRectEx` adds a
    /// frame sized for whatever DPI the calling thread currently carries, and
    /// on a per-monitor-aware process that is not in general the DPI of the
    /// monitor this window lives on. The caption bar and borders grow with DPI
    /// like everything else, so an allowance computed at the wrong DPI leaves
    /// the window short of what its content and its real frame together need.
    ///
    /// Safe: every argument is a plain value, and the call reads styles and a
    /// DPI to arrive at a size. There is nothing a caller could get wrong here
    /// that the type system does not already stop.
    fn frame(self, client: (i32, i32), dpi: Dpi) -> (i32, i32) {
        let (style, ex_style) = self.styles();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client.0,
            bottom: client.1,
        };
        // Reported rather than discarded: on failure `rect` is still the
        // client size, so the window is created short by the whole thickness of
        // its own frame — a window whose content does not fit, with nothing
        // anywhere saying why.
        let raw = dpi.raw();
        // SAFETY: `rect` is a live local the call adjusts in place; the styles
        // and dpi are by value.
        let adjusted = unsafe { AdjustWindowRectExForDpi(&mut rect, style, false, ex_style, raw) };
        if let Err(e) = adjusted {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                &format!("window frame could not be measured at {raw} dpi: {e}"),
            );
        }
        (rect.right - rect.left, rect.bottom - rect.top)
    }
}

/// A [`Role`] carried in the type rather than in a field.
///
/// The panel and the modal surfaces are one window mechanism and were one Rust
/// type, told apart by a `role` field. That made "settings rows pushed into
/// the panel" expressible, and the only thing standing against it was a
/// `debug_assert_eq!` in each of the two redraw paths — which the shipped
/// binary does not contain, because there is no debug build of this program.
/// A check that exists in no build the user runs is a comment claiming a
/// guarantee nobody provides.
///
/// With the role in the type the confusion is not checked, it is unspellable:
/// [`PanelWindow`] and [`ModalWindow`] are different types, so the settings
/// path cannot be handed the panel's handle by any accident short of writing
/// the wrong field name — which does not compile.
///
/// The runtime [`Role`] does not go away, and should not: `WindowState` and
/// the window procedure receive messages about a window they know only by
/// handle, and there the role has to be a value. `ROLE` is where the type-level
/// fact becomes that value, once, at construction.
pub(crate) trait WindowRole {
    /// The role a window of this kind is created with.
    const ROLE: Role;
}

/// Marker for the status panel.
pub(crate) struct PanelRole;

/// Marker for a modal surface — the settings dialog or the confirmation.
pub(crate) struct ModalRole;

impl WindowRole for PanelRole {
    const ROLE: Role = Role::Panel;
}

impl WindowRole for ModalRole {
    const ROLE: Role = Role::Modal;
}

/// Handle to a live window. Dropping it destroys the window, which is the
/// entire point of this design: hidden means gone, not merely invisible.
///
/// `R` says which kind of window it is; every operation below is shared by
/// both, and the two constructors are the only members that are not.
pub(crate) struct Window<R: WindowRole> {
    hwnd: HWND,
    /// Set for a modal settings dialog: the panel it disabled, to be
    /// re-enabled on drop.
    owner: Option<HWND>,
    /// Nothing is stored: `R` is the role, and [`WindowRole::ROLE`] is how it
    /// is read back when a value is needed.
    role: PhantomData<R>,
}

/// The status panel.
pub(crate) type PanelWindow = Window<PanelRole>;

/// A modal surface over the panel.
pub(crate) type ModalWindow = Window<ModalRole>;

impl PanelWindow {
    /// Creates and shows the status panel. A saved position is used when it
    /// still lands on an attached monitor; otherwise the window is centred on
    /// the monitor the user is currently working on.
    ///
    /// The caption comes from the caller's locale like every other visible
    /// string. It used to be a `w!` literal here, which left the title bar
    /// English whatever language the panel below it spoke.
    pub(crate) fn open(
        content: PanelContent,
        theme: &Theme,
        family: Option<&'static str>,
        pos: Option<(i32, i32)>,
        title: &str,
    ) -> Option<Self> {
        Self::create(content, theme, family, pos, None, title)
    }
}

impl ModalWindow {
    /// Creates a modal window over `owner`: the settings dialog or the
    /// self-test confirmation, which differ only in the rows they carry.
    ///
    /// Modality is enforced two ways, and neither makes the window top over
    /// other applications. The owner relationship makes Windows keep the child
    /// above its parent, and `EnableWindow(owner, false)` blocks clicks on the
    /// panel underneath — including its close and system-menu buttons. The one
    /// gap the owner relationship leaves is that the shell can still restack
    /// the disabled panel above its child; that is closed by stripping the
    /// Z-order change in the panel's `WM_WINDOWPOSCHANGING` (see there), not by
    /// marking the modal topmost. `WS_EX_TOPMOST` would keep it above every
    /// other application's windows too, which is not what modal-to-the-panel
    /// means. Exit from the tray still works because the tray icon is not a
    /// window and is unaffected by `EnableWindow`.
    pub(crate) fn open(
        content: PanelContent,
        theme: &Theme,
        family: Option<&'static str>,
        owner: Option<HWND>,
        pos: Option<(i32, i32)>,
        title: &str,
    ) -> Option<Self> {
        let win = Self::create(content, theme, family, pos, owner, title)?;
        if let Some(owner) = owner {
            // SAFETY: `owner` is the panel's window, alive because the handle that owns
            // it is alive — this dialog borrows it for exactly that reason.
            unsafe {
                // Disabling the owner is what makes this modal rather than
                // merely on top: a disabled window receives no mouse or
                // keyboard input at all, so the panel can be neither closed
                // nor minimised while the modal is up.
                let _ = EnableWindow(owner, false);
            }
            MODAL_OWNER.with(|m| m.set(Some(owner)));
        }
        Some(win)
    }
}

impl<R: WindowRole> Window<R> {
    /// DPI this window is currently drawn at.
    ///
    /// Set once at creation and refreshed on `WM_DPICHANGED`. A caller
    /// rebuilding this window's content — `redraw_panel`, `redraw_settings`
    /// — needs it to measure text at the same scale the window is already
    /// laid out in, rather than guessing or assuming the 96-DPI baseline.
    pub(crate) fn dpi(&self) -> Dpi {
        with_state(self.hwnd, |st| st.dpi).unwrap_or(Dpi::BASELINE)
    }

    /// Creates the window at the DPI of the monitor it will actually open
    /// on, in one pass with no visible resize afterwards.
    ///
    /// `content` and `theme` arrive built at the 96-DPI baseline — the same
    /// thing `main.rs` has always produced, unchanged by any of this — and
    /// are provisional: they exist only to get a `(w, h)` good enough to ask
    /// `place_window` where the window goes. Once `place_window` answers,
    /// the point it chose names a real monitor, and that monitor's DPI is
    /// very often not 96. When it is not, `content` and `theme` are rebuilt
    /// against `theme.scaled(dpi)` before anything is measured against them
    /// again — `content_size`, `AdjustWindowRectExForDpi`, the fonts — and
    /// `place_window` is asked a second time with the corrected size, so
    /// a saved position that only just fit the 96-DPI window is re-checked
    /// against the size it will actually occupy rather than one that never
    /// appears on screen.
    ///
    /// This runs before `CreateWindowExW`, so the correction is invisible:
    /// there is no intermediate frame at the wrong size to flash before a
    /// resize catches up. `WM_DPICHANGED` (`wnd_proc`) is the other half of
    /// this — it repeats the same rebuild when a window already on screen is
    /// dragged to a monitor of a different DPI, using the same `theme` and
    /// `role` this closure closes over so the two paths cannot drift apart.
    fn create(
        content: PanelContent,
        theme: &Theme,
        family: Option<&'static str>,
        pos: Option<(i32, i32)>,
        owner: Option<HWND>,
        title: &str,
    ) -> Option<Self> {
        register_class();

        // The floor `sized` clamped against in `main.rs`: `panel_width`
        // for the status panel, `settings_width` for either modal —
        // `role` alone tells them apart, exactly as it already does for
        // the window style a few lines below. Computed unconditionally,
        // not only when a rescale turns out to be needed, because it is
        // also stored in `WindowState` for `WM_DPICHANGED` to use later.
        let role = R::ROLE;
        let min_width = match role {
            Role::Panel => theme.metrics.panel_width,
            Role::Modal => theme.metrics.settings_width,
        };

        // Client size is requested; the OS adds the frame. Provisional:
        // recomputed below once the target monitor's real DPI is known.
        // `ForDpi` at a stated 96 rather than the plain function, so the
        // 96-DPI assumption here is explicit rather than riding on which
        // overload happens to be called — the same reasoning that made
        // `rebuild_content_and_frame` use it for the real, non-96 case.
        let (mut w, mut h) = role.frame((content.width, content.height), Dpi::BASELINE);
        let (mut x, mut y) = place_window(pos, w, h);

        let dpi = dpi_for_point(x, y);
        let (font, font_bold, content) = if dpi == Dpi::BASELINE {
            let (font, font_bold) = fonts(script_of(theme, family, dpi));
            (font, font_bold, content)
        } else {
            let (content, size) = rebuild_content_and_frame(
                content.rows,
                content.samples,
                theme,
                family,
                min_width,
                dpi,
                role,
            );
            w = size.0;
            h = size.1;
            // Re-placed at the corrected size: a saved position that
            // fit the 96-DPI window is re-checked against the size the
            // window will actually be, and centring (the no-saved-
            // position path) centres the real footprint rather than the
            // provisional one.
            (x, y) = place_window(pos, w, h);

            let (font, font_bold) = fonts(script_of(theme, family, dpi));
            (font, font_bold, content)
        };

        // Read before `content` is moved into the window state below.
        let initial_focus = crate::ui::focus::initial(&content.rows);

        let caption = crate::wide::nul_terminated(title);
        // The same pair the frame allowance above was computed from —
        // `Role::styles` is the only place either is decided.
        let (style, ex_style) = role.styles();

        // SAFETY: the class was registered above, the strings are static wide
        // literals, and `instance` is this module's own. Nothing here outlives the
        // call except the window, which the returned `Window` owns.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                w!("ups_monitor_panel"),
                PCWSTR(caption.as_ptr()),
                WINDOW_STYLE(style.0 | WS_VISIBLE.0),
                x,
                y,
                w,
                h,
                owner,
                None,
                None,
                None,
            )
        };
        // `CreateWindowExW` now answers `Result` rather than a handle that
        // has to be tested against zero. The check is the same one, moved
        // into the type: a failure is a failure, and there is no null
        // handle to carry past this point.
        let Ok(hwnd) = hwnd else {
            return None;
        };

        // The state is attached before anything else touches the window.
        //
        // It cannot fail any more, and that is the point of where it now
        // lives: the window owns its state through `GWLP_USERDATA`, so
        // attaching is a store into a slot that is this window's alone. The
        // shared vector it replaces could refuse the push — and used to do
        // so silently, leaving a window on screen that paints nothing,
        // answers no click and reports `close_requested() == false` for
        // ever, with not one line anywhere saying why.
        attach_state(
            hwnd,
            WindowState {
                content,
                // Scaled once here, not read raw from the
                // caller: every other reader of `state.theme`
                // (`paint`, hit-testing, the scrollbar) expects
                // device pixels for this window's actual DPI,
                // not the 96-DPI baseline `theme.rs` defines.
                theme: theme.scaled(dpi),
                base_theme: *theme,
                family,
                min_width,
                dpi,
                font,
                font_bold,
                clicked: None,
                close_requested: false,
                // Where a window opens is the rows' answer, not
                // this function's: a dialog opens on its
                // dismissing button, the panel on nothing.
                focus: initial_focus,
                focus_visible: false,
                pressed: None,
                painted: None,
                hovered: None,
                hovered_button: None,
                leave_tracked: false,
                gesture: Gesture::None,
                typed: Vec::new(),
                caret_visible: true,
                wheel_remainder: 0,
                role,
                window_icons: [None, None],
            },
        );

        // The window icon is set per window rather than on the class:
        // it depends on the active theme's accent, which can change at
        // runtime, and a class icon is fixed at registration.
        set_window_icon(hwnd, theme);

        // SAFETY: `hwnd` names this panel's window to the system, which validates
        // it; the flag is by value.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
        // Settings sits strictly above the panel through the owner
        // relationship plus the panel's WM_WINDOWPOSCHANGING guard, not by
        // being marked topmost: a topmost dialog would also float above
        // every other application, which is not what modal-to-the-panel
        // means. Bringing it to the foreground is enough on creation; the
        // owner relationship keeps it above the panel from then on.
        // SAFETY: as above — the window is this panel's own and still alive.
        let _ = unsafe { SetForegroundWindow(hwnd) };
        Some(Self {
            hwnd,
            owner,
            role: PhantomData,
        })
    }

    pub(crate) fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Replaces the window's stored theme and repaints.
    ///
    /// Moves a new palette into the window, repainting and re-iconing only when
    /// the theme actually changed.
    ///
    /// This is called on every `redraw_panel`, which is every poll and every
    /// timer tick — but the theme changes only when the user picks one in
    /// settings. Doing the work unconditionally was expensive twice over: it
    /// re-rendered and re-created the title-bar icon every second (which, on the
    /// mingw build with no icon resource, also leaked an `HICON` pair each time,
    /// since the fallback path allocates), and it invalidated the whole window,
    /// throwing away the content diff `update` exists to compute. Comparing
    /// against the stored theme first means an unchanged theme costs one clone
    /// and one comparison, and a real change still repaints top to bottom — a
    /// theme swap that leaves the rows byte-for-byte identical (same text, same
    /// layout, different colours) still invalidates, which `update` would skip.
    ///
    /// `theme` arrives at the 96-DPI baseline, the same as every other caller
    /// in this module receives it — scaling happens here, against this
    /// window's own `state.dpi`, rather than being the caller's job. That
    /// keeps DPI entirely inside this module: `main.rs` builds themes and
    /// rows, never pixels, and cannot pass in a theme scaled for the wrong
    /// window or forget to scale one at all.
    pub(crate) fn apply_style(&self, theme: &Theme, family: Option<&'static str>) {
        let changed = with_state(self.hwnd, |state| {
            let scaled = theme.scaled(state.dpi);
            if state.theme == scaled && state.family == family {
                return false;
            }
            state.theme = scaled;
            // The family travels with the theme because the two arrive
            // together — a language change rebuilds the rows and repaints, and
            // so does a theme change — and because both decide what the next
            // paint looks like. A window that kept the old family would set
            // the new language's text in a font that cannot draw it.
            state.family = family;
            // Kept verbatim alongside the scaled copy: `WM_DPICHANGED`
            // rescales from this, not from `state.theme`, so a later
            // monitor change starts from the same 96-DPI source this
            // scaling did rather than from an already-scaled value.
            state.base_theme = *theme;
            true
        })
        .unwrap_or(false);
        if !changed {
            return;
        }
        // The title-bar icon follows the theme too (its accent is the theme's).
        // SAFETY: `self.hwnd` is alive for as long as this `Window` is.
        unsafe {
            set_window_icon(self.hwnd, theme);
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Replaces the content and brings the window into agreement with it —
    /// both the size of the frame and the pixels inside it — in one repaint.
    /// Called once per poll while the panel is open.
    ///
    /// One method rather than the `update` + `resize` pair the callers used to
    /// invoke back to back. That pair produced two `WM_PAINT`s for a single
    /// change whenever the change touched both text and height — a fault flag
    /// appearing, the notifications master switch being turned off — because
    /// `update` invalidated the window and `SetWindowPos` then invalidated it
    /// again. The ordering that avoids it was never anywhere in the code: it
    /// was the caller's to get right, and nothing said what right was.
    ///
    /// Size first, then pixels. The window class carries
    /// `CS_HREDRAW | CS_VREDRAW`, so a frame that actually changes size
    /// invalidates its whole client area on its own; the explicit
    /// invalidation is therefore needed only when the size stayed put.
    pub(crate) fn set_content(&self, content: PanelContent) {
        let (width, height) = (content.width, content.height);
        let changed = self.store_content(content);
        let resized = self.resize_frame(width, height);
        if changed && !resized {
            // SAFETY: `self.hwnd` is alive for as long as this `Window` is; a
            // whole-window invalidation carries no pointer.
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    /// Stores new content, returning whether it differed from what was held.
    ///
    /// Repaints nothing: scheduling the repaint is [`Window::set_content`]'s job,
    /// because only it knows whether the resize is about to do it anyway.
    ///
    /// The diff against the currently held content is the point: rebuilding the
    /// rows every poll is cheap, but invalidating the window every poll is not,
    /// so the repaint is scheduled only when the content differs. A UPS sitting
    /// on mains reports the same voltage, load and charge for minutes at a
    /// time, so the idle case — nearly all of them — costs a comparison instead
    /// of a full double-buffered redraw for no visible change.
    ///
    /// The whole struct is compared, not a hand-written list of its fields.
    /// Keeping the old content when the new one differs in a field the diff
    /// forgot to look at is a stale window with nothing to notice it by, and a
    /// list of fields written out beside a struct that already has one is a
    /// list that the next field has to be remembered into. Derived equality
    /// cannot forget: adding a field to [`PanelContent`] adds it here.
    fn store_content(&self, content: PanelContent) -> bool {
        // One transaction, and what it decided comes back as a value. The
        // second reach — "does the window still have a focused field" — used to
        // be its own `with_state` after this one returned, which made the
        // answer a fact about a state that had been released and could have
        // been changed by any message in between. It is the state this closure
        // is already holding that the question is about.
        let Some((changed, blink_over)) = with_state(self.hwnd, |state| {
            let differs = state.content != content;
            if differs {
                state.content = content;
                // Focus is an id, and the rows that gave it meaning have just
                // been replaced. A control can disappear between one build and
                // the next — the per-event switches when the master switch goes
                // off, the self-test button when it greys out — and focus left
                // pointing at one would be a ring drawn nowhere and a Space
                // that activates nothing. It returns to where the window
                // opened, with the ring hidden again, which is the one state
                // that is always meaningful.
                if state
                    .focus
                    .is_some_and(|id| !crate::ui::focus::contains(&state.content.rows, id))
                {
                    state.focus = crate::ui::focus::initial(&state.content.rows);
                    state.focus_visible = false;
                }
            }
            (differs, state.focused_field().is_none())
        }) else {
            return false;
        };
        if changed && blink_over {
            // The blink follows focus: if the field holding it was the control
            // that vanished, the timer must stop with it or an idle dialog goes
            // on waking the loop twice a second. `KillTimer` is a Win32 call and
            // reaches back into the state for the caret phase, so it happens
            // out here, after the borrow above has been released — the same
            // split between deciding and acting that the message handlers use.
            stop_caret_blink(self.hwnd);
        }
        changed
    }

    /// Updates the scroll offset of an open list without rebuilding anything.
    ///
    /// Returns false if the window has no such list, so the caller can fall
    /// back to the full path.
    ///
    /// This is the whole optimisation for dragging. The general path rebuilds
    /// every row from the config and the locale — twenty-four language names,
    /// seventy-odd heap allocations, 26µs — then measures the result and
    /// resizes the frame. Scrolling a list changes one integer and cannot
    /// change any of that: the rows, their text, the column widths and the
    /// window's size are all exactly what they were. So the fast path writes
    /// the integer into the rows already held and asks for a repaint.
    pub(crate) fn set_list_scroll(&self, id: HotspotId, offset: usize) -> bool {
        // One transaction again: the region to invalidate is read here, beside
        // the change that makes it dirty, rather than in a second reach after
        // this one has let go.
        let Some((changed, region)) = with_state(self.hwnd, |state| {
            (
                scroll_list(&mut state.content.rows, id, offset),
                state.painted_list().map(|l| l.bounds),
            )
        }) else {
            return false;
        };
        if changed {
            // SAFETY: `self.hwnd` is alive, and the rectangle is a live local.
            unsafe {
                // Only the list's own rectangle, not the whole client area.
                //
                // `InvalidateRect(None)` marks every pixel dirty, so the next
                // `WM_PAINT` redraws all the rows, both columns, every border
                // and the whole double buffer — to move one highlight and one
                // thumb. Scrolling cannot change anything outside the open
                // list's frame, so nothing outside it needs redrawing.
                //
                // This is the technique the comparison with Adobe is really
                // about, and it is not GPU rendering: it is repainting only
                // what changed. The saving is real because `paint` clips the
                // double buffer to the update region and blits only that
                // rectangle — until it did, this call marked a smaller region
                // dirty and the painter went on drawing the whole window into
                // the buffer anyway, so all this bought was a smaller final
                // blit. Narrowing the region is only half of the technique;
                // honouring it is the other half.
                match region {
                    Some(rect) => {
                        let _ = InvalidateRect(Some(self.hwnd), Some(&rect.into()), false);
                    }
                    None => {
                        let _ = InvalidateRect(Some(self.hwnd), None, false);
                    }
                }
            }
        }
        changed
    }

    /// Takes keyboard edits aimed at the focused field.
    pub(crate) fn take_edits(&self) -> Vec<(HotspotId, Edit)> {
        with_state(self.hwnd, |st| std::mem::take(&mut st.typed)).unwrap_or_default()
    }

    /// Resizes the window to fit new content. The settings view is taller than
    /// the status view, so switching between them must move the frame or the
    /// buttons fall outside the client area.
    ///
    /// `width` and `height` are the client area, already scaled by the caller
    /// (`main.rs` measures against `panel.dpi()`); `Role::frame` adds the
    /// frame this window's own styles and own DPI call for. An unscaled frame
    /// wrapped around already-scaled content is what left the window a few
    /// pixels short on every repaint at 150%, however correctly the content
    /// inside it was sized.
    fn resize_frame(&self, width: i32, height: i32) -> bool {
        let dpi = self.dpi();
        // SAFETY: `self.hwnd` is alive for as long as this `Window` is; every size
        // below is arithmetic on values.
        unsafe {
            let (want_w, want_h) = R::ROLE.frame((width, height), dpi);

            // Resizing to the size it already is still costs a frame recalc,
            // a non-client repaint and a WM_PAINT. Most settings clicks — a
            // checkbox, a theme cycle — do not change the height at all, so
            // the common case is skipped entirely and the panel stops
            // flickering on every interaction.
            let mut current = RECT::default();
            if GetWindowRect(self.hwnd, &mut current).is_ok()
                && current.right - current.left == want_w
                && current.bottom - current.top == want_h
            {
                return false;
            }

            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                want_w,
                want_h,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            true
        }
    }

    /// Takes a pending hotspot click, if any.
    pub(crate) fn take_click(&self) -> Option<HotspotId> {
        with_state(self.hwnd, |st| st.clicked.take()).flatten()
    }

    /// True once the user has pressed the close button.
    pub(crate) fn close_requested(&self) -> bool {
        with_state(self.hwnd, |st| st.close_requested).unwrap_or(false)
    }

    /// Brings the window to the front. Used when the user clicks the tray or
    /// the panel while a modal settings dialog is already open: the correct
    /// response is to surface the dialog, not to ignore the click.
    pub(crate) fn focus(&self) {
        // SAFETY: both windows are alive — this dialog's own, and the owner it
        // borrows.
        unsafe {
            // Raise within the normal Z-order band (HWND_TOP), not into the
            // topmost band: the dialog must come above its owner panel, not
            // above every other application's windows. SetForegroundWindow
            // activates it, and the owner relationship keeps it over the panel.
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetForegroundWindow(self.hwnd);
            let _ = FlashWindow(self.hwnd, true);
        }
    }

    /// Current position, so it can be remembered across sessions.
    pub(crate) fn position(&self) -> Option<(i32, i32)> {
        let mut rect = RECT::default();
        // SAFETY: `self.hwnd` is alive; `rect` is a live local the call fills in.
        unsafe {
            if GetWindowRect(self.hwnd, &mut rect).is_err() {
                return None;
            }
        }
        Some((rect.left, rect.top))
    }
}

/// Takes `hwnd`'s state back and reduces it to what still has to be freed by
/// hand.
///
/// The state itself is dropped here rather than handed on. Everything in it
/// releases itself — the two fonts are a thread-local `OnceCell` shared by
/// every window on this thread and outlive any one of them, and the rows are
/// ordinary owned data — with the single exception of the icon pair, which
/// Windows owns until `DestroyIcon` says otherwise. Reducing to that pair is
/// what the caller actually needs, and it keeps this answer to a couple of
/// words rather than the six hundred bytes of a window.
///
/// The three outcomes are [`Detached`]'s, unchanged: only the first carries
/// anything to free, and only the last is a defect.
fn release_state(hwnd: HWND) -> Detached<[Option<HICON>; 2]> {
    detach_state(hwnd).map(|state| state.window_icons)
}

/// Gives back everything `hwnd` owns: its state, and the title-bar icons the
/// state was holding.
///
/// Called from two places, which is the point of it being a function.
/// [`Window::drop`] calls it on the ordinary path, before `DestroyWindow`.
/// `WM_NCDESTROY` calls it because the ordinary path is not the only one: a
/// modal is created with an owner, and Windows destroys owned windows together
/// with their owner. If the panel's handle is dropped before the modal's, the
/// modal's window is already gone by the time its own `Window::drop` runs, and
/// everything the state held — the box, and two `HICON`s Windows will not
/// reclaim — leaked with nothing to show for it.
///
/// Calling it twice is the normal case and costs nothing: the first call
/// zeroes the slot, so the second reads `Absent` and returns. `Window::drop`
/// therefore goes back to being a request to destroy a window rather than the
/// only moment anything is freed.
pub(super) fn release_window_state(hwnd: HWND) {
    match release_state(hwnd) {
        Detached::Owned(icons) => {
            for icon in icons.into_iter().flatten() {
                // SAFETY: each handle came from `create_hicon` in this process
                // and was recorded as owned; a resource icon is never recorded,
                // so nothing here belongs to the module.
                unsafe {
                    let _ = DestroyIcon(icon);
                }
            }
        }
        // Already released — by `Window::drop` before `DestroyWindow`, or by an
        // earlier `WM_NCDESTROY` — or never attached at all. Nothing to do
        // either way.
        Detached::Absent => {}
        Detached::Blocked => {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "internal: window state was already borrowed while closing a window; \
                 the state and its icons were leaked",
            );
        }
    }
}

impl<R: WindowRole> Drop for Window<R> {
    /// Takes the state out first, then destroys the window.
    ///
    /// The order is the whole of it, and it used to be the other way round.
    /// `DestroyWindow` is synchronous: it sends `WM_DESTROY` and `WM_NCDESTROY`
    /// to the window procedure before it returns, and those arms reach for the
    /// state through `with_state`. Destroying first therefore ran the teardown
    /// messages against a live state belonging to a window that was already
    /// going away.
    ///
    /// What comes back out, and what is left to free by hand, is
    /// [`release_state`].
    fn drop(&mut self) {
        release_window_state(self.hwnd);

        // SAFETY: `self.hwnd` is alive until `DestroyWindow` below, and `owner` is
        // the panel this dialog borrows, which outlives it.
        unsafe {
            // The owner is re-enabled *before* the dialog is destroyed.
            // Doing it the other way round lets Windows pick some other
            // process's window as the next active one, which is seen as the
            // panel flashing behind another application on every close.
            if let Some(owner) = self.owner {
                MODAL_OWNER.with(|m| m.set(None));
                let _ = EnableWindow(owner, true);
                let _ = SetForegroundWindow(owner);
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Chooses where a new window goes, correctly on a multi-monitor desktop.
///
/// # The bug this replaces
///
/// The old code centred with `SM_CXSCREEN` / `SM_CYSCREEN`, which report the
/// dimensions of the *primary* monitor only. A user who dragged the panel to
/// their second screen and closed it got it back centred on the first one, and
/// the saved coordinates were only honoured when they happened to be positive
/// — a monitor placed to the left of or above the primary one has negative
/// virtual-desktop coordinates, so those positions were rejected as invalid
/// and thrown away.
///
/// # What it does instead
///
/// A saved position is honoured whenever it still lands on a monitor that is
/// currently attached, whatever the sign of its coordinates. That check is the
/// point: a window restored onto a display that has since been unplugged is
/// invisible and unreachable, which looks exactly like a crash.
///
/// With no usable saved position, the window is centred on the monitor holding
/// the cursor rather than on the primary one. On a single-monitor machine that
/// is the same thing; on a multi-monitor desktop it puts the window on the
/// screen the user is currently working on, which is where they are looking.
fn place_window(pos: Option<(i32, i32)>, w: i32, h: i32) -> (i32, i32) {
    if let Some((x, y)) = pos {
        if position_is_visible(x, y, w, h) {
            return (x, y);
        }
    }
    centre_on_active_monitor(w, h)
}

/// True when this rectangle intersects a currently attached display.
///
/// `MonitorFromRect` with `MONITOR_DEFAULTTONULL` returns null precisely when
/// the rectangle lies entirely off every monitor, which is the condition worth
/// rejecting. Partial overlap is accepted deliberately: a window nudged a
/// little past an edge is still reachable and still where the user left it,
/// and shunting it to the centre would be more disruptive than the overhang.
///
/// Safe: it asks the system a question about four integers.
fn position_is_visible(x: i32, y: i32, w: i32, h: i32) -> bool {
    use windows::Win32::Graphics::Gdi::{MonitorFromRect, MONITOR_DEFAULTTONULL};

    let rect = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    // A title bar dragged above the top of the virtual desktop cannot be
    // grabbed again, so an off-screen top is rejected even when the body of
    // the window overlaps a monitor.
    // SAFETY: `rect` is a live local; the flag is by value.
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) };
    !monitor.is_invalid() && y >= virtual_top()
}

/// Top edge of the virtual desktop. Negative when a monitor sits above the
/// primary one.
///
/// Safe: it takes no arguments, so there is no precondition to place on a
/// caller. It was `unsafe` only because it reads a system metric.
fn virtual_top() -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN;
    // SAFETY: reads one system metric by index.
    unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) }
}

/// Centres on the monitor under the cursor, falling back to the primary one.
///
/// The work area is used rather than the full monitor rectangle, so the window
/// is centred in the space actually available to it and never lands under the
/// taskbar.
///
/// Safe: it turns a wanted size into a position by asking the system where the
/// monitors are.
fn centre_on_active_monitor(w: i32, h: i32) -> (i32, i32) {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a live local the call fills in.
    let monitor = if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
        // SAFETY: the point is passed by value.
        unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY) }
    } else {
        // No cursor position — a locked session, or an input desktop we
        // cannot query. The primary monitor is the only sensible answer.
        // SAFETY: as above.
        unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
    };

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `monitor` came from the call above, and `info` is a live local
    // with its `cbSize` already set.
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return Rect::from(info.rcWork).centred_origin(w, h);
    }

    // Last resort: the primary monitor's own metrics. Reached only if the
    // monitor query fails outright, which it should not.
    //
    // Through the same formula as the branch above, over a rectangle at the
    // origin. It used to spell the arithmetic out a second time, which held
    // only because the primary monitor's top-left *is* the origin — true, and
    // true for a reason this line had no way to state.
    // SAFETY: two system metrics read by index.
    let screen = unsafe {
        Rect::new(
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        )
    };
    screen.centred_origin(w, h)
}

/// DPI of the monitor at a point, falling back to 96 (100%) if the query
/// fails.
///
/// Used on a window's top-left corner, after `place_window` has already
/// decided where it goes, to find the DPI it should be built at — so this
/// asks about the monitor the window is actually going to, rather than
/// re-deriving "the active monitor" a second time by another route.
/// `MONITOR_DEFAULTTOPRIMARY` mirrors `centre_on_active_monitor`: a point
/// that somehow lands off every monitor resolves to the primary one rather
/// than failing.
///
/// A failure of `GetDpiForMonitor` itself — the API existing but declining
/// to answer — falls back to 96 for the same reason `set_dpi_awareness`
/// treats its own failure as non-fatal: the window still renders, just
/// without the scaling this exists to get right, which is a cosmetic
/// degradation rather than a reason to refuse to open at all.
///
/// Safe: it answers a question about a point on the virtual desktop.
fn dpi_for_point(x: i32, y: i32) -> Dpi {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    // SAFETY: the point is passed by value.
    let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTOPRIMARY) };
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    // SAFETY: `monitor` came from the call above; both outputs are live locals.
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
        && dpi_x > 0
    {
        Dpi::new(dpi_x)
    } else {
        Dpi::BASELINE
    }
}

thread_local! {
    /// The window currently pinned below a modal dialog, if any.
    ///
    /// Consulted from the window procedure, which has no access to the owning
    /// the handle, to veto Z-order changes on the disabled owner.
    ///
    /// `Option<HWND>` rather than an `isize` that means "nobody" when it is
    /// zero. Two things follow. The handle stops making a pointer → integer →
    /// pointer round trip on every comparison, which is what it did once
    /// `windows` 0.62 stopped defining `HWND.0` as an integer. And "no modal is
    /// up" becomes a value the type can express, so the reader no longer needs
    /// the `!hwnd.0.is_null()` that used to sit beside the comparison — that
    /// guard existed only because zero and "nobody" were the same `isize`.
    static MODAL_OWNER: std::cell::Cell<Option<HWND>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    /// Whether the panel class has been registered on this thread.
    static CLASS_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The window's client area, in client coordinates.
///
/// Lives here rather than inside `paint` because of when it has to happen: the
/// painter runs inside the `with_state` borrow, and anything it can reach the
/// window through is a way back into state it is already holding. Measuring
/// first and passing the answer in removes the handle from that scope
/// altogether — see [`paint`](paint::paint).
///
/// `None` on failure, which the caller treats as nothing to paint. A window
/// that cannot report its own size is one that is going away.
pub(super) fn client_rect(hwnd: HWND) -> Option<Rect> {
    let mut measured = RECT::default();
    // SAFETY: `hwnd` names a window to the system, which validates it, and
    // `measured` is a live local the call fills in.
    unsafe { GetClientRect(hwnd, &mut measured) }.ok()?;
    Some(Rect::from(measured))
}

/// Registers the window class once per process.
///
/// Re-registering fails harmlessly, but it still crosses into the window
/// manager on every open. The guard keeps the open path free of calls whose
/// only possible outcome is a discarded error.
///
/// Safe: it takes no arguments, so there is no precondition to place on a
/// caller.
fn register_class() {
    if CLASS_READY.with(Cell::get) {
        return;
    }

    // SAFETY: `None` asks for this executable's own module, which always exists.
    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    // The class icons matter even though every window also sets its own via
    // WM_SETICON. The probe showed `class[hIcon=0x0 hIconSm=0x0]` for this
    // class while the tray class had both, and the class icon is what the
    // shell falls back to whenever it inspects a window without asking it.
    // Loaded from the exe resource so all three surfaces agree by
    // construction.
    // SAFETY: reads one system metric by index.
    let icon_px = unsafe { GetSystemMetrics(SM_CXICON) }.clamp(16, 256);
    let big = crate::ui::appicon::resource_icon(icon_px);

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.into(),
        lpszClassName: w!("ups_monitor_panel"),
        // SAFETY: a null module with a system cursor id is how a stock cursor is
        // asked for; the handle belongs to the system and is not released here.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hIcon: big
            .map(crate::ui::appicon::WindowIcon::handle)
            .unwrap_or_default(),
        ..Default::default()
    };
    // `RegisterClassW` returns 0 on failure. `CLASS_READY` is set only after
    // it succeeds — set eagerly before the call, a failed registration (or an
    // early return above) would leave the flag true, the guard would skip the
    // retry, and the panel's `CreateWindowExW` would fail on every open with
    // nothing saying why.
    // SAFETY: `wc` is a fully initialised class description, live for the call.
    // The strings inside it are static literals.
    if unsafe { RegisterClassW(&wc) } == 0 {
        // SAFETY: reads this thread's last-error value.
        let err = unsafe { GetLastError() };
        crate::evlog::event(
            crate::evlog::Cat::Error,
            &format!("panel window class registration failed ({:#06x})", err.0),
        );
        return;
    }
    CLASS_READY.with(|c| c.set(true));
}

/// Starts the caret blink timer for `hwnd`.
///
/// The one deliberate exception to the utility's "nothing on a timer" rule: a
/// blinking caret cannot be expressed without a periodic wake. It is confined
/// as tightly as the effect allows — the timer exists only while a text field
/// holds focus, so it runs only when the settings dialog is open *and* the
/// pointer or keyboard has entered the interval field, never in the tray and
/// never on an idle dialog. The period is the system caret blink time, so the
/// caret matches every other one on the machine. `SetTimer` with an existing
/// id just resets it, so calling this when already running is harmless.
///
/// Safe: `hwnd` names a window to the system, which validates it, and nothing
/// here reads through it. The same stance [`with_state`] already takes.
pub(super) fn start_caret_blink(hwnd: HWND) {
    // SAFETY: reads a system-wide setting; no arguments.
    let mut period = unsafe { GetCaretBlinkTime() };
    // 0 means "do not blink" (a system accessibility setting); INFINITE is
    // reported as that too. Fall back to a sane rate so the caret is at least
    // steady-on rather than the timer firing wildly.
    if period == 0 || period == u32::MAX {
        period = 530;
    }
    // The id is not checked here, unlike the refresh timer's. A caret timer
    // that fails to start leaves the caret solid on — exactly what the system
    // gives a user who has turned blinking off — so there is no degradation to
    // report and nothing for the user to do about it.
    // SAFETY: `hwnd` names a window to the system; the callback is `None`, so
    // the tick arrives as `WM_TIMER` and no function pointer outlives anything.
    let _ = unsafe { SetTimer(Some(hwnd), crate::ui::timers::CARET, period, None) };
}

/// Stops the caret blink timer and leaves the caret in its visible state, so
/// the field it just left is not frozen mid-blink with the caret hidden.
pub(super) fn stop_caret_blink(hwnd: HWND) {
    // SAFETY: cancels the timer above by the same id on the same window.
    let _ = unsafe { KillTimer(Some(hwnd), crate::ui::timers::CARET) };
    with_state(hwnd, |state| state.caret_visible = true);
}

/// Forces the caret visible and restarts its blink interval from zero.
///
/// Called on every caret interaction — a keystroke, a deletion, an arrow, a
/// mouse click or drag — so the caret is solid the instant the user acts on
/// it and only resumes blinking after the following idle interval. This is
/// exactly what a native text control does: were the phase left to run free,
/// a key pressed during the off-phase would move a caret the user cannot see.
/// `SetTimer` with an existing id restarts that timer's countdown, so the
/// next blink is a full interval away from this action rather than whatever
/// was left of the previous cycle.
pub(super) fn reset_caret_blink(hwnd: HWND) {
    with_state(hwnd, |state| state.caret_visible = true);
    start_caret_blink(hwnd);
}

/// Puts the application mark in the title bar and the Alt+Tab list.
///
/// Both sizes are set: `ICON_SMALL` is the title bar and the taskbar, and
/// `ICON_BIG` is Alt+Tab. Setting only one leaves Windows to scale the other,
/// which is visibly soft.
///
/// Sized with `GetSystemMetricsForDpi` against this window's own
/// `state.dpi`, not the plain `GetSystemMetrics`. The plain call answers for
/// whatever DPI the calling thread currently carries, which is not
/// necessarily the DPI of the monitor this particular window is on — the
/// same gap `AdjustWindowRectExForDpi` closes for the frame.
///
/// The mark is then *rendered* at exactly that size rather than loaded from
/// the executable's icon resource. The renderer draws the same analytic
/// shapes at any size, so every DPI step gets an exact bitmap and none of
/// them costs a byte in the binary. `LoadImageW` can only return one of the
/// sizes `build.rs` baked in; when the request falls between them Windows
/// stretches the nearest to fit, which is soft edges on an otherwise crisp
/// window, next to a caption the DWM always draws at the right size. An
/// earlier version preferred the resource and answered that softness by
/// baking every size the 100–200% range can ask for — nine of them, about
/// 41 KB — which is the wrong half of the problem to fix when a renderer is
/// already in the process.
///
/// `resource_icon` stays as the fallback for a failed `create_hicon`, so a
/// GDI failure cannot leave the title bar blank. The resource itself remains
/// necessary, just not here: Explorer and a windowless Task Manager entry
/// read it off the file on disk and never ask this process anything.
///
/// Called on window creation, on theme change (`apply_style`) and on
/// `WM_DPICHANGED` — the three moments at which the right size or the right
/// palette can change. Not from the poll timer that repaints the panel every
/// few seconds: re-rendering the mark there would be work that cannot change
/// what is on screen.
///
/// Safe: the only handles destroyed are the ones a previous call created and
/// recorded in the window's own state, which `WindowIcon::owned` distinguishes
/// from the module-owned resource icon. Nothing a caller passes decides that.
pub(super) fn set_window_icon(hwnd: HWND, theme: &Theme) {
    use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;

    let dpi = with_state(hwnd, |st| st.dpi).unwrap_or(Dpi::BASELINE);
    // SAFETY: reads one DPI-scaled system metric; both arguments are by value.
    let small = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi.raw()) }.clamp(16, 64) as u32;
    // SAFETY: as above.
    let big = unsafe { GetSystemMetricsForDpi(SM_CXICON, dpi.raw()) }.clamp(16, 256) as u32;

    // Icons this call creates and hands to the window, `[small, big]`. Only a
    // rendered icon is owned; a resource icon belongs to the module.
    let mut created: [Option<HICON>; 2] = [None, None];

    for (slot, (which, size)) in [
        (crate::ui::appicon::IconSlot::Small, small),
        (crate::ui::appicon::IconSlot::Big, big),
    ]
    .into_iter()
    .enumerate()
    {
        // Render at the exact requested size; fall back to the exe resource
        // only if that fails, so a GDI failure cannot blank the title bar.
        // `WM_SETICON` stores the handle rather than copying it, so a rendered
        // icon is the window's from here — recorded below and destroyed on the
        // *next* call, not here, which would blank the title bar. A resource
        // icon records nothing: `WindowIcon::owned` is what tells the two
        // apart, so this loop does not have to.
        let rgba = crate::ui::appicon::render_rgba(theme, size);
        let icon = crate::ui::tray_window::create_hicon(&rgba, size)
            .map(crate::ui::appicon::WindowIcon::Owned)
            .or_else(|| crate::ui::appicon::resource_icon(size as i32));
        if let (Some(icon), Some(out)) = (icon, created.get_mut(slot)) {
            *out = crate::ui::appicon::set_window_icon(hwnd, which, &icon);
        }
    }

    // Swap the newly created icons into the window state and destroy the pair
    // the previous call left. Destroying only *after* the new icons are set
    // means the title bar is never momentarily iconless, and destroying at all
    // is what stops this leaking one pair of `HICON`s per theme or DPI change —
    // the old code never freed them, on the theory the window did not own them.
    let previous = with_state(hwnd, |state| {
        std::mem::replace(&mut state.window_icons, created)
    });
    if let Some(previous) = previous {
        for old in previous.into_iter().flatten() {
            // SAFETY: each handle was created by a previous call to this function and
            // recorded as owned; the window no longer refers to it.
            let _ = unsafe { DestroyIcon(old) };
        }
    }
}

/// Re-measures `rows` and the window frame that holds them, at `dpi`.
///
/// Shared by `Window::create` (rebuilding a provisional 96-DPI window before
/// it is ever shown) and `wnd_proc`'s `WM_DPICHANGED` handler (rebuilding an
/// already-visible one that moved to a monitor of another DPI). Naming the
/// step once and calling it from both is what keeps the two paths from
/// quietly drifting into two different ideas of what "the right size for
/// this DPI" means — the failure this whole feature exists to close, one
/// level up.
///
/// `theme` is the 96-DPI baseline, `min_width` likewise (`theme.metrics.panel_width`
/// or `theme.metrics.settings_width`, picked by the caller) — both are scaled here,
/// together, so the width floor moves by exactly the factor the content
/// around it does. Returns the rebuilt content and the outer window size
/// (`AdjustWindowRectExForDpi` already applied), ready for `CreateWindowExW`
/// or `SetWindowPos`.
///
/// `role` is what the frame allowance is computed from (`Role::frame`), so a
/// modal's shorter tool-window caption and thicker dialog border are accounted
/// for rather than the panel's being assumed. Content sized correctly by
/// `scaled` inside a window sized incorrectly by the frame allowance is the
/// undersized-window symptom this whole path exists to close, and the styles
/// are one more way to get that allowance wrong.
///
/// Safe: measuring creates and releases its own device context (see
/// [`with_metrics`](crate::ui::text::with_metrics)), and the frame allowance is
/// arithmetic over styles and a DPI.
pub(super) fn rebuild_content_and_frame(
    rows: Vec<Row>,
    samples: Vec<Row>,
    theme: &Theme,
    family: Option<&'static str>,
    min_width: i32,
    dpi: Dpi,
    role: Role,
) -> (PanelContent, (i32, i32)) {
    let content = PanelContent::measure(
        rows,
        samples,
        theme,
        min_width,
        script_of(theme, family, dpi),
    );
    let size = role.frame((content.width, content.height), dpi);
    (content, size)
}

/// The font choices a window drawn with `theme` at `dpi` is measured and
/// painted under.
///
/// Written once because the three facts have to be the same three at every step
/// of a repaint: fonts created at one size and a layout measured at another
/// produce a window whose rows do not fit the text in them. The size is the
/// theme's own — it used to be read from a process-wide cell that the theme
/// loader wrote, which is the same number by a longer route and one that a
/// second window, drawn from a theme that had since been replaced, could find
/// already changed under it.
fn script_of(theme: &Theme, family: Option<&'static str>, dpi: Dpi) -> Script {
    Script {
        family,
        dpi,
        points: theme.font_points,
    }
}

/// Moves list `id` to `offset`, clamped to what it can actually show.
///
/// Says whether anything moved, so the caller can skip the repaint when it did
/// not. Apart from the rows themselves this touches nothing, which is what
/// lets it be tested without a window — the geometry the caller invalidates
/// afterwards is read beside this call, not inside it.
///
/// The clamp is not decoration. The scrollbar hands over an offset derived from
/// a pixel position, and the last thumb position maps past the last scroll
/// position by a rounding error; without the ceiling a drag to the very bottom
/// scrolls the list past its own end and paints a gap under the final option.
fn scroll_list(rows: &mut [Row], id: HotspotId, offset: usize) -> bool {
    for row in rows {
        if let Row::Dropdown {
            scroll,
            id: rid,
            options,
            ..
        } = row
        {
            if *rid == id {
                let want = offset.min(last_scroll_offset(options.len()));
                let moved = *scroll != want;
                *scroll = want;
                return moved;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    //! Tests for the window itself: the clamping `scroll_list` applies to an
    //! offset arriving from the scrollbar, and the frame arithmetic each
    //! `Role` implies. Both are reachable without a live window, which is why
    //! they are here and the input behaviour is in [`super::proc`].

    use super::*;
    use crate::testsupport::hot;
    use crate::ui::row::DropdownItem;
    use crate::ui::scrollbar::DROPDOWN_MAX_VISIBLE;

    /// A list cannot be scrolled past its own end.
    ///
    /// The offset arrives from the scrollbar, which computes it from a pixel
    /// position: the bottom of the trough maps past the last scroll position by
    /// a rounding error, so a drag all the way down asks for an offset the list
    /// does not have. Unclamped, the painter then draws from an index beyond
    /// the options and leaves a gap under the last one.
    #[test]
    fn a_list_cannot_be_scrolled_past_its_last_page() {
        let mut rows = vec![dropdown(hot(7), DROPDOWN_MAX_VISIBLE + 3)];
        assert!(
            scroll_list(&mut rows, hot(7), 999),
            "an over-large offset moves"
        );
        assert_eq!(
            scroll_of(&rows),
            3,
            "the last page starts three options in, not at the end of the list"
        );
    }

    /// A list shorter than the window it opens in does not scroll at all.
    ///
    /// `options.len() - visible` is zero here, and it is a *saturating*
    /// subtraction for exactly this case: an unsigned wrap would have turned
    /// "nothing to scroll" into the largest offset there is.
    #[test]
    fn a_list_that_fits_does_not_scroll() {
        let mut rows = vec![dropdown(hot(7), 2)];
        assert!(
            !scroll_list(&mut rows, hot(7), 5),
            "a list that fits cannot move"
        );
        assert_eq!(scroll_of(&rows), 0);
    }

    /// Scrolling to where the list already is changes nothing, and says so.
    ///
    /// The answer is what the caller repaints on: a wheel notch against a list
    /// already at the bottom would otherwise invalidate and redraw the frame
    /// for a scroll position that did not move.
    #[test]
    fn scrolling_to_the_current_offset_reports_no_change() {
        let mut rows = vec![dropdown(hot(7), DROPDOWN_MAX_VISIBLE + 3)];
        assert!(scroll_list(&mut rows, hot(7), 2));
        assert!(
            !scroll_list(&mut rows, hot(7), 2),
            "the same offset twice is not a change"
        );
        assert!(
            !scroll_list(&mut rows, hot(8), 1),
            "an id that is not in the rows is not a change either"
        );
    }

    /// A dropdown of `count` options, closed, scrolled to the top.
    fn dropdown(id: HotspotId, count: usize) -> Row {
        Row::Dropdown {
            label: String::new(),
            options: (0..count)
                .map(|i| DropdownItem {
                    label: format!("option {i}"),
                    native: String::new(),
                    font: None,
                })
                .collect(),
            selected: 0,
            highlighted: 0,
            open: true,
            scroll: 0,
            id,
        }
    }

    /// The scroll offset of the first dropdown in `rows`.
    fn scroll_of(rows: &[Row]) -> usize {
        rows.iter()
            .find_map(|row| match row {
                Row::Dropdown { scroll, .. } => Some(*scroll),
                _ => None,
            })
            .expect("the fixture has a dropdown")
    }

    /// A modal's frame is not a panel's, and the difference is exactly what
    /// the window size depends on.
    ///
    /// `Role::styles` is the single source both `CreateWindowExW` and
    /// `AdjustWindowRectExForDpi` read, which is the property that matters;
    /// this pins the reason it has to be one source. A tool window has the
    /// short caption rather than the full one and a dialog frame is thicker
    /// than a plain one, so computing a modal's frame from the panel's styles
    /// makes its client area too tall and too narrow — by more pixels the
    /// higher the DPI, since caption height and border width are precisely
    /// what `ForDpi` scales. Collapsing the two roles back to one pair would
    /// silently restore that.
    #[test]
    fn modal_and_panel_frames_are_computed_from_different_styles() {
        let (panel_style, panel_ex) = Role::Panel.styles();
        let (modal_style, modal_ex) = Role::Modal.styles();

        assert_eq!(
            panel_ex,
            WINDOW_EX_STYLE(0),
            "the panel has no extended style"
        );
        assert_ne!(
            panel_ex, modal_ex,
            "a modal is a tool window with a dialog frame; a panel is neither, \
             and the two frames are not the same size"
        );
        assert_ne!(
            modal_ex.0 & WS_EX_TOOLWINDOW.0,
            0,
            "modal must be a tool window"
        );
        assert_ne!(
            modal_ex.0 & WS_EX_DLGMODALFRAME.0,
            0,
            "modal must carry the dialog frame"
        );
        assert_ne!(
            panel_style.0 & WS_MINIMIZEBOX.0,
            0,
            "the panel keeps its minimise box"
        );
        assert_eq!(
            modal_style.0 & WS_MINIMIZEBOX.0,
            0,
            "a modal must not offer to hide itself while its owner stays disabled"
        );
    }
}
