//! The message pump and the refresh timer.
//!
//! One pass of the loop is one transaction: messages are drained and
//! classified, then a single `tick` services whichever sides they touched.
//! Draining first is what keeps a burst of input from costing a device pass
//! each.

use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, KillTimer, PeekMessageW, SetTimer, TranslateMessage, MSG, PM_REMOVE, WM_NULL,
    WM_QUIT, WM_TIMER,
};

use super::{ModalKind, Ui};
use crate::evlog;
use crate::ui;
use crate::ui::tray_window::TrayWindow;
use crate::ui::window::PanelWindow;
use crate::ui::winstate::Input;
/// How often the open panel refreshes. Only runs while the panel exists.
const REFRESH_MS: u32 = 1000;

/// Classifies a message as `(device, ui)` work.
///
/// `WM_NULL` is the poll thread's wake — device. `WM_UI_INPUT` is a click or
/// keystroke waiting on a window — ui. `WM_TIMER` is ambiguous: the tray
/// window's timer is the device refresh tick, but the panel and settings
/// windows run a caret-blink timer on the same message with the same id, told
/// apart only by their HWND. A blink is a ui event and must not trigger a
/// device pass — that was servicing the whole device side twice a second for as
/// long as a text field held focus. Anything else is neither.
fn classify(msg: &MSG) -> (bool, bool) {
    match msg.message {
        WM_NULL => (true, false),
        ui::tray_window::WM_UI_INPUT => (false, true),
        WM_TIMER if msg.hwnd == ui::tray_window::tray_hwnd() => (true, false),
        WM_TIMER => (false, true),
        _ => (false, false),
    }
}

impl Ui {
    pub(crate) fn run(&mut self) {
        self.tray = ui::tray_window::TrayWindow::new(&self.app.theme);
        if self.tray.is_none() {
            // Without a tray icon there is no way to reach the application at
            // all, so failing loudly beats running invisibly. But `App::new`
            // has already spawned the poll thread, which is now opening HID
            // handles and posting to a window that does not exist. Returning
            // straight out would leak that thread and cut the log off without a
            // stop line, so this path runs the same teardown as a normal exit:
            // join the thread, save the config, close the session log.
            evlog::event(
                evlog::Cat::Error,
                "tray icon unavailable; the application cannot run and is shutting down",
            );
            self.teardown();
            return;
        }
        self.sync_tray_labels();
        self.refresh_icon();

        // Two independent reasons to show the panel at startup, checked in
        // this order:
        //
        // 1. The user asked for a window rather than the tray.
        // 2. The first pump found the device missing, which the tray icon
        //    alone cannot explain.
        //
        // Independent, not ranked: neither reason suppresses the other, and
        // there is no override between them. The second check simply finds
        // the window already open when the first opened it — that is what
        // `self.panel.is_none()` reads. So "start minimised" is honoured
        // exactly until a missing device gives a reason to speak, which is
        // the behaviour the requirement asks for and is reached by the
        // absence of a precedence rule rather than by one.
        //
        // The pump has to sit between them: reason 2 is a message from the
        // poll thread, and nothing has drained that channel yet.
        if !self.app.config.start_minimized {
            self.open_panel();
        }
        self.app.pump();
        if self.app.take_show_panel_request() && self.panel.is_none() {
            self.open_panel();
        }

        let mut msg = MSG::default();
        loop {
            // Block until something happens. This is the idle state: no
            // frames, no polling of our own, zero CPU.
            // SAFETY: `msg` is a live local the call fills in; `None` for the
            // window filter means every window on this thread, which is what
            // the loop wants.
            let got = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0)
            };
            if got.0 <= 0 {
                break;
            }

            // A message is only a reason to work if it is one of ours. The
            // tray window shares this queue and generates routine traffic;
            // running a full pass per message previously drove more than a
            // thousand passes a second, each repainting the panel.
            //
            // Two kinds of ours, kept apart. `WM_NULL` and the *tray window's*
            // `WM_TIMER` mean the device side may have moved; `WM_UI_INPUT`
            // means a window has a click or a keystroke waiting. A `WM_TIMER`
            // for any other window is the caret blink on the panel or settings
            // dialog — a UI event, not a device one. Classifying every
            // `WM_TIMER` as device let the twice-a-second caret blink drag a
            // full device-servicing pass along behind it whenever a text field
            // was focused; `classify` tells the two timers apart by their HWND.
            let (mut device, mut ui) = classify(&msg);

            // SAFETY: `msg` was filled by the `GetMessageW` above and is live
            // for both calls. Dispatching re-enters the window procedures of
            // this thread's own windows, which is the point of the loop.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Drain what is already queued, so a burst costs one pass total.
            let mut quit = false;
            // SAFETY: as the blocking call above, except that this one returns
            // at once when the queue is empty.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                if msg.message == WM_QUIT {
                    quit = true;
                    break;
                }
                let (d, u) = classify(&msg);
                device |= d;
                ui |= u;
                // SAFETY: as the pair above — `msg` is what `PeekMessageW`
                // just filled in and is live for both calls.
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // A tray click or menu choice sets a flag inside the window
            // procedure rather than posting a message of its own, and the
            // dispatch above is what sets those flags. So this check must
            // happen unconditionally after dispatching, never only when no
            // other work was found: a right-click arrives as WM_TRAY, which
            // is not in the `work` set, and skipping the check here left the
            // command sitting uncollected until the next unrelated wake.
            //
            // It counts as device-side work rather than UI work because it can
            // open the panel, and a panel needs a reading to draw.
            device |= self.tray.as_ref().is_some_and(TrayWindow::has_pending);

            if (device || ui) && !self.tick(device) {
                break;
            }
            if quit {
                break;
            }
        }

        self.teardown();
    }

    /// One pass of application work. Returns false to quit.
    ///
    /// `device` says whether anything device-side may have changed — a poll
    /// message, a timer tick, a tray event. When it is false the wake came
    /// from a window reporting user input, and everything between the close
    /// checks and the input routing is skipped: none of it can have changed.
    ///
    /// The split replaces a flag that used to sit inside `refresh_icon`.
    /// A flag there could stop the panel being *rebuilt*, but the pass still
    /// drained the poll channel, serviced the tray and checked for
    /// notifications on every mouse move. Deciding once, here, at the top, is
    /// both cheaper and easier to reason about: the reason for the wake is
    /// known exactly at this point and nowhere further down.
    fn tick(&mut self, device: bool) -> bool {
        // Closing is settled first. Everything below can repaint the panel,
        // and repainting a window that this same pass is about to destroy is
        // pure waste.
        // The modal is checked first and independently. Closing it leaves the
        // panel exactly as it was — open if it was open, absent if it was
        // not. Nothing here creates a panel.
        let mut modal_closed = false;
        if self
            .modal
            .as_ref()
            .is_some_and(|m| m.window.close_requested())
        {
            self.dispatch(Input::ModalClose);
            modal_closed = self.modal.is_none();
        }
        // Routed through the same table as everything else, which is what
        // makes "the panel cannot be closed while a modal is up" a single
        // rule rather than a condition repeated at each call site. A stale
        // flag raised before the modal opened is refused here too.
        let mut closed = false;
        if self
            .panel
            .as_ref()
            .is_some_and(PanelWindow::close_requested)
        {
            self.dispatch(Input::PanelClose);
            closed = self.panel.is_none();
        }

        // Everything in this block is device-side: it reads the poll channel
        // and everything downstream of it. A wake caused by the mouse cannot
        // change any of it.
        if device {
            self.app.pump();

            if !self.handle_tray() {
                return false;
            }

            self.deliver_notifications();

            self.refresh_icon();
            // The panel shows the reading, so a device-side pass repaints it.
            // refresh_icon no longer does this itself; the two were coupled.
            if self.panel.is_some() {
                self.redraw_panel();
            }

            if self.app.take_show_panel_request() && self.panel.is_none() {
                self.open_panel();
            }
        }

        // Input is routed to the window that produced it, and only if that
        // window survived this pass. The modal routes by kind: the settings
        // dialog runs the full draft/focus machinery; the confirmation has
        // only two buttons and no editable state.
        if !modal_closed {
            match self.modal.as_ref().map(|m| m.kind) {
                Some(ModalKind::Settings) => self.route_settings_input(),
                Some(ModalKind::ConfirmSelfTest) => self.route_confirm_input(),
                None => {}
            }
        }
        if !closed {
            if let Some(panel) = &self.panel {
                let click = panel.take_click();
                // The panel has no editable fields, so keystrokes aimed at it
                // are discarded rather than routed anywhere.
                let _ = panel.take_edits();
                if let Some(id) = click {
                    self.handle_panel_click(id);
                }
            }
        }

        true
    }

    /// The refresh timer exists only while the panel does. A timer running
    /// against a destroyed window would be exactly the kind of background
    /// wake-up this rewrite removed.
    pub(super) fn start_timer(&mut self) {
        if self.timer_active {
            return;
        }
        // `SetTimer` returns the id it created, or 0 on failure. Marking the
        // timer active regardless was a silent trap: the panel would then
        // believe it had a refresh timer, `start_timer` would keep returning
        // early, and the panel would simply stop updating with nothing anywhere
        // saying why. Compared against the id that was *asked* for rather than
        // just against 0, so a timer created under some other id — which this
        // code would then never be able to kill — is caught as well.
        //
        // The handle is checked before the call, not only the result after it.
        // `SetTimer` with a null `hwnd` does not fail: it *ignores the id it
        // was given* and creates a thread timer under a fresh system id. The
        // check below sees the mismatch and writes its line, but the timer has
        // already been created, is not held by anyone who could kill it, and
        // posts `WM_TIMER` into the thread queue until the process ends — where
        // `classify` reads `msg.hwnd == 0 == tray_hwnd()` as the tray window's
        // own timer and runs a full device pass once a second, for ever.
        // Unreachable today (`run` returns early when the tray window is
        // missing), which is exactly why it is worth one line rather than a
        // comment saying it cannot happen.
        let hwnd = ui::tray_window::tray_hwnd();
        if hwnd.0.is_null() {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "panel refresh timer not created: there is no tray window to own it",
            );
            return;
        }
        // SAFETY: `hwnd` names the tray window to the system, which validates
        // it; the callback is `None`, so the tick arrives as `WM_TIMER` on this
        // thread's queue and there is no function pointer to outlive anything.
        let id = unsafe { SetTimer(Some(hwnd), ui::timers::REFRESH, REFRESH_MS, None) };
        if id != ui::timers::REFRESH {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "panel refresh timer could not be created; the panel will not update live",
            );
            return;
        }
        self.timer_active = true;
    }

    pub(super) fn stop_timer(&mut self) {
        if !self.timer_active {
            return;
        }
        // SAFETY: cancels the timer started above by the same id on the same
        // window. Killing a timer that is no longer there simply fails, which
        // is why the result is discarded.
        unsafe {
            let _ = KillTimer(Some(ui::tray_window::tray_hwnd()), ui::timers::REFRESH);
        }
        self.timer_active = false;
    }
}
