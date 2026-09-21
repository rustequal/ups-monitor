//! Tray icon owned directly through `Shell_NotifyIcon`.
//!
//! The icon and its hidden window live on the **UI thread**, sharing its
//! message loop. See `TrayWindow` for why: an earlier design gave the tray its
//! own thread and pump, which put tray commands in a queue the UI loop never
//! waited on.
//!
//! Some state is still shared through atomics rather than returned directly,
//! because the window procedure is a C callback with no room for a `self`
//! pointer: it writes flags that `poll_events` collects on the next pass.

use std::cell::RefCell;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
    NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, GetCursorPos, GetWindowLongPtrW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterDeviceNotificationW, RegisterWindowMessageW, SetForegroundWindow,
    SetWindowLongPtrW, TrackPopupMenu, UnregisterDeviceNotification, DEVICE_NOTIFY_WINDOW_HANDLE,
    DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HDR, DEV_BROADCAST_HDR_DEVICE_TYPE,
    GWLP_USERDATA, HDEVNOTIFY, HICON, ICONINFO, MF_SEPARATOR, MF_STRING, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_DESTROY, WM_LBUTTONUP, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::ui::gdi::Bitmap;
/// Tray notification callback message.
const WM_TRAY: u32 = WM_APP + 1;

/// Wake posted when a *window* has input for the owner to collect: a click, a
/// keystroke, a scroll.
///
/// Distinct from the `WM_NULL` the poll thread posts, and that distinction is
/// the point. Both used to be `WM_NULL`, so the loop could not tell "the
/// device reported" from "the mouse moved" and ran the same full pass for
/// each: draining the poll channel, servicing the tray, delivering
/// notifications, rebuilding the tray tooltip and icon, and rebuilding the
/// panel. That is the right work for a reading arriving once a second, and
/// almost none of it is right for a mouse move arriving a hundred times a
/// second while a scrollbar is dragged.
///
/// Splitting the signal lets each wake do only what it implies, which is what
/// removes the work rather than merely skipping it with flags further down.
pub(crate) const WM_UI_INPUT: u32 = WM_APP + 2;

/// This process owns exactly one tray icon.
const ICON_ID: u32 = 1;

pub(crate) const CMD_PANEL: u32 = 1001;
pub(crate) const CMD_SETTINGS: u32 = 1002;
pub(crate) const CMD_EXIT: u32 = 1003;

/// `DBT_DEVTYP_DEVICEINTERFACE`; not exposed as a constant by the windows crate.
const DBT_DEVTYP_DEVICEINTERFACE: DEV_BROADCAST_HDR_DEVICE_TYPE =
    DEV_BROADCAST_HDR_DEVICE_TYPE(0x0000_0005);

/// The tray window's handle, for the one reader that is genuinely on another
/// thread.
///
/// The poll thread posts its wake here, and that is the whole of why this is a
/// static: a handle a second thread has to be able to see. The five flags that
/// used to sit beside it were **not** cross-thread — the window procedure is
/// called by `DispatchMessageW` on the UI thread, so they were a bridge between
/// two points of one thread, dressed as atomics because a bare
/// `extern "system"` function has no `self` to reach. What it does have is its
/// `hwnd`, which is what [`TrayState`] is now hung from.
///
/// A pointer rather than an `isize`. Until `windows` 0.62 an `HWND` *was* an
/// `isize` and there was no conversion; now storing one as an integer means a
/// pointer → integer → pointer round trip on every read, which is the weakest
/// pointer discipline a program can have and buys nothing. `AtomicPtr` stores
/// what the handle already is.
static TRAY_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// The id `RegisterWindowMessageW("TaskbarCreated")` returned, or 0 before the
/// window exists. Zero is not a valid message id, so the guard in `wnd_proc`
/// cannot match on an unregistered value by accident.
///
/// Kept a static rather than moved into [`TrayState`]: this is not the window's
/// state but a number the operating system assigns per *process*, the same for
/// every window in it, and storing it per window would be storing one fact in
/// as many places as there are windows.
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

/// Everything the tray window remembers between messages.
///
/// One value, owned by the window through `GWLP_USERDATA`, replacing five
/// process-wide atomics and a thread-local. The difference is not tidiness: a
/// static outlives the window, so a flag raised by a window that is then
/// destroyed is still standing for the next one — and a plain `bool` says what
/// an `AtomicBool` with `Relaxed` on every access was only pretending to be,
/// which is a value read and written on one thread.
///
/// Every field is an *event waiting to be collected*, not a condition: the
/// window procedure raises them and [`TrayWindow::poll_events`] takes them,
/// exactly once each.
#[derive(Default)]
struct TrayState {
    /// The events raised since the owner last collected them.
    ///
    /// One value rather than five fields beside `menu_labels`, because
    /// [`TrayEvents`] is the same five and they were being copied across
    /// one by one. Two structs and a copier are three places to edit when
    /// an event is added and three places for one of them to be missed.
    events: TrayEvents,
    /// Labels for the right-click menu, in the active language. Written by the
    /// owner when the locale changes, read by the window procedure when it
    /// builds the menu.
    menu_labels: [String; 3],
}

impl TrayState {
    /// Records one event. Exhaustive, so a variant added later has to say
    /// which field it lands in.
    fn raise(&mut self, event: TrayEvent) {
        match event {
            TrayEvent::LeftClick => self.events.left_click = true,
            TrayEvent::DeviceChanged => self.events.device_changed = true,
            TrayEvent::TaskbarRecreated => self.events.taskbar_recreated = true,
            TrayEvent::MetricsChanged => self.events.metrics_changed = true,
            TrayEvent::Command(id) => self.events.command = Some(id),
        }
    }
}

/// Something the window procedure noticed that the loop has to hear about.
///
/// A value rather than five assignments spread through `wnd_proc`, and that is
/// what makes the wake unconditional. A tray signal is not a message: it is a
/// flag our own loop collects on its next pass, and the procedure runs on the
/// UI thread but *outside* that pass — so a flag raised without waking the loop
/// sits there until something unrelated happens along. The symptom is a click
/// that takes effect a second later, when the refresh timer next fires, and it
/// has happened here before.
///
/// That used to be guarded by a test that searched the procedure's source for
/// `FLAG.store(` and checked for a `wake_ui()` within a few lines. With one
/// function raising every event, there is one place for the wake to be, and it
/// is there — which is a guarantee rather than a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayEvent {
    LeftClick,
    DeviceChanged,
    TaskbarRecreated,
    MetricsChanged,
    Command(u32),
}

/// Records `event` on the window and wakes the loop to collect it.
fn raise(hwnd: HWND, event: TrayEvent) {
    with_tray_state(hwnd, |state| state.raise(event));
    wake_ui();
}

/// Runs `f` against the tray window's state, if there is a window with one.
///
/// `None` has two causes, exactly as in the panel's own `with_state` — the
/// shape this function was brought into line with. Either no tray window
/// exists, or the message arrived before the state was attached
/// (`WM_NCCREATE` and `WM_CREATE` both do), and that is the ordinary path:
/// there is nothing yet to change. Or the borrow failed, which means this call
/// is nested inside another one on the same window, and that is a defect worth
/// a line in the log.
///
/// The `RefCell` is not decoration. This procedure is reentrant: `show_menu`
/// runs `TrackPopupMenu`, which spins its own modal message loop and
/// dispatches messages straight back here. Handing out a second `&mut
/// TrayState` while the first is live is undefined behaviour, and it was
/// prevented only by `WM_RBUTTONUP` cloning its labels before calling
/// `show_menu` — one comment in one place, which is not what an invariant of
/// this weight should rest on. The panel's state has been held this way since
/// the fourth audit; two implementations of one technique in one tree should
/// be one implementation, and they differed precisely where the difference was
/// dangerous.
fn with_tray_state<R>(hwnd: HWND, f: impl FnOnce(&mut TrayState) -> R) -> Option<R> {
    // SAFETY: the pointer is either null or the one `attach_state` produced
    // from `Box::into_raw` for this window, and `WM_NCDESTROY` — the last
    // message a window receives — takes it back out before freeing it. Nothing
    // else stores or copies it, so it cannot be reached after that point.
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RefCell<TrayState>;
    if state.is_null() {
        return None;
    }
    // SAFETY: as above — the pointer is live for this call, because only
    // `detach_state` frees it and it cannot run while this borrow stands.
    let Ok(mut borrowed) = (unsafe { &*state }).try_borrow_mut() else {
        crate::evlog::event(
            crate::evlog::Cat::Error,
            "internal: tray state was already borrowed; a tray event was skipped",
        );
        return None;
    };
    Some(f(&mut borrowed))
}

/// Gives `hwnd` a fresh [`TrayState`] to own.
///
/// Called once, immediately after the window is created and before anything
/// asks it for an event.
fn attach_state(hwnd: HWND) {
    let state = Box::into_raw(Box::new(RefCell::new(TrayState::default())));
    // SAFETY: `hwnd` is a window of this program's own class, so its
    // `GWLP_USERDATA` slot is ours to use, and it is empty until now.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
}

/// Takes the state back from `hwnd` and destroys it.
///
/// Called from `WM_NCDESTROY`, which is the last message a window ever
/// receives — freeing on `WM_DESTROY` instead would leave the pointer live for
/// the non-client teardown that follows it.
fn detach_state(hwnd: HWND) {
    // SAFETY: the slot holds what `attach_state` put there, and is zeroed here
    // so a second call — or any message that somehow follows — reads null and
    // does nothing.
    unsafe {
        let state = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut RefCell<TrayState>;
        if !state.is_null() {
            drop(Box::from_raw(state));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    fn flag(self) -> NOTIFY_ICON_INFOTIP_FLAGS {
        match self {
            Self::Info => NIIF_INFO,
            Self::Warning => NIIF_WARNING,
            Self::Critical => NIIF_ERROR,
        }
    }
}

/// The tray window's own handle, so other code can exclude it when searching
/// for the application window.
///
/// Read from the poll thread as well as the UI thread — it is the one piece of
/// state here that really does cross a thread boundary.
pub(crate) fn tray_hwnd() -> HWND {
    HWND(TRAY_HWND.load(Ordering::Acquire))
}

/// What the shell's tooltip field holds, in UTF-16 code units.
///
/// `NOTIFYICONDATAW::szTip` is 128 units including the terminator. Named
/// because the text is composed in one module and truncated in another, and a
/// bare `127` in each is two places to change if the field ever grows.
pub(crate) const TIP_MAX_UTF16: usize = 127;

/// `Default` is "nothing happened", which is what a window that is gone — or
/// not yet built — has to report: it cannot have raised an event.
#[derive(Default, PartialEq, Eq)]
pub(crate) struct TrayEvents {
    /// The icon was left-clicked.
    pub left_click: bool,
    /// A HID interface arrived or was removed.
    pub device_changed: bool,
    /// A menu command was chosen; `None` when nothing is waiting.
    pub command: Option<u32>,
    /// The shell announced a new taskbar, meaning every notification icon
    /// registered with the previous one is gone and must be registered again.
    pub taskbar_recreated: bool,
    /// A system-wide setting changed that the tray bitmaps are rendered from —
    /// in practice the small-icon metric, which moves with DPI and with the
    /// non-client metrics the user can change in Display settings.
    ///
    /// The icon cache used to be re-validated on *every* pass of the
    /// application loop, including passes that handled no message at all, and
    /// its cheapness came from a guard inside another module. This is the event
    /// that guard was really waiting for, so it is delivered like every other
    /// one here.
    pub metrics_changed: bool,
}

/// The tray icon, owned by the thread that created it.
///
/// Earlier versions ran this on its own thread with its own `GetMessage`
/// pump. That put tray commands in a queue the UI loop never waited on: the
/// menu opened, the command landed in the window's state, and nobody collected it
/// until some unrelated message happened to wake the UI. Creating the window
/// here means one thread, one queue, and a tray click wakes the loop directly.
pub(crate) struct TrayWindow {
    hwnd: HWND,
    icon: Option<HICON>,
    /// The tooltip last handed to the shell, UTF-16 and already truncated.
    ///
    /// Held here rather than only in the caller because this struct is the
    /// record of what the shell has been told, and `readd` replays exactly
    /// that record. The caller does keep a copy, but only to skip redundant
    /// `NIM_MODIFY` calls — it sends a tooltip when the text *changes*, so
    /// after a re-registration it would send nothing at all and the icon
    /// would sit there unlabelled until the reading happened to differ.
    tip: Vec<u16>,
    notify: Option<HDEVNOTIFY>,
    /// The `[small, big]` pair handed to this window through `WM_SETICON`,
    /// where the handle is ours to free.
    ///
    /// `WM_SETICON` stores the handle rather than copying it, so somebody has
    /// to own it — the same reasoning `WindowState::window_icons` already
    /// follows for the panel's title-bar pair. Here both were simply dropped
    /// on the floor: the fallback path rendered two `HICON`s that nothing ever
    /// destroyed, and the leak was invisible because it happens once per
    /// process and only on the branch where the exe resource fails to load.
    ///
    /// A slot is `None` when the icon came from the exe's own resource.
    /// `LoadImageW` with `LR_SHARED` returns a handle the system owns, and
    /// `DestroyIcon` must not be called on it — which is why this is a pair of
    /// `Option`s rather than a pair of handles.
    window_icons: [Option<HICON>; 2],
}

impl TrayWindow {
    /// Creates the tray window on the calling thread. Must be called from the
    /// thread that runs the message loop.
    pub(crate) fn new(theme: &crate::ui::theme::Theme) -> Option<Self> {
        // SAFETY: `None` asks for this executable's own module, which always exists.
        let instance = unsafe { GetModuleHandleW(None) }.ok()?;
        let class = w!("ups_monitor_tray");

        // The class icon is what Windows falls back to for any window of
        // this class that has not been given its own, and it is also what
        // several shell surfaces query. Registering it here means the
        // hidden tray window carries the mark even though it is never
        // shown.
        // The size the shell asks for in the notification area. Taken from
        // the icon cache's definition rather than restated: this window's
        // icon and the cached state icons go to the same shell surface, and
        // two copies of the rule would show up as a blurred tray icon the
        // first time one of them was edited.
        let icon_px = crate::ui::tray::system_icon_size();

        let class_rgba = crate::ui::appicon::render_rgba(theme, 32);
        // Held in a binding rather than built inline, because whether it
        // has an owner depends on what `RegisterClassW` does next. A class
        // that registers keeps its icon for the life of the process, which
        // is why nothing frees it on the success path. A registration that
        // fails takes nothing — and neither does one that reports
        // `ERROR_CLASS_ALREADY_EXISTS`, where the pre-existing class keeps
        // the icon it was registered with. On those paths the handle is
        // still ours, and it used to be released on no path at all.
        let class_icon = create_hicon(&class_rgba, 32);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            hIcon: class_icon.unwrap_or_default(),
            ..Default::default()
        };
        // `RegisterClassW` returns 0 on failure. The window is registered
        // once per process, so a real failure here means the class the
        // hidden window needs does not exist and `CreateWindowExW` below
        // will fail too — reported there. This logs the earlier, more
        // specific cause. `ERROR_CLASS_ALREADY_EXISTS` is not a failure:
        // it means the class is already usable, which is all the caller
        // needs, so it is not treated as one.
        // SAFETY: `wc` is a fully initialised class description, live for the call;
        // the strings inside it are static literals.
        if unsafe { RegisterClassW(&wc) } == 0 {
            // ERROR_CLASS_ALREADY_EXISTS is not a failure: the class is
            // usable, which is all the caller needs. Any other code is a
            // real failure worth a line.
            // SAFETY: reads this thread's last-error value.
            let err = unsafe { GetLastError() };
            if err != ERROR_CLASS_ALREADY_EXISTS {
                crate::evlog::event(
                    crate::evlog::Cat::Error,
                    &format!("tray window class registration failed ({:#06x})", err.0),
                );
            }
            if let Some(icon) = class_icon {
                // SAFETY: the icon was created for the class above and the class was never
                // registered, so nothing else refers to it.
                let _ = unsafe { DestroyIcon(icon) };
            }
        }

        // Titled as the program, not as the file. Task Manager reads the
        // window text for the group row, so "ups-monitor" here is what
        // produced a row labelled like a filename.
        // WS_POPUP + WS_EX_TOOLWINDOW, not WS_OVERLAPPED.
        //
        // The probe on a real machine showed this window carrying
        // `style=0x04c00000` — WS_CAPTION | WS_BORDER | WS_DLGFRAME, which
        // is what WS_OVERLAPPED expands to — and no WS_EX_TOOLWINDOW. That
        // is the signature of an ordinary top-level application window
        // that merely happens to be hidden, so the shell treated it as one
        // and gave the process a taskbar/Task Manager identity built from
        // it rather than from the notification icon.
        //
        // The documented remedy is WS_EX_TOOLWINDOW: "A tool window does
        // not appear in the task bar or in the window that appears when
        // the user presses ALT+TAB." WS_POPUP drops the caption and frame
        // the window never had any use for — it is zero-sized and never
        // shown; it exists only to receive WM_TRAY and device-change
        // notifications.
        // Registered before the window exists, so no broadcast can
        // arrive while the id the procedure compares against is still
        // zero. The name is a documented, process-independent constant:
        // every caller of `RegisterWindowMessageW` with the same string
        // gets the same id, which is how the shell reaches windows it
        // knows nothing else about.
        TASKBAR_CREATED_MSG.store(
            // SAFETY: a static wide literal, registered as a message name.
            unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            Ordering::Release,
        );

        let (style, ex_style, parent) = hidden_window_styles();
        // SAFETY: the class was registered above, and every string is a static wide
        // literal. The window that comes back is owned by the value being built.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                class,
                w!("UPS Monitor"),
                style,
                0,
                0,
                0,
                0,
                parent,
                None,
                Some(instance.into()),
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            return None;
        };
        // Before anything can raise an event: the window is now able to
        // receive messages, and a message arriving with no state attached
        // is a message whose findings go nowhere.
        attach_state(hwnd);

        // Task Manager picks the process icon in this order: the icon of
        // a visible window, then the notification icon, then the icon of
        // the executable (Raymond Chen, "How does Task Manager choose the
        // icon to show for a process?", 2018-03-01). This utility spends
        // most of its life with no window open at all, so the exe
        // resource — which is where the previous two attempts put all the
        // effort — is the branch Task Manager reaches last and almost
        // never. What it actually reads is this window and the tray icon.
        //
        // WM_SETICON on the hidden window costs nothing and makes the
        // first branch produce the right answer whenever the shell asks
        // about the process rather than a specific visible window.
        // Prefer the exe's own icon resource, falling back to the
        // procedurally rendered mark. Loading from the resource exercises
        // the same RT_GROUP_ICON the shell reads, so the two agree by
        // construction rather than by coincidence.
        // The exe resource first, falling back to a fresh render. Which
        // handle must later be destroyed is not decided here: it travels
        // with the icon, and `WindowIcon::owned` answers it.
        let mut window_icons: [Option<HICON>; 2] = [None, None];
        // The two slots are paired with their descriptions rather than
        // addressed by a running index. `window_icons` exists to be handed to
        // `WindowIcon::owned` as a pair, so the loop writes through the array's
        // own entries: there is no counter here that could name the wrong one,
        // and adding a third slot would be a compile error instead of a silent
        // write past the end.
        for (kept, (which, px)) in window_icons.iter_mut().zip([
            (crate::ui::appicon::IconSlot::Small, 16i32),
            (crate::ui::appicon::IconSlot::Big, 32),
        ]) {
            let icon = crate::ui::appicon::resource_icon(px).or_else(|| {
                let rgba = crate::ui::appicon::render_rgba(theme, px as u32);
                create_hicon(&rgba, px as u32).map(crate::ui::appicon::WindowIcon::Owned)
            });
            if let Some(icon) = icon {
                *kept = crate::ui::appicon::set_window_icon(hwnd, which, &icon);
            }
        }

        // NIF_ICON is set on the *initial* add, not only on the later
        // NIM_MODIFY.
        //
        // The diagnostic log settled this: `NIM_ADD ok=true hIcon=0x0`.
        // The registration succeeded while carrying no icon, so for the
        // window between NIM_ADD and the first NIM_MODIFY the shell held
        // an iconless entry — and Task Manager, which reads the
        // notification icon when a process has no visible window, had
        // nothing to read. Shell_NotifyIcon does not fail in this case:
        // an icon is simply not among the fields NIF_MESSAGE|NIF_TIP
        // declares valid, so `hIcon` was never looked at.
        // The icon `NIM_ADD` is given must be owned like every later one:
        // `set_icon` destroys the previous handle when it swaps in a new
        // one, and `Drop` destroys whatever is current. Leaving this in
        // `icon: None` meant the very first icon was never freed — the first
        // `set_icon` replaced `None` and destroyed nothing, and `Drop` had
        // nothing to release. It is stored below.
        let initial_rgba = crate::ui::appicon::render_rgba(theme, icon_px);
        let initial_icon = create_hicon(&initial_rgba, icon_px);
        let added = add_notify_icon(hwnd, initial_icon, &[]);
        if !added {
            // Everything this function made and nothing else took over.
            // `WM_SETICON` does not transfer ownership — the window is
            // handed a handle to draw with and the caller keeps it — so
            // the title-bar pair is still ours here. It was left behind on
            // this path, which is the one path where `Drop` never runs
            // because no `Self` is returned. The whole reason
            // `appicon::WindowIcon` exists is that ownership should travel
            // with the handle; these two were the pair that went round it.
            for icon in window_icons.into_iter().flatten() {
                // SAFETY: an icon this value created and has not handed on.
                let _ = unsafe { DestroyIcon(icon) };
            }
            if let Some(icon) = initial_icon {
                // SAFETY: as above — the second of the pair.
                let _ = unsafe { DestroyIcon(icon) };
            }
            // SAFETY: this value's own window, destroyed once, on drop.
            let _ = unsafe { DestroyWindow(hwnd) };
            return None;
        }

        // HID interface arrival/removal, so hot-plug skips the backoff.
        let notify = register_hid_notifications(hwnd);

        TRAY_HWND.store(hwnd.0, Ordering::Release);
        Some(Self {
            hwnd,
            icon: initial_icon,
            tip: Vec::new(),
            notify,
            window_icons,
        })
    }

    /// Replaces the tray icon. Called directly: no channel, no cross-thread
    /// post, because the caller already owns this window.
    pub(crate) fn set_icon(&mut self, rgba: &[u8], size: u32) {
        // SAFETY: the icon is created here and either handed to the shell — which
        // copies it — or destroyed on the failure path below.
        unsafe {
            let Some(icon) = create_hicon(rgba, size) else {
                return;
            };
            let mut data = base_data(self.hwnd);
            data.uFlags = NIF_ICON;
            data.hIcon = icon;
            // Failure is not an error worth acting on: the shell simply keeps
            // the previous icon, and monitoring continues either way.
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
            // Replace only after the shell has taken the new one, or the icon
            // flickers to blank in between.
            if let Some(old) = self.icon.replace(icon) {
                let _ = DestroyIcon(old);
            }
        }
    }

    /// Hands the shell a new tooltip, truncating it to what the field holds.
    ///
    /// The truncation lives here, at the boundary that owns the limit, so the
    /// caller composing the text does not have to carry the field's capacity
    /// around with it.
    pub(crate) fn set_tooltip(&mut self, text: &str) {
        // SAFETY: `data` is a live local with `cbSize` set by `base_data`, and the
        // tip is copied into its inline buffer, not referenced.
        unsafe {
            self.tip = crate::wide::truncated(text, TIP_MAX_UTF16);
            let mut data = base_data(self.hwnd);
            data.uFlags = NIF_TIP;
            copy_into(&mut data.szTip, &self.tip);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    /// Registers the icon again after Explorer restarted.
    ///
    /// A taskbar carries the notification icons registered with it and nothing
    /// else: when Explorer dies its icons die with it, and the shell announces
    /// the replacement by broadcasting the registered message `TaskbarCreated`
    /// to every top-level window. Applications that ignore it keep running with
    /// no way to be reached — the process is in the task list, the tray is
    /// empty, and for a utility that lives only in the tray that is
    /// indistinguishable from a hang.
    ///
    /// This is `NIM_ADD` rather than `NIM_MODIFY`, because there is nothing
    /// left to modify; the previous registration did not fail, it ceased to
    /// exist. The state replayed is this struct's own — the current icon and
    /// the current tooltip — so the entry comes back complete rather than
    /// blank until something upstream happens to change.
    ///
    /// Ignoring the broadcast is not the only way to get this wrong: a
    /// message-only window (`HWND_MESSAGE`) does not receive broadcasts at
    /// all, so the fix depends on this window being an ordinary top-level one.
    /// It is — see `hidden_window_styles`, which keeps it top-level and merely
    /// invisible for a different reason.
    pub(crate) fn readd(&self) {
        if !add_notify_icon(self.hwnd, self.icon, &self.tip) {
            crate::evlog::event(
                crate::evlog::Cat::Error,
                "tray icon could not be re-registered after Explorer restarted",
            );
        }
    }

    pub(crate) fn balloon(&self, title: &str, body: &str, severity: Severity) {
        // SAFETY: `data` is a live local with `cbSize` set by `base_data`, and
        // every string in it is an inline array filled by `copy_into` within
        // its own bounds — nothing is referenced past the call.
        unsafe {
            let mut data = base_data(self.hwnd);
            data.uFlags = NIF_INFO;
            copy_into(&mut data.szInfoTitle, &crate::wide::truncated(title, 63));
            copy_into(&mut data.szInfo, &crate::wide::truncated(body, 255));
            data.dwInfoFlags = severity.flag();
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    pub(crate) fn set_menu_labels(&mut self, panel: &str, settings: &str, exit: &str) {
        // Stored on the window: the menu is built inside the window procedure
        // on right-click, and that is the one reader. A struct field here held
        // a second copy that nothing ever read back.
        with_tray_state(self.hwnd, |state| {
            state.menu_labels = [panel.to_owned(), settings.to_owned(), exit.to_owned()];
        });
    }

    /// True if a click or menu choice is waiting to be collected. Checked by
    /// the message loop, which cannot see these flags any other way: they are
    /// set inside the window procedure rather than delivered as a message.
    ///
    /// Asked by comparing against "nothing happened" rather than by testing
    /// each field. The list of fields was written out here as well, which made
    /// four places to edit when an event is added — and this is the one where
    /// missing it is silent: the flag would be raised, the loop would never
    /// wake to collect it, and the tray would simply stop responding. Derived
    /// equality cannot be out of date.
    pub(crate) fn has_pending(&self) -> bool {
        with_tray_state(self.hwnd, |state| state.events != TrayEvents::default()).unwrap_or(false)
    }

    /// Takes the events accumulated since the last call, clearing them.
    ///
    /// Nothing here can block, so it was not worth calling non-blocking: the
    /// window procedure has already run, on this same thread, and left its
    /// findings in the flags above. The swap is how each event is delivered
    /// exactly once.
    pub(crate) fn poll_events(&self) -> TrayEvents {
        with_tray_state(self.hwnd, |state| std::mem::take(&mut state.events)).unwrap_or_default()
    }
}

impl Drop for TrayWindow {
    fn drop(&mut self) {
        // SAFETY: the notification registration is this value's own, unregistered
        // once.
        unsafe {
            if let Some(n) = self.notify {
                let _ = UnregisterDeviceNotification(n);
            }
            let data = base_data(self.hwnd);
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            if let Some(icon) = self.icon.take() {
                let _ = DestroyIcon(icon);
            }
            let _ = DestroyWindow(self.hwnd);
            // After the window is gone, so nothing can still be drawing with
            // them. `WM_SETICON` stores the handle rather than copying it.
            for icon in self.window_icons.into_iter().flatten() {
                let _ = DestroyIcon(icon);
            }
        }
        TRAY_HWND.store(std::ptr::null_mut(), Ordering::Release);
    }
}

fn show_menu(hwnd: HWND, labels: &[String; 3]) {
    // SAFETY: takes no arguments; the menu that comes back is owned here and
    // destroyed below on every path.
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let panel = crate::wide::truncated(&labels[0], 128);
    let settings = crate::wide::truncated(&labels[1], 128);
    let exit = crate::wide::truncated(&labels[2], 128);

    // SAFETY: `menu` is live, and the label is a NUL-terminated local that
    // outlives the call — `AppendMenuW` copies the text.
    let _ = unsafe { AppendMenuW(menu, MF_STRING, CMD_PANEL as usize, PCWSTR(panel.as_ptr())) };
    // SAFETY: as above.
    let _ = unsafe {
        AppendMenuW(
            menu,
            MF_STRING,
            CMD_SETTINGS as usize,
            PCWSTR(settings.as_ptr()),
        )
    };
    // SAFETY: `menu` is live; a separator has no text.
    let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) };
    // SAFETY: as above, with a label that outlives the call.
    let _ = unsafe { AppendMenuW(menu, MF_STRING, CMD_EXIT as usize, PCWSTR(exit.as_ptr())) };

    let mut pt = POINT::default();
    // SAFETY: `pt` is a live local the call fills in.
    let _ = unsafe { GetCursorPos(&mut pt) };
    // Required, or the menu will not dismiss when focus moves elsewhere.
    // SAFETY: `hwnd` is this program's own tray window.
    let _ = unsafe { SetForegroundWindow(hwnd) };

    // SAFETY: `menu` and `hwnd` are both live. This call runs its own modal
    // message loop, which dispatches back into `wnd_proc` — the reason the tray
    // state is held in a `RefCell` rather than handed out as `&mut`.
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            // The reserved parameter, which must be zero. `Option` here is the
            // binding's way of spelling "may be omitted"; omitting it passes
            // the same zero.
            None,
            hwnd,
            None,
        )
    };
    // SAFETY: the menu built above, destroyed once, after the loop has returned.
    let _ = unsafe { DestroyMenu(menu) };

    let id = chosen.0 as u32;
    if matches!(id, CMD_PANEL | CMD_SETTINGS | CMD_EXIT) {
        // Recorded here, consumed by handle_tray on the UI thread. If the
        // log shows this note with no frame following, the UI thread is
        // not picking commands up.
        // Recorded without waking, deliberately, and this is the one
        // place that is right. `show_menu` is called from `handle_tray`,
        // which *is* the pass that collects the command, so a wake here
        // would post a message asking the loop to do what it is already
        // doing.
        with_tray_state(hwnd, |state| state.raise(TrayEvent::Command(id)));
    }
}

/// The hidden window's procedure, called by Windows and by nothing else.
///
/// # Safety
///
/// `unsafe` here is not a choice: a window procedure is called by the system
/// with arguments it constructs, so the obligation belongs to Windows and the
/// keyword only records that. The one thing this body assumes beyond that is
/// `lparam` for `WM_DEVICECHANGE`, which the system documents as a
/// `DEV_BROADCAST_HDR` and which is checked for null and for device type
/// before the wider struct is read through it.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_COMMAND: u32 = 0x0111;
    const WM_DEVICECHANGE: u32 = 0x0219;
    // Both carry a change to the small-icon metric. `WM_SETTINGCHANGE` is the
    // broadcast for a non-client metrics edit; `WM_DPICHANGED` reaches this
    // window when the monitor it sits on changes scale. Neither is examined
    // further — `IconCache::refresh` compares the metric it last rendered at
    // and does nothing when it has not moved, so the flag is a prompt to look,
    // not a claim that something changed.
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const WM_DPICHANGED: u32 = 0x02E0;
    const DBT_DEVICEARRIVAL: usize = 0x8000;
    const DBT_DEVICEREMOVECOMPLETE: usize = 0x8004;

    // Checked before the `match` because the id is not a constant: it is
    // whatever `RegisterWindowMessageW` handed out at startup, and a match arm
    // needs a pattern. The zero guard matters — `TASKBAR_CREATED_MSG` is zero
    // until registration succeeds, and zero is `WM_NULL`, which does arrive.
    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    if taskbar_created != 0 && msg == taskbar_created {
        raise(hwnd, TrayEvent::TaskbarRecreated);
        return LRESULT(0);
    }

    match msg {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => {
                    raise(hwnd, TrayEvent::LeftClick);
                    wake_ui();
                }
                WM_RBUTTONUP => {
                    // Cloned before the call: TrackPopupMenu runs a modal
                    // message loop that dispatches back into this window, and
                    // holding the borrow across it would risk a double borrow.
                    let labels = with_tray_state(hwnd, |state| state.menu_labels.clone())
                        .unwrap_or_default();
                    show_menu(hwnd, &labels);
                    // show_menu may have recorded a choice.
                    wake_ui();
                }
                _ => {}
            }
            LRESULT(0)
        }
        // Not reached with the current menu, which uses `TPM_RETURNCMD` and
        // therefore returns the choice directly to `show_menu` instead of
        // posting `WM_COMMAND`. Kept because it costs nothing and a menu shown
        // without that flag — or a future accelerator — would arrive here.
        //
        // It wakes the loop for the same reason every other flag write does:
        // A menu command is not a message, so without the nudge the command
        // would sit in the flag until something unrelated happened along. The
        // omission was harmless only because the path is dormant, which is
        // precisely the kind of latent bug that surfaces the day it stops
        // being dormant.
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            if matches!(id, CMD_PANEL | CMD_SETTINGS | CMD_EXIT) {
                raise(hwnd, TrayEvent::Command(id));
                wake_ui();
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            let hdr = lparam.0 as *const DEV_BROADCAST_HDR;
            // Arrival or removal of a device *interface* — not a volume, not a
            // port. `register_hid_notifications` already subscribes with the
            // HID class GUID, so the shell only delivers HID events here; the
            // GUID is re-checked below so the handler is correct on its own
            // terms rather than relying on the registration alone. Reading the
            // interface header requires the type to be DEVICEINTERFACE first,
            // or the `DEV_BROADCAST_DEVICEINTERFACE_W` cast reads past a
            // shorter struct.
            let relevant = matches!(wparam.0, DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE)
                && !hdr.is_null()
                // SAFETY: `hdr` is non-null, checked immediately above, and Windows
                // documents `lparam` for this message as a `DEV_BROADCAST_HDR`.
                && unsafe { (*hdr).dbch_devicetype } == DBT_DEVTYP_DEVICEINTERFACE
                && {
                    let iface = hdr.cast::<DEV_BROADCAST_DEVICEINTERFACE_W>();
                    // SAFETY: the header says this is a device-interface broadcast, so the
                    // wider struct is the one actually behind the pointer.
                    unsafe { (*iface).dbcc_classguid == HidD_GetHidGuid() }
                };
            if relevant {
                raise(hwnd, TrayEvent::DeviceChanged);
                wake_ui();
            }
            LRESULT(1)
        }
        WM_SETTINGCHANGE | WM_DPICHANGED => {
            raise(hwnd, TrayEvent::MetricsChanged);
            wake_ui();
            // Passed on: these are broadcasts the default procedure may still
            // want, and swallowing them is not this window's business.
            // SAFETY: forwards the arguments the system passed in, unchanged.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // The last message a window ever receives, which is where the state it
        // owns is given back. `WM_DESTROY` would be too early: the non-client
        // teardown that follows it still reaches this procedure.
        WM_NCDESTROY => {
            detach_state(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            // SAFETY: posts `WM_QUIT` to this thread's own queue; the code is by value.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: forwards the arguments the system passed in, unchanged.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Guarantees the message loop runs a pass to collect a flag just set.
///
/// Every signal that has to reach the message loop from outside its own body is
/// a flag, not a message: tray clicks, menu choices, device changes, panel
/// clicks, keystrokes and close requests. Most of them come from a window
/// procedure on this same thread; only the poll thread's repaint genuinely
/// crosses one. `GetMessage` only returns for a message
/// actually in the queue, so setting a flag and returning is not enough — the
/// loop stays blocked until something unrelated arrives.
///
/// The worst case is `TrackPopupMenu`, which runs its own modal loop and
/// drains the queue dry; by the time it returns and stores the command there
/// is nothing left to wake on. That was the "clicked, and it appears a second
/// later" delay.
///
/// Posting an explicit `WM_NULL` puts a real message in the queue, so the next
/// `GetMessage` returns at once and `tick()` collects the flag.
///
/// Public because the panel needs exactly this and nothing else. It had its
/// own copy under a different name (`wake_loop`), byte-for-byte the same call
/// with the same rationale in different words — two places to fix if the
/// mechanism ever changes, and no way to notice that only one of them was.
///
/// This posts `WM_UI_INPUT`, which by itself asks the loop for a UI-only
/// pass. The tray paths in this file also call it while setting a flag —
/// a click, a menu choice, a device-change notification — and those *do*
/// need the device half. They get it because the loop separately promotes
/// the pass whenever `has_pending()` reports a tray flag, so the wake and
/// the reason for it stay independent: the wake says "look", the flags say
/// "at what".
pub(crate) fn wake_ui() {
    // Guard `hwnd == 0` exactly as the poll-thread repaint closure does. Before
    // the hidden window is created `tray_hwnd()` is 0, and `PostMessageW` with a
    // null hwnd does not fail — it posts a *thread* message to the calling
    // thread's queue, where nothing reads it. Skipping is correct: anything that
    // needed the wake is collected by the explicit pump right after the window
    // comes up.
    let hwnd = tray_hwnd();
    if hwnd.0.is_null() {
        return;
    }
    // SAFETY: `hwnd` is this program's tray window; the message carries no
    // pointers.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_UI_INPUT, WPARAM(0), LPARAM(0));
    }
}

/// Asks the message loop for a pass that includes the device half.
///
/// The sibling of [`wake_ui`], and the difference between them is the whole
/// reason both exist: `classify` reads `WM_UI_INPUT` as input waiting on a
/// window and `WM_NULL` as the poll thread having something to report, and only
/// the second kind of pass drains the poll channel. A caller that has left
/// messages in that channel therefore cannot use `wake_ui` — it would get a
/// repaint and the backlog would stay where it is.
///
/// This is the call the poll thread's repaint closure makes on every message it
/// sends; it lives here rather than being written out at each site so the null
/// -handle guard and the choice of message are stated once. `App::pump` makes
/// the same call when its per-pass drain cap leaves work behind.
pub(crate) fn wake_device() {
    // Same guard, same reason as `wake_ui`: before the hidden window exists
    // `tray_hwnd()` is 0, and `PostMessageW` with a null handle silently posts
    // a thread message to the *caller's* queue, which nothing reads. Skipping
    // loses nothing, because `run` pumps the channel explicitly as soon as the
    // tray window comes up.
    let hwnd = tray_hwnd();
    if hwnd.0.is_null() {
        return;
    }
    // SAFETY: `hwnd` is this program's tray window, checked non-null above;
    // `WM_NULL` carries no payload, so there is nothing to outlive the call.
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    }
}

/// Subscribes the window to HID interface arrival and removal.
fn register_hid_notifications(hwnd: HWND) -> Option<HDEVNOTIFY> {
    let mut filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
        // Same GUID the HID layer enumerates with.
        // SAFETY: returns the HID class GUID by value.
        dbcc_classguid: unsafe { HidD_GetHidGuid() },
        ..Default::default()
    };
    // SAFETY: `filter` is a live local for the duration of the call — the
    // registration copies what it needs — and `hwnd` is this program's window.
    unsafe {
        RegisterDeviceNotificationW(
            hwnd.into(),
            std::ptr::from_mut(&mut filter).cast::<core::ffi::c_void>(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
        .ok()
    }
}

/// Registers the notification icon, for the first time or after Explorer
/// restarted.
///
/// One function for both, because the two must agree: an entry re-created with
/// fewer fields than the original is a different entry, and the difference —
/// a missing callback message, say — shows up as a tray icon that draws but
/// does not respond to clicks.
fn add_notify_icon(hwnd: HWND, icon: Option<HICON>, tip: &[u16]) -> bool {
    // SAFETY: `data` is a live local with `cbSize` set by `base_data`; every
    // string in it is an inline array.
    unsafe {
        let mut data = base_data(hwnd);
        data.uFlags = add_flags();
        data.uCallbackMessage = WM_TRAY;
        if let Some(icon) = icon {
            data.hIcon = icon;
        }
        copy_into(&mut data.szTip, tip);
        Shell_NotifyIconW(NIM_ADD, &data).as_bool()
    }
}

fn base_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// Builds the 1-bpp AND mask `CreateIconIndirect` expects.
///
/// Two things here were wrong and both were invisible at the sizes usually
/// looked at:
///
/// 1. **Row padding.** A 1-bpp DIB pads every *row* to a WORD boundary, so a
///    16 px icon needs 2 bytes per row and a 24 px icon needs 4 — not
///    `size * size / 8`. At 16, 32, 48 and 64 those two formulas agree, which
///    is why the bug survived; at 20, 24 and 40 — the sizes Windows requests
///    at 125% and 150% display scaling — the buffer was short and
///    `CreateBitmap` read past its end, taking whatever happened to follow as
///    the mask.
/// 2. **Contents.** The mask was zero-filled, which declares every pixel
///    opaque, so the transparent surround was drawn too. On a dark taskbar
///    that is a dark rectangle: present, but indistinguishable from nothing.
///
/// A set bit means transparent.
fn build_mask(rgba: &[u8], size: u32) -> Vec<u8> {
    let size = size as usize;
    let row = size.div_ceil(16) * 2;
    let mut bits = vec![0u8; row * size];
    // Both buffers are walked as rows rather than addressed by arithmetic. The
    // two strides differ — four bytes per pixel on one side, a padded byte per
    // eight pixels on the other — and computing an offset into each from the
    // same `(x, y)` is how the padding bug above got in: one formula was right
    // and the other agreed with it only at power-of-two sizes. `zip` makes the
    // pairing of a source row with its mask row the loop's own structure, so a
    // short buffer ends the walk instead of reading past the end.
    let pixels = rgba.chunks_exact(size * 4);
    for (source, mask) in pixels.zip(bits.chunks_exact_mut(row)) {
        for (x, pixel) in source.chunks_exact(4).enumerate() {
            // A pixel is four bytes by construction of the chunk; the alpha is
            // the last of them.
            let transparent = pixel.last().is_some_and(|&alpha| alpha < 128);
            if let (true, Some(byte)) = (transparent, mask.get_mut(x / 8)) {
                *byte |= 0x80 >> (x % 8);
            }
        }
    }
    bits
}

/// RGBA to BGRA, channels swapped and **alpha left alone**.
///
/// # Do not premultiply here
///
/// `AlphaBlend` requires premultiplied source, so premultiplying the buffer
/// handed to `CreateIconIndirect` looks like the obviously correct thing to
/// do. It is not: the icon is not the surface `AlphaBlend` receives. Windows
/// takes the bitmap given here, applies alpha itself, and an icon built from
/// premultiplied bits is therefore multiplied twice.
///
/// This was established by measurement rather than by argument, because the
/// argument pointed the wrong way. A screenshot of the tray at 100% scaling
/// was compared against the renderer's own output composited over the
/// taskbar's background. For the third-covered pixels down the right edge of
/// the mark, straight compositing predicts `(176, 220, 187)` and double
/// multiplication predicts `(155, 170, 159)`; the screen held `(157, 172,
/// 161)`. Every sampled edge pixel agreed with the second within two counts
/// per channel.
///
/// The visible symptom was a dark fringe all round the glyph — the edge
/// pixels rendered at `colour * a²` instead of `colour * a`, so each one sat
/// darker than both the mark and the background it was blending into. It read
/// as a drop shadow, which is what the report called it.
///
/// The same reasoning explains why the `.ico`/`.res` encoder also stores
/// straight alpha: both are inputs Windows converts, not surfaces it blends.
fn straight_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        // The chunk is four bytes by construction, and `try_into` is how that
        // is said to the compiler rather than to the reader: a fixed-size
        // array names the four channels once, and the swap below is then a
        // reordering of named values instead of four subscripts that have to
        // stay within a bound nobody restates.
        if let Ok([r, g, b, a]) = <[u8; 4]>::try_from(px) {
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    out
}

/// Builds an icon from a straight-alpha RGBA buffer.
///
/// The buffer is passed through with its channels swapped and nothing else
/// done to it; see `straight_bgra` for why premultiplying here is wrong.
///
/// # Why `CreateDIBSection` and not `CreateBitmap`
///
/// `CreateBitmap` makes a device-*dependent* bitmap and takes the bits as
/// being in the device's own format. For 32 bpp on a 32-bpp desktop that
/// happens to line up, but nothing in the call states the layout being
/// supplied, so the alpha channel travels on an assumption rather than on a
/// declaration. A DIB section names its format in a `BITMAPINFOHEADER`:
/// `biBitCount` 32, `BI_RGB`, and a negative `biHeight` for top-down rows, so
/// the buffer copied into it means what it says.
pub(crate) fn create_hicon(rgba: &[u8], size: u32) -> Option<HICON> {
    let color = Bitmap::dib_bgra(size, &straight_bgra(rgba))?;

    // The mask is still supplied even though a 32-bpp colour bitmap makes it
    // redundant on the alpha-blended draw path: the legacy path — the one Task
    // Manager's process list uses — reads it, and a wrong mask there is what
    // once drew the transparent surround as solid black.
    let mask = Bitmap::monochrome(size, &build_mask(rgba, size))?;

    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask.raw(),
        hbmColor: color.raw(),
    };

    // SAFETY: both bitmaps are live for the call — they are owned by this
    // scope — and `ICONINFO` is fully initialised above.
    //
    // `CreateIconIndirect` copies the bitmaps, so the two originals are
    // released as this returns. That used to be four `DeleteObject` calls
    // spread over three exit paths, one of which had to remember that the
    // second bitmap did not exist yet.
    unsafe { CreateIconIndirect(&info) }.ok()
}

fn copy_into(dst: &mut [u16], src: &[u16]) {
    // The last slot belongs to the terminator, and splitting it off is what
    // says so. The bound on the copy and the position of the zero then come
    // out of one decision instead of being two expressions over `dst.len()`
    // that have to agree — which is what they were, and what makes an
    // off-by-one here a buffer with no terminator in it. A zero-length buffer
    // has room for neither and is left untouched.
    let Some((terminator, room)) = dst.split_last_mut() else {
        return;
    };
    let mut slots = room.iter_mut();
    // `src` leads the pairing deliberately: `Zip` pulls from its first side
    // first, so a short `src` must be the one that stops the walk, or the slot
    // the terminator belongs in would be taken and thrown away.
    for (&unit, slot) in src.iter().zip(slots.by_ref()) {
        *slot = unit;
    }
    // Wherever the copy stopped is where the text ends: the next free slot if
    // it ran out of text, the reserved one if it ran out of room.
    *slots.next().unwrap_or(terminator) = 0;
}

/// Styles for the hidden message window.
///
/// Separated so the rule is testable. The window exists only to receive
/// `WM_TRAY` and device-change notifications; it is zero-sized and never
/// shown. It must therefore not look like an application window to the shell.
///
/// The probe on a real machine caught this: the window carried
/// `style=0x04c00000` (`WS_CAPTION | WS_BORDER | WS_DLGFRAME`, which is what
/// `WS_OVERLAPPED` expands to) and no `WS_EX_TOOLWINDOW`. To the shell that is
/// an ordinary hidden top-level window, so the process was given a
/// taskbar/Task Manager identity derived from it instead of from the
/// notification icon.
///
/// `WS_EX_TOOLWINDOW` is the documented fix — a tool window does not appear in
/// the taskbar or the Alt+Tab list — and `WS_POPUP` drops a caption and frame
/// that a never-shown window has no use for.
///
/// The parent comes from here too, and it has to be `None`. The obvious way to
/// hide a window that is never shown is `HWND_MESSAGE`, which makes it
/// message-only — and a message-only window is excluded from **broadcasts**.
/// Both messages this window exists to receive are broadcasts: `TaskbarCreated`
/// when Explorer restarts, and `WM_DEVICECHANGE` when the UPS is plugged in.
/// Returning the parent alongside the styles puts that decision where the
/// window's shape is decided, and lets it be checked as a value rather than by
/// searching `new` for the string `HWND_MESSAGE`.
fn hidden_window_styles() -> (WINDOW_STYLE, WINDOW_EX_STYLE, Option<HWND>) {
    (WS_POPUP, WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, None)
}

/// Flags for the initial `NIM_ADD`.
///
/// Split out so the rule is testable: the icon must be declared valid on the
/// *first* registration, not only on a later `NIM_MODIFY`. The diagnostic log
/// from a real machine showed `NIM_ADD ok=true hIcon=0x0` — the call
/// succeeded while carrying no icon, because `hIcon` is simply not read
/// unless `NIF_ICON` is among the flags. Until the first modify arrived the
/// shell held an iconless entry, and Task Manager reads exactly that entry
/// for a process with no visible window.
fn add_flags() -> NOTIFY_ICON_DATA_FLAGS {
    NIF_MESSAGE | NIF_TIP | NIF_ICON
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{WS_CAPTION, WS_EX_APPWINDOW};

    /// Every event lands in its own field, and is delivered exactly once.
    ///
    /// This replaces a structural test that searched the window procedure's
    /// source for `FLAG.store(` and looked for a `wake_ui()` within a few lines
    /// of each. It could not tell a wake that runs from one in an arm that is
    /// never reached, and it stopped matching anything the moment the flags
    /// stopped being statics.
    ///
    /// The property it stood for — a raised event always wakes the loop — is no
    /// longer a thing to test. There is one function that raises an event,
    /// `raise`, and it wakes; the only write that deliberately does not is in
    /// `show_menu`, which is called from the pass that collects it. What is
    /// worth pinning instead is the mapping, which the exhaustive `match` makes
    /// total but does not make correct: five events into five fields is five
    /// chances to write the wrong one.
    #[test]
    fn each_event_lands_in_its_own_field_and_is_taken_once() {
        let mut state = TrayState::default();
        state.raise(TrayEvent::LeftClick);
        state.raise(TrayEvent::DeviceChanged);
        state.raise(TrayEvent::TaskbarRecreated);
        state.raise(TrayEvent::MetricsChanged);
        state.raise(TrayEvent::Command(CMD_SETTINGS));

        assert!(state.events.left_click);
        assert!(state.events.device_changed);
        assert!(state.events.taskbar_recreated);
        assert!(state.events.metrics_changed);
        assert_eq!(state.events.command, Some(CMD_SETTINGS));
    }

    /// One event does not raise another.
    ///
    /// The failure this rules out is a `match` arm assigning the neighbouring
    /// field — a left click reported as a device change, which would make the
    /// panel reconnect on every click and never open.
    #[test]
    fn raising_one_event_leaves_the_others_alone() {
        for event in [
            TrayEvent::LeftClick,
            TrayEvent::DeviceChanged,
            TrayEvent::TaskbarRecreated,
            TrayEvent::MetricsChanged,
            TrayEvent::Command(CMD_EXIT),
        ] {
            let mut state = TrayState::default();
            state.raise(event);
            let raised = usize::from(state.events.left_click)
                + usize::from(state.events.device_changed)
                + usize::from(state.events.taskbar_recreated)
                + usize::from(state.events.metrics_changed)
                + usize::from(state.events.command.is_some());
            assert_eq!(raised, 1, "{event:?} raised {raised} events, not one");
        }
    }

    /// Channels are swapped and alpha is carried through untouched.
    ///
    /// The temptation is to premultiply, because `AlphaBlend` requires it. An
    /// icon is not an `AlphaBlend` surface: Windows applies alpha to these
    /// bits itself, so premultiplying them makes it happen twice and every
    /// antialiased pixel comes out at `colour * a²` — darker than the mark and
    /// darker than the background, which on screen is a drop shadow round the
    /// glyph. Measured against a real screenshot; the arithmetic is in
    /// `straight_bgra`.
    #[test]
    fn icon_pixels_keep_straight_alpha() {
        assert_eq!(straight_bgra(&[10, 20, 30, 255]), vec![30, 20, 10, 255]);
        // Half-transparent white stays white. Premultiplying would give 128.
        assert_eq!(
            straight_bgra(&[255, 255, 255, 128]),
            vec![255, 255, 255, 128],
            "colour must not be scaled by alpha; Windows does that itself"
        );
        assert_eq!(straight_bgra(&[255, 255, 255, 0]), vec![255, 255, 255, 0]);
    }

    /// On a real glyph the conversion changes nothing but channel order.
    ///
    /// A pixel-for-pixel identity, so any future scaling of the colour
    /// channels — premultiplication being the one that keeps suggesting
    /// itself — fails here rather than on someone's taskbar.
    #[test]
    fn the_conversion_is_a_channel_swap_and_nothing_else() {
        let theme = crate::ui::theme::Theme::default();
        for size in [16u32, 20, 24, 32] {
            let rgba = crate::ui::appicon::render_rgba(&theme, size);
            let bgra = straight_bgra(&rgba);
            assert_eq!(bgra.len(), rgba.len());
            for (src, dst) in rgba.chunks_exact(4).zip(bgra.chunks_exact(4)) {
                assert_eq!(
                    [dst[0], dst[1], dst[2], dst[3]],
                    [src[2], src[1], src[0], src[3]],
                    "{size}px: the conversion altered a pixel beyond swapping R and B"
                );
            }
        }
    }

    /// The initial add and the re-add must be the same call.
    ///
    /// After Explorer restarts there is no registration left to amend, so the
    /// icon is added again from scratch. Any field the replay leaves out that
    /// the original set produces a subtly different entry — most visibly a
    /// missing `uCallbackMessage`, which yields an icon that draws but ignores
    /// clicks, and there is no error to notice. Routing both through
    /// `add_notify_icon` is what makes them identical; this pins that they
    /// still do.
    ///
    /// **Structural, and temporarily so.** It reads source text rather than
    /// running the code, because registering a notification icon needs a shell
    /// that is running and a window that is real. A text search cannot tell a
    /// correct call from one in an arm that is never reached, so this is weaker
    /// than the property it stands for. The remedy is the one applied to
    /// `app::plan_emission`: lift the decision into a function that takes what
    /// it needs and returns what it decided, then assert on the value. Until
    /// that is done here, this is the only check there is.
    #[test]
    fn the_first_registration_and_the_replay_are_one_call() {
        let source = include_str!("tray_window.rs");
        for owner in ["pub(crate) fn new(", "pub(crate) fn readd("] {
            let body = crate::testsupport::fn_body(source, owner);
            assert!(
                body.contains("add_notify_icon("),
                "{owner} registers the tray icon by hand instead of through \
                 the shared call; the two registrations can then differ"
            );
        }
        let body = crate::testsupport::fn_body(source, "fn add_notify_icon(");
        assert!(
            body.contains("uCallbackMessage"),
            "without a callback message the shell has nowhere to send clicks"
        );
        assert!(
            body.contains("NIM_ADD"),
            "a taskbar that has just been created holds no entry to modify"
        );
    }

    /// The hidden window must not present itself as an application window, and
    /// must not be message-only.
    #[test]
    fn hidden_window_is_a_tool_window() {
        let (style, ex_style, parent) = hidden_window_styles();

        assert!(
            parent.is_none(),
            "a message-only parent (HWND_MESSAGE) excludes the window from \
             broadcasts, and both TaskbarCreated and WM_DEVICECHANGE arrive \
             as broadcasts"
        );

        assert!(
            ex_style & WS_EX_TOOLWINDOW == WS_EX_TOOLWINDOW,
            "without WS_EX_TOOLWINDOW the shell treats this hidden, zero-sized \
             window as an application window and builds the process's taskbar \
             identity from it instead of from the notification icon"
        );
        assert!(
            style & WS_CAPTION != WS_CAPTION,
            "WS_CAPTION (part of WS_OVERLAPPED) marks this as a normal \
             top-level window; it is never shown and has no title bar"
        );
        assert!(
            ex_style & WS_EX_APPWINDOW != WS_EX_APPWINDOW,
            "WS_EX_APPWINDOW would force it onto the taskbar, the opposite of \
             what is wanted"
        );
    }

    /// The initial add must declare the icon field valid.
    #[test]
    fn initial_add_declares_the_icon() {
        let flags = add_flags();
        assert!(
            flags & NIF_ICON == NIF_ICON,
            "NIM_ADD without NIF_ICON registers an iconless tray entry; \
             Shell_NotifyIcon still returns success, so this fails silently"
        );
        // The other two are what make the icon interactive and labelled.
        assert!(flags & NIF_MESSAGE == NIF_MESSAGE);
        assert!(flags & NIF_TIP == NIF_TIP);
    }

    /// The mask buffer must be big enough for WORD-padded rows at every size
    /// Windows may request, not only at the powers of two.
    ///
    /// `size * size / 8` agrees with the real requirement at 16, 32, 48 and
    /// 64 and is short at 20, 24 and 40. Those are precisely the sizes the
    /// shell asks for at 125% and 150% scaling, so the icon was built from a
    /// short buffer on the majority of laptops while looking fine on a 100%
    /// display.
    #[test]
    fn mask_buffer_is_word_aligned_per_row() {
        for size in [16u32, 20, 24, 32, 40, 48, 64] {
            let rgba = vec![0u8; (size * size * 4) as usize];
            let mask = build_mask(&rgba, size);
            let expected_row = (size as usize).div_ceil(16) * 2;
            assert_eq!(
                mask.len(),
                expected_row * size as usize,
                "{size}px mask must be {expected_row} bytes per row"
            );
            assert!(
                mask.len() >= ((size * size) / 8) as usize,
                "{size}px mask is smaller than the naive size*size/8"
            );
        }
    }

    /// A set bit means transparent, and it must follow the alpha channel.
    #[test]
    fn mask_marks_transparent_pixels() {
        let size = 24u32;
        let theme = crate::ui::theme::Theme::default();
        let rgba = crate::ui::appicon::render_rgba(&theme, size);
        let mask = build_mask(&rgba, size);
        let row = (size as usize).div_ceil(16) * 2;

        let mut transparent = 0usize;
        for y in 0..size as usize {
            for x in 0..size as usize {
                let alpha = rgba[(y * size as usize + x) * 4 + 3];
                let bit = mask[y * row + x / 8] & (0x80 >> (x % 8)) != 0;
                assert_eq!(bit, alpha < 128, "mask disagrees with alpha at ({x},{y})");
                if alpha < 128 {
                    transparent += 1;
                }
            }
        }
        assert!(
            transparent > 0,
            "a zero mask paints the surround and hides the icon on dark backgrounds"
        );
    }
}
