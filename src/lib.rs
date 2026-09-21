//! CyberPower CP1350EPFCLCD tray monitor.
//!
//! # Architecture
//!
//! We own the message loop. That is the whole point.
//!
//! Earlier versions used `eframe`, which owns the winit event loop and does
//! not share it, and whose egui layer does not render at all while its window
//! is hidden. "Minimise to tray" cannot be expressed in that model: the window
//! has to be hidden behind eframe's back, after which its loop keeps spinning
//! on a window it no longer believes exists. Eight rounds of instrumentation
//! traced the residual 25-68% CPU to exactly that conflict — the UI thread
//! burned a core *between* frames, outside our code, with every one of our own
//! probes reading idle.
//!
//! Here the panel window is created when shown and destroyed when hidden.
//! Hidden costs nothing because there is nothing left to cost: no window, no
//! device context, no render loop. The thread blocks in `GetMessage`, which is
//! the documented way to idle at zero CPU on Windows, and wakes only when a
//! real message arrives.
//!
//! The tray icon lives on this same thread, so the cross-thread wake
//! machinery that earlier versions needed is gone along with the feedback
//! loops it caused.

mod app;
mod color;
mod config;
mod error;
mod evlog;
mod hid;
/// Icon encoding, shared with `build.rs` through a `#[path]` include there.
///
/// Test-only in the shipping binary: the exe's own icon resource is produced at
/// build time, and nothing at run time writes an `.ico`. Compiling it into the
/// binary would be dead weight; compiling it into the test build is what keeps
/// the byte layout the build script depends on under test.
#[cfg(test)]
mod icoenc;
mod icon;
mod ini;
mod instance;
mod lang;
mod notify;
mod poller;
mod resolved;
mod shell;
mod strings;
#[cfg(test)]
mod testsupport;
mod ui;
mod warning;
mod wide;

use shell::Ui;

/// Marks the process per-monitor-DPI-aware, so Windows never bitmap-scales
/// its windows on a scaled display. Called once, before any window exists.
///
/// `SetProcessDpiAwarenessContext` is the modern entry point and is preferred
/// over an application manifest here because the build script writes its
/// resource block by hand — adding a manifest record to that hand-rolled `.res`
/// would be more fragile than a single documented call at startup, and this
/// keeps the awareness decision in code next to the reason for it. A failure is
/// not fatal: the process simply stays DPI-unaware and the windows are scaled,
/// which is a cosmetic degradation, so the result is intentionally ignored.
///
/// This used to declare `DPI_AWARENESS_CONTEXT_SYSTEM_AWARE` instead, on the
/// reasoning that these windows are small and fixed, and that per-monitor
/// awareness would only add `WM_DPICHANGED` handling for a case — dragging
/// the panel to a monitor of another DPI — that seemed like a small tax to
/// skip. It was not: system awareness resolves *once*, against whichever
/// monitor Windows considers current when the awareness context is set, and
/// that is not reliably the DPI the panel is later drawn at. A tray
/// application in particular can start before the shell has settled on the
/// interactive session's DPI, and there is no signal that tells it to
/// re-resolve — it is handed one DPI for the rest of the process's life.
/// When that DPI turns out wrong for the monitor the window actually lands
/// on, the DWM has no per-monitor awareness to defer to and bitmap-stretches
/// the client area to the display's real scale, which is the blurred-text
/// failure this awareness call exists to prevent in the first place. The
/// title bar stays sharp throughout, because it is non-client content the
/// DWM draws itself, straight from the monitor's real DPI — it is what made
/// the failure diagnosable at all.
///
/// Per-monitor awareness closes that gap: every window carries its own DPI,
/// read fresh from `GetDpiForWindow` right after creation and again on
/// `WM_DPICHANGED`, so the panel is always laid out and drawn at the DPI of
/// the monitor it is actually on. The `WM_DPICHANGED` handling this obliges
/// is a fixed, one-time cost, paid once here — not a per-window one that
/// grows with the feature — and it is the price of the correctness the
/// context is named for, not an optional add-on to skip.
fn set_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // SAFETY: a by-value constant, and the call is process-wide state with no
    // pointer to outlive it. Failure is not fatal — see the note above.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Runs the monitor: claims the single-instance slot, opens the session log,
/// loads the configuration and enters the message loop, returning when the
/// user quits.
///
/// The whole program lives here rather than in `main` so that everything below
/// it is a library: the binary is a shim that sets the subsystem attribute and
/// calls this, and nothing else.
pub fn run() {
    // Declared before any window or device context exists, which is why it is
    // the very first thing the program does — even the "already running" dialog is a
    // window, and DPI awareness set after a window is created does not apply to
    // it. Without this the process is DPI-unaware: on a scaled display the DWM
    // bitmap-stretches every window, so the panel and its text render blurred.
    // Per-monitor awareness is what makes that promise hold on every monitor
    // rather than on whichever one Windows happened to resolve at startup —
    // see the note on `set_dpi_awareness` for why the weaker system-wide
    // level does not. The `WM_DPICHANGED` handling it obliges lives in
    // `ui::wnd_proc`.
    set_dpi_awareness();

    // Claimed before anything else happens. A refused launch must leave the
    // disk exactly as it found it: no INI created, no line logged. Both
    // temptations were real and both were wrong. `Config::load` creates the
    // file when absent — and a second copy started from *another directory*
    // has no file, so the refusal would have planted an INI where nothing is
    // running. A log line has the same flaw from another directory (a fresh
    // log in a dead directory), and in the shared directory it writes into a
    // file owned by the running instance. The dialog is the whole answer.
    //
    // The language is still read, through a read-only peek that touches
    // nothing: the refusal message is a visible string and therefore
    // localized like every other one.
    let mut guard_warning = None;
    let _instance = match instance::acquire() {
        instance::Acquire::First(guard) => Some(guard),
        instance::Acquire::AlreadyRunning => {
            let language = config::Config::peek_language();
            // No logging here on purpose: a second instance must leave nothing
            // behind, and the log is not open yet. An unrecognised code simply
            // gives the English dialog.
            let locale = lang::Locale::by_code(&language).value;
            instance::report_already_running(locale);
            return;
        }
        // The guard itself failed, which is neither of the above. Running
        // unprotected beats refusing to monitor; the degradation is logged
        // once the session log is open, below.
        instance::Acquire::Unavailable(reason) => {
            guard_warning = Some(reason);
            None
        }
    };

    evlog::session_start(env!("CARGO_PKG_VERSION"));
    if let Some(reason) = guard_warning {
        evlog::event(
            evlog::Cat::Error,
            &format!(
                "single-instance guard unavailable: {reason}; a duplicate launch will not be detected"
            ),
        );
    }

    let loaded = config::Config::load();
    // Before anything opens the device: the diagnostics this gates are raised
    // by the first connect and the first poll, which is exactly when a user
    // who turned Debug on to investigate a failing start needs them.
    evlog::set_debug(loaded.config.log_level == config::LogLevel::Debug);
    let mut state = Ui::new(loaded.config, loaded.warning);
    state.run();
    // The guard is dropped here, after `run` has returned and the session has
    // been logged, so the slot is only released once this instance is really
    // finished with the device and the files.
}
