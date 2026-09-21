//! Single-instance guard.
//!
//! A second copy of the utility is never useful and is actively harmful: two
//! processes polling the same UPS double the HID traffic on a device this
//! utility deliberately limits to one read per second, both write the same INI
//! and the same log, and the user gets every balloon twice from two tray icons
//! that look identical.
//!
//! # Why a named kernel object rather than a lock file
//!
//! The requirement is that a copy started *from another directory* is still
//! recognised as a second instance. A lock file beside the executable cannot
//! do that — two directories mean two lock files — and would also leave a
//! stale file behind after a crash, which is exactly the failure mode where
//! the user needs the utility to start.
//!
//! A named mutex in the `Local\` namespace has neither problem. The name is a
//! fixed constant, not derived from the path, so location is irrelevant. The
//! kernel destroys the object when the last handle closes, including on
//! abnormal termination, so there is no stale state to clean up. And it costs
//! nothing at runtime: the handle is opened once at startup and held for the
//! life of the process.
//!
//! # Why ownership, and not `GetLastError`
//!
//! The textbook form of this check is `CreateMutexW` followed by
//! `GetLastError() == ERROR_ALREADY_EXISTS`. It reads the calling thread's
//! error state *after* a call the bindings report as successful, which means
//! it depends on the generated wrapper doing nothing between the syscall and
//! its return. That is an observable property of one version of `windows-rs`,
//! not a documented guarantee of its API. If a later version ever does
//! something in between, the check silently stops firing — and the symptom is
//! two copies of the utility polling one device, which is the exact situation
//! this module exists to prevent, arriving with no error message anywhere.
//!
//! So the question is asked of the kernel instead. The mutex is created
//! requesting initial ownership and then waited on with a zero timeout, which
//! reports a property of the *object*: this process owns it (we created it and
//! are first), or someone else does (a copy is already running). Creation and
//! ownership are serialised by the kernel, so two copies launched in the same
//! instant still get one answer each — the race a probe-then-create sequence
//! would leave open.
//!
//! Ownership brings the abandoned case with it: a wait can report that the
//! owner died without releasing. That means the previous instance is gone and
//! this one has just been handed ownership, so it is treated as being first —
//! the same conclusion the kernel's own bookkeeping has already reached.
//!
//! `Local\` rather than `Global\`: the scope is one user session. Two users
//! logged into the same machine each have their own tray, their own windows
//! and their own copy of the utility, and blocking the second of them would be
//! wrong. `Global\` would also require privileges that a portable utility run
//! from a USB stick cannot assume it has.
//!
//! This uses no registry, no files and nothing outside the process, so the
//! utility stays as portable as it was.

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};

use crate::strings;
/// Fixed name, deliberately not derived from the executable path: a copy
/// started from a different directory must collide with the first one. The
/// GUID suffix makes an accidental collision with an unrelated program's
/// mutex name effectively impossible.
const MUTEX_NAME: windows::core::PCWSTR =
    w!("Local\\ups-monitor-{7C1A9F42-3B6E-4D58-9A0C-5E2F81D34B77}");

/// A held claim on being the only running instance. Dropping it releases the
/// claim, so the next copy started can take it.
pub(crate) struct InstanceGuard {
    handle: HANDLE,
}

/// Outcome of claiming the single-instance slot.
///
/// Three cases, not two. `CreateMutexW` failing outright is neither "we are
/// first" nor "someone else is running" — it is the guard being unavailable,
/// and reporting it as "already running" would show the user a dialog about
/// an instance that does not exist. The caller proceeds without protection
/// instead: a monitoring tool that refuses to monitor because a mutex could
/// not be created has inverted its priorities.
pub(crate) enum Acquire {
    /// This process now holds the slot.
    First(InstanceGuard),
    /// Another instance already holds it.
    AlreadyRunning,
    /// The guard could not be established at all; the reason is carried so
    /// the caller can log it once the session log is open.
    Unavailable(String),
}

/// Claims the single-instance slot.
///
/// The mutex is created either way — a caller that finds one already owned
/// simply closes its handle and exits — and the verdict comes from who owns
/// it, not from thread-local error state. See the module note for why.
pub(crate) fn acquire() -> Acquire {
    acquire_named(MUTEX_NAME)
}

/// [`acquire`], against a caller-chosen object name.
///
/// The name is a parameter for one reason: the test cannot use the real one.
/// It used to, and so the test suite claimed the same kernel object the shipped
/// utility claims — run it on a machine where the utility is running and the
/// test fails, for a reason that has nothing to do with the code under test. A
/// test whose result depends on what else is running on the machine is not a
/// test of anything. `acquire` supplies the constant; the test supplies its own
/// name and stays hermetic.
fn acquire_named(name: windows::core::PCWSTR) -> Acquire {
    use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

    // SAFETY: `name` is a NUL-terminated wide string that outlives the call,
    // and the handle that comes back is taken over by the guard below, which
    // closes it exactly once.
    unsafe {
        // `bInitialOwner = true` only has an effect for the process that
        // actually creates the object; for every later one the flag is ignored
        // and the handle comes back un-owned. That asymmetry is the whole
        // mechanism, and it is a documented property of `CreateMutexW`.
        let handle = match CreateMutexW(None, true, name) {
            Ok(h) => h,
            Err(e) => return Acquire::Unavailable(e.message()),
        };
        // Zero timeout: this is a question, not a wait. A process that already
        // owns the mutex re-enters it immediately, so the answer arrives
        // without blocking in either case.
        match WaitForSingleObject(handle, 0) {
            // We created it, or the previous owner died and the kernel has
            // just handed ownership over. Either way this process is the one
            // running instance.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Acquire::First(InstanceGuard { handle }),
            // Someone else holds it. Closing our handle here is what keeps
            // the first instance's claim intact — the object survives as long
            // as that instance's handle does.
            _ => {
                let _ = CloseHandle(handle);
                Acquire::AlreadyRunning
            }
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: an owned handle from the `CreateMutexW` above, closed exactly
        // once because only this value holds it and only `Drop` closes it.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Tells the user why nothing happened, then returns.
///
/// A second launch that exits silently is indistinguishable from a launch that
/// crashed: the user double-clicks, sees no window, and tries again. The
/// message names the situation and points at the tray, which is where the
/// running instance actually is.
///
/// The text comes from the locale like every other visible string, so a future
/// language gets its own wording. This dialog is the *only* output of a refused
/// launch: the second instance creates no INI and writes no log — from another
/// directory those files would appear in a directory where nothing is running,
/// and in the shared directory they belong to the instance that is.
pub(crate) fn report_already_running(locale: crate::lang::Locale) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    let title = crate::wide::nul_terminated(locale.t(strings::Key::InstanceAlreadyRunningTitle));
    let text = crate::wide::nul_terminated(locale.t(strings::Key::InstanceAlreadyRunning));
    // SAFETY: both strings are NUL-terminated and outlive the call — the box is
    // modal, so it returns only after the user dismisses it.
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            // Topmost and foreground: the dialog is the only thing this
            // process will ever show, and a modal box that opens behind the
            // window the user was looking at is a dialog they never see.
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard must be acquirable at all, or the utility never starts.
    ///
    /// Under its own object name, never the shipped one. Claiming the real
    /// mutex made this test fail whenever the utility happened to be running on
    /// the machine — a red suite caused by the desktop rather than by the code.
    ///
    /// The other half of the contract — that a *second* process is refused —
    /// cannot be reached from here, and is deliberately not faked. A Windows
    /// mutex is re-entrant for its owner, so a second `acquire_named` on this
    /// thread is handed the slot again and reports `First`; asserting anything
    /// about it would be asserting re-entrancy, not exclusion. Proving
    /// exclusion needs a second process, which belongs to an integration test
    /// that launches one rather than to a unit test pretending to have one.
    #[test]
    fn the_slot_can_be_claimed() {
        // Distinct from the shipped name by construction: a different GUID, and
        // a `-test` marker for anyone who meets it in a kernel object list.
        const TEST_NAME: windows::core::PCWSTR =
            w!("Local\\ups-monitor-test-{0F3D6C81-59B4-4A27-8E15-7C90AD22E6F3}");
        assert!(
            matches!(acquire_named(TEST_NAME), Acquire::First(_)),
            "the first instance must be allowed to run"
        );
    }

    /// Both strings resolve to real text in the embedded English locale.
    ///
    /// With `Key` a missing key can no longer compile, so this checks the
    /// weaker remaining property: the text is not empty and is not just the
    /// dotted name echoed back.
    #[test]
    fn message_strings_are_localized_not_keys() {
        use crate::strings::Key;
        let l = crate::lang::Locale::english();
        for key in [
            Key::InstanceAlreadyRunning,
            Key::InstanceAlreadyRunningTitle,
        ] {
            let text = l.t(key);
            assert!(!text.is_empty(), "{} must resolve to text", key.label());
            assert_ne!(text, key.label(), "{} must not echo its key", key.label());
        }
    }
}
