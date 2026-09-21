//! Raw HID access. All `unsafe` in the crate is confined to this module and
//! `descriptor.rs`; everything above sees a safe API.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem::{offset_of, size_of};

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetIndexedString, HidD_GetPreparsedData, HidD_SetFeature, HIDD_ATTRIBUTES,
    PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::error::{Error, Result};
/// Owned HID handle plus its preparsed descriptor data.
pub(crate) struct RawDevice {
    handle: HANDLE,
    preparsed: PHIDP_PREPARSED_DATA,
    /// The device interface path this handle was opened from.
    ///
    /// Used when several interfaces match the configured VID/PID: `connect`
    /// takes the first and logs which one, along with the paths it passed
    /// over. That is the only place the value is decision-relevant, and it is
    /// the reason the string is retained rather than dropped after `CreateFileW`.
    pub path: String,
}

impl RawDevice {
    /// Enumerate HID interfaces and open the first one matching `vid`.
    /// `pid == 0` matches any product id under that vendor.
    ///
    /// Returns the interface that was opened and the ones that also matched,
    /// in that order — which is exactly what the caller does with them: it
    /// talks to the first and names the rest in the log when there is more
    /// than one.
    ///
    /// A pair rather than a `Vec`, because "not empty" is the whole content of
    /// the `Err` branch below and a `Vec` cannot say it. Returned as a list,
    /// the caller had to take element zero, and the only thing standing
    /// between that index and an `abort` in the poll thread — the release
    /// profile is `panic = "abort"` — was an agreement between two functions
    /// that the compiler cannot read. Here the emptiness is spent at the point
    /// it is decided.
    ///
    /// # Errors
    ///
    /// [`Error::DeviceNotFound`] when the enumeration succeeded and nothing in
    /// it matched — the ordinary case of a UPS that is simply not plugged in,
    /// which the reconnect loop retries rather than reports. Propagates
    /// [`Error::Enumeration`] from [`enumerate_hid_paths`] when the enumeration
    /// itself failed, which is a different situation: the question was never
    /// asked, so "not found" would be a claim this call is not entitled to
    /// make. A path that matches by name but refuses to open is skipped, not
    /// raised — a keyboard held by another process must not fail the search
    /// for a UPS.
    pub(crate) fn open_matching(vid: u16, pid: u16) -> Result<(Self, Vec<Self>)> {
        let wanted = path_fragment(vid, pid);
        let mut found = Vec::new();
        for path in enumerate_hid_paths()? {
            // The identity is in the path, so a path that cannot belong to this
            // device is dropped before it is opened. Opening was the only test
            // before, which meant a `CreateFileW`/`HidD_GetAttributes`/
            // `CloseHandle` triple against every keyboard, mouse, touchpad and
            // dongle in the system — ten to thirty of them — repeated every ten
            // seconds, forever, by the reconnect loop of a utility whose UPS is
            // simply not plugged in. The usual case is now one open.
            if !path_carries(&path, &wanted) {
                continue;
            }
            if let Some(dev) = Self::open_if_matching(&path, vid, pid) {
                found.push(dev);
            }
        }
        let mut found = found.into_iter();
        let Some(opened) = found.next() else {
            return Err(Error::DeviceNotFound { vid, pid });
        };
        Ok((opened, found.collect()))
    }

    /// Opens one interface path and keeps it only when the VID/PID matches.
    ///
    /// `dwDesiredAccess = 0` is the critical trick: requesting `GENERIC_READ`
    /// returns `ERROR_ACCESS_DENIED` because the system HID battery stack
    /// already holds the device. With a zero access mask the open succeeds
    /// and `HidD_GetFeature` / `HidD_SetFeature` still work.
    ///
    /// Attributes are still checked, and checked *before* the preparsed data is
    /// fetched. The path filter in `open_matching` is a filter, not a proof:
    /// the identity that counts is the one the device reports over the wire,
    /// and parsing the descriptor of a device that is about to be closed again
    /// is work done for nothing.
    ///
    /// `Option`, not `Result`: an interface that cannot be opened or that
    /// belongs to another device is simply not a candidate, and the previous
    /// per-path error was constructed only to be discarded at the call site.
    /// The one failure the caller can act on — no candidate at all — is
    /// reported by `open_matching`.
    fn open_if_matching(path: &str, vid: u16, pid: u16) -> Option<Self> {
        let wide = crate::wide::nul_terminated(path);
        // SAFETY: `wide` is a NUL-terminated copy of the interface path and
        // outlives the call. Access is zero — neither read nor write — which is
        // what lets a device already opened for I/O by another process still be
        // queried for its attributes.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
                None,
            )
        }
        .ok()?;

        // SAFETY: closes a handle this function opened and is about to
        // abandon. Every path that calls it returns immediately after, so the
        // handle cannot be closed twice.
        let close = |h| unsafe {
            let _ = CloseHandle(h);
        };

        let mut attrs = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: `handle` is the live handle just opened, and `attrs` is a
        // live local the call fills in.
        if !unsafe { HidD_GetAttributes(handle, &mut attrs) } {
            close(handle);
            return None;
        }
        if !(attrs.VendorID == vid && (pid == 0 || attrs.ProductID == pid)) {
            close(handle);
            return None;
        }

        let mut preparsed = PHIDP_PREPARSED_DATA::default();
        // SAFETY: as above. On success the pointer written into `preparsed`
        // becomes this value's to free, which `Drop` does exactly once.
        if !unsafe { HidD_GetPreparsedData(handle, &mut preparsed) } {
            close(handle);
            return None;
        }

        Some(Self {
            handle,
            preparsed,
            path: path.to_owned(),
        })
    }

    pub(super) fn preparsed(&self) -> Preparsed<'_> {
        Preparsed {
            raw: self.preparsed,
            _owner: PhantomData,
        }
    }

    /// Read a feature report. `buf[0]` must already hold the report id, and
    /// `buf` must be sized to the descriptor's declared feature report length.
    ///
    /// # Errors
    ///
    /// [`Error::FeatureRead`], carrying the report id together with the Win32
    /// code and message. The code is the load-bearing part and is captured
    /// rather than discarded. It used to return a bare `FeatureRead { rid }`,
    /// which named the report and nothing else — and the distinction it threw
    /// away is the one that decides what to do next. `ERROR_GEN_FAILURE` is
    /// the device having stopped answering, the state these units enter under
    /// aggressive polling and leave only on a physical reconnect.
    /// `ERROR_INVALID_PARAMETER` is this code sending a buffer the wrong size
    /// for the report, which is a bug here and no fault of the device at all.
    /// `ERROR_DEVICE_NOT_CONNECTED` is a cable. All three rendered as the
    /// same sentence.
    ///
    /// The elapsed time is measured on the same call, because a transfer that
    /// succeeds slowly is the symptom that precedes one that fails: a device
    /// being polled harder than it can answer shows up as latency climbing
    /// long before the first outright failure.
    pub(crate) fn get_feature(&self, buf: &mut [u8]) -> Result<()> {
        let rid = buf.first().copied().unwrap_or(0);
        // The clock is read only when the level that prints it is on. The
        // timing is evidence for the Debug line and nothing else consumes it,
        // and "Debug costs nothing while it is off" is a rule this was quietly
        // breaking: two `QueryPerformanceCounter` calls per transfer, ten
        // transfers a poll, on every machine whose owner never opened Settings.
        let started = crate::evlog::debug_enabled().then(std::time::Instant::now);
        // SAFETY: the handle is live for as long as this value is, and `buf` is
        // a live slice the call writes into for the length passed alongside it —
        // its own, not a remembered one.
        let ok = unsafe {
            HidD_GetFeature(
                self.handle,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u32,
            )
        };
        // `None` says the level was off when the transfer began, so there is no
        // line to write and no timing to write in it. That is the same
        // condition `debug_transfer` checks for itself, half a millisecond
        // later; carrying it in the value keeps a line from ever claiming a
        // duration nobody measured.
        let elapsed = started.map(|t| t.elapsed());

        if ok {
            crate::evlog::debug_transfer(rid, buf.len(), elapsed, Ok(buf));
            Ok(())
        } else {
            // Read immediately: any intervening call can overwrite the
            // thread's last-error slot, and the value is the whole point.
            let cause = windows::core::Error::from_thread();
            let code = cause.code().0 as u32;
            let message = cause.message();
            crate::evlog::debug_transfer(rid, buf.len(), elapsed, Err((code, message.clone())));
            Err(Error::FeatureRead {
                rid,
                code,
                cause: message,
            })
        }
    }

    /// Write a feature report. Used only for the beeper control.
    ///
    /// # Errors
    ///
    /// [`Error::FeatureWrite`], carrying the report id and the Win32 code and
    /// message. Its own variant rather than [`Error::FeatureRead`] because the
    /// log verdicts are precise on purpose: "read failed" on a line about a
    /// refused write sends whoever is diagnosing it to the wrong operation.
    pub(crate) fn set_feature(&self, buf: &[u8]) -> Result<()> {
        let rid = buf.first().copied().unwrap_or(0);
        // SAFETY: as `get_feature`, except the buffer is read rather than
        // written — which the `*const` in the signature now says on its own.
        let ok = unsafe {
            HidD_SetFeature(self.handle, buf.as_ptr().cast::<c_void>(), buf.len() as u32)
        };
        if ok {
            crate::evlog::debug_write(rid, buf, Ok(()));
            Ok(())
        } else {
            // Read immediately: any intervening call can overwrite the
            // thread's last-error slot, and the value is the whole point.
            let cause = windows::core::Error::from_thread();
            let code = cause.code().0 as u32;
            let message = cause.message();
            crate::evlog::debug_write(rid, buf, Err((code, message.clone())));
            // Its own variant: the log verdicts are precise on purpose, and
            // "read failed" on a refused *write* sends the reader to the
            // wrong operation.
            Err(Error::FeatureWrite {
                rid,
                code,
                cause: message,
            })
        }
    }

    /// Read an indexed string (model, serial, manufacturer, chemistry, firmware).
    pub(crate) fn indexed_string(&self, index: u32) -> Option<String> {
        let mut buf = [0u16; 128];
        // SAFETY: the handle is live, and the destination is a live local
        // buffer whose byte length is passed with it.
        let ok = unsafe {
            HidD_GetIndexedString(
                self.handle,
                index,
                buf.as_mut_ptr().cast::<c_void>(),
                (buf.len() * 2) as u32,
            )
        };
        if !ok {
            return None;
        }
        // Up to the NUL, or the whole buffer when the device sent none. Taken
        // as an iterator so the terminator's position is found and used in one
        // expression rather than found, stored, and then applied as a range.
        let units: Vec<u16> = buf.iter().copied().take_while(|&c| c != 0).collect();
        let s = String::from_utf16_lossy(&units).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
}

/// The device's parsed report descriptor, borrowed from the device that owns
/// it.
///
/// Every `HidP_*` call takes this pointer, and every one of their `SAFETY`
/// notes used to say the same thing: that it is live, and that it belongs to
/// the device. Both were true and neither was checked — `preparsed()` handed
/// out a bare `PHIDP_PREPARSED_DATA`, which is a raw pointer with no lifetime
/// on it, and `first_report_id` took one as a parameter, so the pointer
/// already travelled across a signature untethered from the value that frees
/// it. Nothing but the order of the lines stopped a caller from keeping one
/// past the `RawDevice` it came from, and `Drop` calls `HidD_FreePreparsedData`.
///
/// This is [`Canvas`]'s arrangement applied to the other raw handle that
/// crosses a boundary in this program: the wrapper cannot be built from a raw
/// pointer, it borrows the device for `'a`, and [`Preparsed::raw`] is called
/// inline at the call itself. The condition stops being something the `SAFETY`
/// note asserts and becomes something the borrow checker refuses to break.
///
/// [`Canvas`]: crate::ui::gdi::Canvas
#[derive(Clone, Copy)]
pub(super) struct Preparsed<'a> {
    raw: PHIDP_PREPARSED_DATA,
    _owner: PhantomData<&'a RawDevice>,
}

impl Preparsed<'_> {
    /// The pointer, for the `HidP_*` call that is about to be made with it.
    pub(super) fn raw(self) -> PHIDP_PREPARSED_DATA {
        self.raw
    }
}

impl Drop for RawDevice {
    fn drop(&mut self) {
        // SAFETY: both are owned by this value — the preparsed data from
        // `HidD_GetPreparsedData` and the handle from `CreateFileW` — and `Drop`
        // runs once, so neither is released twice.
        unsafe {
            let _ = HidD_FreePreparsedData(self.preparsed);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Owns an `HDEVINFO` and destroys it on drop.
///
/// The list must be freed on every exit from `enumerate_hid_paths`. Today the
/// function has no early return inside the loop, so a single free at the end
/// would suffice — but the loop is exactly where a future `?` or `return` would
/// be added, and a leak there would be silent. Tying the free to a guard makes
/// the release unconditional by construction, the same pattern the self-test
/// session and the event handles use.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        // SAFETY: an owned `HDEVINFO` from `SetupDiGetClassDevsW`, destroyed
        // exactly once because only this value holds it.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// The substring a HID interface path carries for `vid`/`pid`.
///
/// The HID class driver builds the interface path from the hardware id, so a
/// USB device's path always contains `vid_xxxx&pid_xxxx` in lower-case hex —
/// `\\?\hid#vid_0764&pid_0601&mi_00#7&...`. That makes the identity readable
/// without opening anything, which is the only reason the reconnect loop does
/// not have to open everything.
///
/// `pid == 0` is the wildcard `open_matching` documents, so it narrows to the
/// vendor and leaves the product to `HidD_GetAttributes`.
fn path_fragment(vid: u16, pid: u16) -> String {
    if pid == 0 {
        format!("vid_{vid:04x}")
    } else {
        format!("vid_{vid:04x}&pid_{pid:04x}")
    }
}

/// Whether an interface path carries the fragment [`path_fragment`] built.
///
/// Case-folded, because the case of a path is the driver's business: Windows
/// writes these lower-case in practice, and nothing documents that it must, so
/// a device would not be dropped over an upper-case `VID_`.
fn path_carries(path: &str, fragment: &str) -> bool {
    path.to_ascii_lowercase().contains(fragment)
}

/// Every HID interface path the system currently exposes.
///
/// Walks `GUID_DEVINTERFACE_HID` and collects one path per interface.
///
/// # Errors
///
/// [`Error::Enumeration`], wrapping the Win32 error, when the device
/// information set cannot be opened. The per-interface calls below it are
/// allowed to fail quietly: an interface that will not describe itself is one
/// this utility cannot use, and dropping it leaves the rest of the enumeration
/// intact, where failing the whole call would hide every other device behind
/// one uncooperative one.
fn enumerate_hid_paths() -> Result<Vec<String>> {
    // SAFETY: takes no arguments and returns the HID class GUID by value.
    let guid = unsafe { HidD_GetHidGuid() };

    let devinfo = DevInfo(
        // SAFETY: `guid` is a live local, and the two string parameters are
        // explicitly null. The set that comes back is owned by `DevInfo`, which
        // destroys it on drop.
        unsafe {
            SetupDiGetClassDevsW(
                Some(&guid),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(Error::Enumeration)?,
    );
    let handle = devinfo.0;

    let mut paths = Vec::new();
    let mut index = 0u32;

    loop {
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        let more =
            // SAFETY: `handle` is the live set owned by `devinfo`; `guid` and
            // `iface` are live locals, the latter written by the call.
            unsafe { SetupDiEnumDeviceInterfaces(handle, None, &guid, index, &mut iface) }
                .is_ok();
        if !more {
            break;
        }
        index += 1;

        // First call sizes the buffer; it is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER.
        let mut required: u32 = 0;
        // SAFETY: the sizing call — a null detail pointer with a zero length is
        // how this function is asked how much room it needs, and `required` is a
        // live local it writes that answer into.
        let sized = unsafe {
            SetupDiGetDeviceInterfaceDetailW(handle, &iface, None, 0, Some(&mut required), None)
        };
        if sized.is_ok() || required == 0 {
            continue;
        }
        if windows::core::Error::from_thread().code() != ERROR_INSUFFICIENT_BUFFER.to_hresult() {
            continue;
        }

        // Backed by `u64`s, not `u8`s: the pointer is about to be treated as
        // `SP_DEVICE_INTERFACE_DETAIL_DATA_W`, and writing through a pointer
        // that does not meet the struct's alignment is undefined behaviour.
        // `Vec<u8>` only guarantees byte alignment — the allocator happening
        // to hand back aligned memory is not a contract — while `u64` meets
        // or exceeds any alignment the struct can require.
        let words = (required as usize).div_ceil(size_of::<u64>());
        let mut buffer = vec![0u64; words];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `detail` points at the start of a buffer allocated for
        // `required` bytes and aligned for this struct, so writing its header
        // field is in bounds.
        unsafe {
            // Not the size of the buffer — the size of the *fixed part* of the
            // struct, which is how `SetupDiGetDeviceInterfaceDetailW`
            // identifies the layout it is being handed. `size_of` gives 8 here
            // and that is correct for x86_64: a `u32` followed by a `u16` array
            // aligned to 4. On 32-bit x86 the same struct packs to 6, and the
            // call fails with `ERROR_INVALID_USER_BUFFER` if 8 is passed —
            // which is why this is written down rather than left to be
            // rediscovered by whoever first builds for i686. This utility
            // targets x86_64 only, so `size_of` is right as it stands.
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }

        // SAFETY: the same call with room this time — `detail` has `required`
        // bytes behind it, which is the length passed alongside it and the
        // length the sizing call above asked for.
        let got = unsafe {
            SetupDiGetDeviceInterfaceDetailW(handle, &iface, Some(detail), required, None, None)
        };
        if got.is_err() {
            continue;
        }

        // The path is a NUL-terminated `WCHAR` run that starts inside the
        // struct and continues past its declared end, so its length has to be
        // bounded by what actually fits in the buffer the call was given:
        // everything after the field's own offset. These bytes come from a
        // third-party driver; a missing terminator must not become a read past
        // the allocation.
        let offset = offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        // SAFETY: `detail` was filled by the call above. The scan is bounded by
        // `max`, which is what remains of the buffer after the field's own
        // offset, so an unterminated path from a third-party driver stops at the
        // end of the allocation rather than past it.
        let path = unsafe {
            let p = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
            let max = (required as usize).saturating_sub(offset) / size_of::<u16>();
            let mut len = 0usize;
            while len < max && *p.add(len) != 0 {
                len += 1;
            }
            if len == max {
                // No terminator within the buffer: the path cannot be read, so
                // this interface is not a candidate. Same policy as
                // `open_if_matching` applies to an interface it cannot use.
                continue;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        };
        paths.push(path);
    }

    // `devinfo` frees the list here on drop, on every path out of the function.
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An interface path in the shape Windows writes them, for the given
    /// vendor and product.
    ///
    /// Written once so the three tests below differ only in the part that is
    /// under test. The tail is a real device instance id and interface class
    /// GUID: the filter must find the identity inside a path of this shape, not
    /// inside a fragment trimmed to make the test pass.
    fn interface_path(vid_pid: &str) -> String {
        const INSTANCE: &str = r"&mi_00#7&1e2c5f3d&0&0000";
        const CLASS_GUID: &str = "#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        format!(r"\\?\hid#{vid_pid}{INSTANCE}{CLASS_GUID}")
    }

    /// The path filter must pass this device and drop everything else.
    ///
    /// It is the only test an interface gets before it is opened, so a filter
    /// that is too narrow is a UPS that is never found — a silent failure with
    /// no log line, because nothing failed.
    #[test]
    fn the_path_filter_admits_this_device_and_no_other() {
        let wanted = path_fragment(0x0764, 0x0601);
        assert!(
            path_carries(&interface_path("vid_0764&pid_0601"), &wanted),
            "the device's own path must pass"
        );
        assert!(
            !path_carries(&interface_path("vid_046d&pid_c31c"), &wanted),
            "another vendor's interface must not be opened"
        );
        // A device of the right vendor but the wrong product is dropped too:
        // the fragment names both halves.
        assert!(
            !path_carries(&interface_path("vid_0764&pid_0501"), &wanted),
            "a different product under the same vendor must not pass"
        );
    }

    /// `pid == 0` is the wildcard `open_matching` documents, and the filter
    /// must widen with it rather than quietly cancel it: narrowing on a product
    /// id of zero would make the wildcard match nothing at all.
    #[test]
    fn a_wildcard_product_id_narrows_only_to_the_vendor() {
        let wanted = path_fragment(0x0764, 0);
        for pid in ["0601", "0501"] {
            let path = interface_path(&format!("vid_0764&pid_{pid}"));
            assert!(
                path_carries(&path, &wanted),
                "the wildcard must admit every product of the vendor"
            );
        }
    }

    /// The case of a path is the driver's business, not this utility's.
    #[test]
    fn the_path_filter_ignores_case() {
        let shouted = interface_path("vid_0764&pid_0601").to_uppercase();
        assert!(
            path_carries(&shouted, &path_fragment(0x0764, 0x0601)),
            "an upper-case path names the same device"
        );
    }
}
