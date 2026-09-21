//! The window and message shell around [`App`].
//!
//! `App` is the application's state and the decisions that follow from it;
//! this is everything that has to happen for those decisions to become windows
//! on a screen. The two were one type until the shell outgrew its own file:
//! a message pump, ownership of every window, routing of tray, panel and modal
//! input, settings commit, notification delivery and a timer, in one `impl` of
//! nine hundred lines where no boundary between them was written down anywhere.
//!
//! The split is by role, and the module boundary is what makes it hold: the
//! fields of [`Ui`] are declared here and reachable from the three submodules
//! because they are its descendants, and from nowhere else. `main.rs` builds
//! one of these and calls [`Ui::run`].

mod pump;
mod routing;
mod windows;

/// Re-exported for one crate-wide invariant test.
///
/// `ui::focus` checks that no row builder in the crate produces two rows of
/// buttons, and it checks it against every builder rather than the two that
/// happen to be reachable — which is the whole value of that test. Nothing in
/// a shipped build calls this from outside `windows`, so the re-export says
/// `cfg(test)` rather than widening the module boundary for good.
#[cfg(test)]
pub(crate) use windows::confirm_rows;

use crate::app::App;
use crate::poller;
use crate::ui::window::{ModalWindow, PanelWindow};
use crate::{config, error, evlog, ui};
/// The Win32 side of the program: windows, the tray, and the message pump.
///
/// The mirror of [`App`]. Everything here is a handle, a saved window position
/// or a piece of "what is on screen right now"; nothing here decides what the
/// program *means*. When a click arrives it is routed to a method on `app`,
/// and when `app` produces an [`Event`](crate::notify::Event) this delivers it as
/// a balloon or a repaint. The division is the reason `app` can be tested
/// without a display.
///
/// It owns `app` rather than borrowing it because it outlives every window: a
/// panel is created and destroyed as the user opens and closes it, while the
/// model persists for the session.
///
/// `last_tooltip` and `last_icon` are not caches for speed. They are what the
/// tray is currently showing, kept so an unchanged value is not written again —
/// `Shell_NotifyIconW` with the same tooltip still makes the balloon flicker
/// on some shells, and the icon handle would be recreated for no reason.
pub(crate) struct Ui {
    app: App,
    panel: Option<PanelWindow>,
    /// The one modal window, when up: the settings dialog or the self-test
    /// confirmation. A separate top-level window with its own lifetime; closing
    /// it never creates, destroys or reveals the panel. There is one field
    /// because there is one modal window — its `kind` says which content it
    /// carries, and nothing outside `dispatch`/routing needs to ask.
    modal: Option<Modal>,
    tray: Option<ui::tray_window::TrayWindow>,
    /// Saved between openings so the panel reappears where the user left it.
    panel_pos: Option<(i32, i32)>,
    /// Same, for the settings dialog. The confirmation is transient and always
    /// centres over the panel, so only settings restores a position.
    settings_pos: Option<(i32, i32)>,
    last_tooltip: String,
    last_icon: Option<crate::ui::tray::IconState>,
    timer_active: bool,
}

/// The one modal window and which content it is showing.
///
/// Settings and the self-test confirmation are the same window mechanism — a
/// dialog-framed modal that disables the panel beneath it — differing only in
/// the rows drawn and in what a confirmation means. Holding them in one field
/// with a `kind` is what lets the window-state table stay ignorant of the
/// difference: it decides "a modal is up" and nothing finer.
struct Modal {
    window: ModalWindow,
    kind: ModalKind,
}

/// What a modal window is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    /// The settings dialog, backed by `app.draft`.
    Settings,
    /// The self-test confirmation: a question and Yes/No. A confirmed Yes runs
    /// the test; any dismissal does nothing.
    ConfirmSelfTest,
}

impl Ui {
    pub(crate) fn new(cfg: config::Config, cfg_error: Option<error::Error>) -> Self {
        // A saved position is anything actually recorded. Negative coordinates
        // are legitimate on a multi-monitor desktop — a display to the left of
        // or above the primary one has them — so they cannot be used to mean
        // "unset". `i32::MIN` is the sentinel instead, being a coordinate no
        // real window can occupy. Whether the position is still usable is
        // decided at open time against the monitors currently attached, not
        // here.
        let panel_pos = if cfg.panel_x != config::POS_UNSET && cfg.panel_y != config::POS_UNSET {
            Some((cfg.panel_x, cfg.panel_y))
        } else {
            None
        };
        // The poll thread nudges our loop by posting a null message to the
        // tray window. GetMessage returns, tick() runs, and the thread goes
        // straight back to blocking. No shared waker, no repaint plumbing.
        //
        // The post itself — the choice of `WM_NULL` and the guard against a
        // tray window that does not exist yet — belongs to `wake_device`, so
        // this is a call rather than a second copy of it. It used to be
        // written out here, which put the one mechanism that reaches
        // `classify`'s device arm in two places with no way to notice that
        // only one of them had been changed.
        // The poll thread is started here and handed over, rather than started
        // inside `App`. `App` is the application's state and the decisions that
        // follow from it, and a type that spawns an OS thread and opens a USB
        // device in its constructor cannot be built for a test of those
        // decisions — which is why the connect/disconnect chain went unchecked
        // end to end. Constructing the dependency here and passing it in also
        // puts the two halves of the wake mechanism in one place: the thread
        // that posts the message and the window that receives it.
        let poll = poller::spawn(
            cfg.vendor_id,
            cfg.product_id,
            cfg.poll_interval_ms,
            ui::tray_window::wake_device,
        );
        let app = App::new(cfg, cfg_error, poll);

        Self {
            app,
            panel: None,
            modal: None,
            tray: None,
            panel_pos,
            settings_pos: None,
            last_tooltip: String::new(),
            last_icon: None,
            timer_active: false,
        }
    }

    /// Joins the poll thread, saves the config, and closes the session log.
    ///
    /// Shared by the normal exit and the early tray-failure return so both
    /// paths bring the poll thread down cleanly rather than leaving it running
    /// until process teardown kills it mid-transfer.
    ///
    /// That every return from `run` reaches here is held by the shape of `run`
    /// — two exits, both calling this — and not by a test. An earlier version
    /// of this comment named one, `every_exit_path_tears_down`, which has never
    /// existed in the tree: a citation of a guarantee is worse than no citation
    /// at all, because it stops the next reader from asking.
    fn teardown(&mut self) {
        // Exiting with the panel open must still record where it was, or the
        // position is only ever saved when the user closes it by hand first.
        // The modal goes first: it re-enables the panel on drop, and a
        // disabled window left behind by an abrupt exit is a window the user
        // cannot interact with.
        //
        // Through `close_modal`, not by clearing the field. That function is
        // where everything owed to a closing modal is collected — the dialog
        // position, the discarded draft, the timer that only the modal needed
        // — and assigning `None` here was a second exit path it knew nothing
        // about. Stopping the timer on the way out is harmless; anything that
        // ever became unwanted at shutdown belongs inside `close_modal`, as a
        // condition it states, rather than being avoided by going around it.
        self.close_modal();
        if let Some(panel) = &self.panel {
            if let Some((x, y)) = panel.position() {
                self.app.config.panel_x = x;
                self.app.config.panel_y = y;
            }
        }
        self.app.shutdown();
        evlog::session_end();
    }
}
