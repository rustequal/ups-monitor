//! Descriptor parsing. Offsets are never hardcoded: the parameter map is built
//! from `HidP_GetValueCaps` / `HidP_GetButtonCaps` at runtime.

use windows::Win32::Devices::HumanInterfaceDevice::{
    HidP_Feature, HidP_GetButtonCaps, HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages,
    HidP_GetValueCaps, HidP_Input, HidP_MaxUsageListLength, HidP_Output, HidP_SetUsageValue,
    HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_REPORT_TYPE, HIDP_STATUS_BUTTON_NOT_PRESSED, HIDP_VALUE_CAPS,
};
use windows::Win32::Foundation::NTSTATUS;

use super::device::{Preparsed, RawDevice};
use crate::error::{Error, Result};
pub(crate) const PAGE_POWER: u16 = 0x84;
pub(crate) const PAGE_BATTERY: u16 = 0x85;
/// CyberPower's vendor-defined usage page. The self-test channel gate and the
/// ASCII command pipe both live here; nothing on it is documented.
pub(crate) const PAGE_VENDOR: u16 = 0xff01;

// Collection usages used to disambiguate Voltage.
const COLL_INPUT: u16 = 0x1A;
const COLL_OUTPUT: u16 = 0x1C;
const COLL_POWER_SUMMARY: u16 = 0x24;

// Length-counter usages of the two vendor ASCII channel reports. The report id
// that carries the OUT counter is the command channel; the one carrying the IN
// counter is the reply channel. Used to resolve those ids by usage.
const VENDOR_OUT_COUNTER: u16 = 0x15;
const VENDOR_IN_COUNTER: u16 = 0x16;

/// Upper bound on how many usages one range-form button cap may expand to.
/// The descriptor is data from the device, so a corrupt or hostile one could
/// declare a 65 536-wide range; this caps the expansion and the truncation is
/// logged. No real HID Power Device cap comes near this.
const MAX_USAGE_RANGE: u16 = 256;

/// Which physical collection a duplicated usage belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Input,
    Output,
    PowerSummary,
    Unscoped,
}

impl Scope {
    /// Whether a field sitting in `field` satisfies a lookup for `self`.
    ///
    /// The single scope rule of the module, shared by `find_value` and
    /// `find_flag` so the two cannot drift: a named collection accepts only
    /// itself; `Unscoped` accepts every collection, including another
    /// `Unscoped`.
    ///
    /// Note the asymmetry — this is a lookup predicate, not equality.
    /// `Scope::Unscoped.accepts(Scope::Input)` is true (the caller did not
    /// care), while `Scope::Input.accepts(Scope::Unscoped)` is false (the
    /// caller asked for the Input collection and this field is not in it).
    fn accepts(self, field: Scope) -> bool {
        matches!(self, Scope::Unscoped) || self == field
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ValueField {
    pub page: u16,
    pub usage: u16,
    pub report_id: u8,
    /// Semantic collection the field sits under (Input / Output /
    /// `PowerSummary`), resolved from the link-collection tree. Used by
    /// `find_value` to pick the right duplicate of a usage that appears more
    /// than once.
    pub scope: Scope,
    /// The raw `HIDP_VALUE_CAPS.LinkCollection` node index. Passed to
    /// `HidP_GetUsageValue` so the read is scoped to *this* field's collection
    /// inside the report, not merely to the report id. Without it the API
    /// returns the first `page:usage` match in the report — correct only while
    /// each duplicate happens to live in its own report, which is a property of
    /// one firmware, not a guarantee.
    pub link_collection: u16,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlagField {
    pub page: u16,
    pub usage: u16,
    pub report_id: u8,
    /// Semantic collection, as for `ValueField`. `find_flag` uses it to
    /// disambiguate a usage present in several `PresentStatus` collections.
    pub scope: Scope,
    /// Raw link-collection node index, passed to `HidP_GetUsages` for the same
    /// reason `ValueField` carries one.
    pub link_collection: u16,
}

/// The parsed usage map for one device.
///
/// Feature reports only, in the map as well as in the readers. The caps for
/// input reports used to be parsed into the same lists with nothing marking
/// which report type an entry came from, while `read_value` and `read_flag`
/// are hardwired to `HidP_Feature` — so on a firmware where some usage exists
/// only as an input report, the lookup would find it and the read would then
/// run against a feature transfer of the same id, failing in a way the log
/// attributes to this utility. That is the same silent-wrong-path hazard the
/// removal of the `is_feature` parameter closed, reopened from the other
/// side. This device carries every field the utility needs in feature
/// reports; if input reports are ever needed, the caps come back from one
/// `HidP_GetValueCaps` call, and the entry must then carry its report type.
pub(crate) struct Descriptor {
    pub values: Vec<ValueField>,
    pub flags: Vec<FlagField>,
    pub feature_len: usize,
    /// Output report byte length, needed to size the `WriteFile` frame that
    /// carries a vendor ASCII command. Zero on a device with no output
    /// reports, in which case the self-test channel is unavailable.
    pub output_len: usize,
    /// Input report byte length, needed to size the `ReadFile` buffer that
    /// collects vendor replies.
    pub input_len: usize,
    /// Report id of the vendor OUT channel (host -> UPS), resolved by usage
    /// rather than hardcoded. `None` if this firmware exposes no such report.
    pub vendor_out_report_id: Option<u8>,
    /// Report id of the vendor IN channel (UPS -> host), resolved by usage.
    pub vendor_in_report_id: Option<u8>,
}

impl Descriptor {
    /// Reads the device's report descriptor and resolves every field this
    /// utility knows how to ask for.
    ///
    /// # Errors
    ///
    /// [`Error::Parse`], naming the `HidP` call that refused, when the
    /// capabilities or the value-caps arrays cannot be read — a device whose
    /// descriptor the OS itself will not describe. Propagates the same variant
    /// from [`LinkCollections::build`].
    ///
    /// A usage this utility looks for and does not find is *not* an error
    /// here: the field is left `None` and the reading degrades to a blank
    /// line. Firmware varies in what it publishes, and a UPS that reports no
    /// output wattage is a UPS with one fewer row, not a failure to connect.
    pub(crate) fn parse(dev: &RawDevice) -> Result<Self> {
        // Taken once and passed down. It is a [`Preparsed`], not the raw
        // pointer it wraps: it borrows `dev` for the whole of this function, so
        // the liveness every `HidP_*` call below depends on is a fact the
        // compiler enforces rather than one their `SAFETY` notes assert.
        let pp = dev.preparsed();

        let mut caps = HIDP_CAPS::default();
        // SAFETY: `caps` is a live local the call fills in; `pp` outlives the
        // call by its own borrow.
        let status = unsafe { HidP_GetCaps(pp.raw(), &mut caps) };
        if status.is_err() {
            return Err(Error::Parse("HidP_GetCaps"));
        }

        let link_map = LinkCollections::build(dev)?;

        let mut values = Vec::new();
        if caps.NumberFeatureValueCaps > 0 {
            let mut len = caps.NumberFeatureValueCaps;
            let mut buf = vec![HIDP_VALUE_CAPS::default(); len as usize];
            let status = {
                // SAFETY: `buf` has room for `len` entries — `len` is its own
                // length, passed by reference so the call can report how many
                // it wrote. `pp` outlives the call by its own borrow.
                unsafe { HidP_GetValueCaps(HidP_Feature, buf.as_mut_ptr(), &mut len, pp.raw()) }
            };
            // As for the button caps below: a failure here is the parse failing,
            // not an empty map. Swallowing it left `connect` succeeding on a
            // device whose fields could never be read, showing dashes with no
            // line in the log to say the descriptor did not parse.
            if status.is_err() {
                return Err(Error::Parse("HidP_GetValueCaps"));
            }
            for cap in buf.iter().take(len as usize) {
                // Range-form caps cover several usages; the fields we need
                // are all single-usage, so only NotRange is relevant here.
                if cap.IsRange {
                    continue;
                }
                // SAFETY: the union is read through the arm `IsRange` selects,
                // which was tested immediately above. Both arms are plain
                // integers, so there is no validity question beyond that.
                let usage = unsafe { cap.Anonymous.NotRange.Usage };
                values.push(ValueField {
                    page: cap.UsagePage,
                    usage,
                    report_id: cap.ReportID,
                    scope: link_map.scope_of(cap.LinkCollection),
                    link_collection: cap.LinkCollection,
                });
            }
        }

        let mut flags = Vec::new();
        if caps.NumberFeatureButtonCaps > 0 {
            let mut len = caps.NumberFeatureButtonCaps;
            let mut buf = vec![HIDP_BUTTON_CAPS::default(); len as usize];
            let status =
                // SAFETY: as `HidP_GetValueCaps` above — a buffer of `len`
                // entries, and a `pp` that outlives the call by its borrow.
                unsafe { HidP_GetButtonCaps(HidP_Feature, buf.as_mut_ptr(), &mut len, pp.raw()) };
            // A failure here is not a device with no buttons — that case is the
            // `NumberFeatureButtonCaps == 0` guard above. It is the parse itself
            // failing, which would otherwise leave `flags` empty, let `parse`
            // return `Ok`, and surface only as every status flag reading false.
            if status.is_err() {
                return Err(Error::Parse("HidP_GetButtonCaps"));
            }
            for cap in buf.iter().take(len as usize) {
                let scope = link_map.scope_of(cap.LinkCollection);
                if cap.IsRange {
                    // SAFETY: the `IsRange` arm, tested by the branch this is
                    // in.
                    let r = unsafe { cap.Anonymous.Range };
                    let (min, max) = (r.UsageMin, r.UsageMax);
                    let (range, truncated) = capped_usage_range(min, max);
                    if truncated {
                        crate::evlog::event(
                            crate::evlog::Cat::Device,
                            &format!(
                                "button cap range {min:#06x}..={max:#06x} truncated to {} usages",
                                range.clone().count()
                            ),
                        );
                    }
                    for u in range {
                        flags.push(FlagField {
                            page: cap.UsagePage,
                            usage: u,
                            report_id: cap.ReportID,
                            scope,
                            link_collection: cap.LinkCollection,
                        });
                    }
                } else {
                    flags.push(FlagField {
                        page: cap.UsagePage,
                        // SAFETY: the `NotRange` arm, tested by this branch.
                        usage: unsafe { cap.Anonymous.NotRange.Usage },
                        report_id: cap.ReportID,
                        scope,
                        link_collection: cap.LinkCollection,
                    });
                }
            }
        }

        // Vendor ASCII channel report ids, resolved by usage rather than
        // hardcoded. The OUT frame (host -> UPS) declares a length counter at
        // usage 0xff01:0x15 and the IN frame (UPS -> host) at 0xff01:0x16; the
        // report id carrying each counter is the id of that channel. These are
        // report 41 and 40 on this firmware, but reading them from the caps is
        // what keeps the self-test working if a later revision renumbers them.
        let vendor_out_report_id = first_report_id(
            pp,
            HidP_Output,
            caps.NumberOutputValueCaps,
            PAGE_VENDOR,
            VENDOR_OUT_COUNTER,
        );
        let vendor_in_report_id = first_report_id(
            pp,
            HidP_Input,
            caps.NumberInputValueCaps,
            PAGE_VENDOR,
            VENDOR_IN_COUNTER,
        );

        Ok(Self {
            values,
            flags,
            feature_len: caps.FeatureReportByteLength as usize,
            output_len: caps.OutputReportByteLength as usize,
            input_len: caps.InputReportByteLength as usize,
            vendor_out_report_id,
            vendor_in_report_id,
        })
    }

    /// Finds a value field by page, usage and collection.
    ///
    /// Scope matching is [`Scope::accepts`]: a named collection matches only
    /// itself, and `Unscoped` matches anything. One pass, one rule.
    ///
    /// It used to be two passes — first fields whose scope was *literally*
    /// `Unscoped`, then any field — which gave a usage sitting outside every
    /// known collection priority over the same usage inside one. That priority
    /// was never stated anywhere, never tested, and contradicted the
    /// doc-comment beside it, which described only the second pass. Nothing
    /// asks for it: `Unscoped` on the calling side means "this usage appears
    /// once, I do not care where", so the first match is the answer.
    pub(crate) fn find_value(&self, page: u16, usage: u16, scope: Scope) -> Option<ValueField> {
        self.values
            .iter()
            .find(|v| v.page == page && v.usage == usage && scope.accepts(v.scope))
            .copied()
    }

    /// Finds a flag by page, usage and collection, symmetric to `find_value`
    /// down to the scope rule.
    ///
    /// The asymmetry this removes was real: a usage can appear in several
    /// `PresentStatus` collections (Input, Output, `PowerSummary` — that is how a
    /// HID Power Device is built), and taking the first `page:usage` match
    /// ignored which one. `Scope::Unscoped` keeps the old behaviour for the
    /// flags that appear exactly once, accepting any collection.
    pub(crate) fn find_flag(&self, page: u16, usage: u16, scope: Scope) -> Option<FlagField> {
        self.flags
            .iter()
            .find(|f| f.page == page && f.usage == usage && scope.accepts(f.scope))
            .copied()
    }
}

/// Report id of the first value field on `report_type` matching `page:usage`.
///
/// A narrow probe for the two vendor channel reports, whose value fields are
/// not modelled in the feature-only `values`/`flags` lists. `NotRange` only:
/// the counters looked up here are single usages, and a range cap would not
/// name one anyway.
fn first_report_id(
    pp: Preparsed<'_>,
    report_type: HIDP_REPORT_TYPE,
    count: u16,
    page: u16,
    usage: u16,
) -> Option<u8> {
    if count == 0 {
        return None;
    }
    let mut len = count;
    let mut buf = vec![HIDP_VALUE_CAPS::default(); count as usize];
    // SAFETY: `buf` holds `len` entries, and `pp` outlives the call by its own
    // borrow, as everywhere else this call appears.
    let status = unsafe { HidP_GetValueCaps(report_type, buf.as_mut_ptr(), &mut len, pp.raw()) };
    if status.is_err() {
        return None;
    }
    buf.iter().take(len as usize).find_map(|cap| {
        if cap.IsRange || cap.UsagePage != page {
            return None;
        }
        // SAFETY: the `NotRange` arm, tested by the filter this closure is in.
        (unsafe { cap.Anonymous.NotRange.Usage } == usage).then_some(cap.ReportID)
    })
}

/// Reads a numeric field out of an already-fetched **feature** report.
///
/// Scoped to the field's own link collection, not to the report as a whole:
/// the `field.link_collection` passed to `HidP_GetUsageValue` is what tells the
/// API which occurrence of `page:usage` to read when the same usage appears
/// more than once in one report. Passing `0` ("any collection") returned the
/// first match, which is correct only while every duplicate sits in its own
/// report — true on this firmware, not guaranteed on another.
///
/// Not parameterised by report type. It used to take an `is_feature: bool`
/// selecting between `HidP_Feature` and `HidP_Input`, and every call site in
/// the crate passed `true` — so the `HidP_Input` branch was unreachable, and
/// the flag's only real effect was to let a future caller pass `false` and get
/// a silent misread against a buffer fetched as a feature report.
///
/// # Errors
///
/// [`Error::UsageMissing`], carrying the page, usage, report id and the
/// `HidP` status. The status is what tells the three causes apart:
/// `HIDP_STATUS_USAGE_NOT_FOUND` means the usage is not in the report that was
/// passed — normally that the report id was resolved from a different report
/// than the one fetched; `HIDP_STATUS_INVALID_REPORT_LENGTH` means the buffer
/// did not match the declared length, which is this code's error and not the
/// device's; `HIDP_STATUS_INCOMPATIBLE_REPORT_ID` means the field lives in
/// another report entirely.
pub(crate) fn read_value(dev: &RawDevice, field: ValueField, report: &mut [u8]) -> Result<u32> {
    let mut out: u32 = 0;
    let report_type = HidP_Feature;
    // SAFETY: `report` is a live slice of the report just read, passed with its
    // own length; the preparsed data outlives the call by its own borrow.
    let status = unsafe {
        HidP_GetUsageValue(
            report_type,
            field.page,
            Some(field.link_collection),
            field.usage,
            &mut out,
            dev.preparsed().raw(),
            report,
        )
    };
    if status.is_err() {
        // The NTSTATUS is the diagnosis. `USAGE_NOT_FOUND` against a report
        // that was fetched successfully means the map resolved this field to
        // the wrong report id; `INVALID_REPORT_LENGTH` means the buffer was
        // sized from a different declaration than the one being parsed. Both
        // are bugs here rather than device faults, and both were previously
        // indistinguishable from the device simply not having the field.
        return Err(Error::UsageMissing {
            page: field.page,
            usage: field.usage,
            rid: field.report_id,
            status: status.0,
        });
    }
    Ok(out)
}

/// Reads a button (flag) out of a **feature** report, returning whether it
/// is set. See `read_value` for why this is not parameterised by report type.
///
/// # Errors
///
/// [`Error::UsageMissing`], on the same three causes [`read_value`]
/// distinguishes by status. A flag that is simply *clear* is `Ok(false)`, not
/// an error: absence from the returned usage list is how HID says "off".
pub(crate) fn read_flag(dev: &RawDevice, field: FlagField, report: &mut [u8]) -> Result<bool> {
    let report_type = HidP_Feature;
    // Sized from the descriptor, not a guessed constant: `HidP_GetUsages`
    // refuses a buffer smaller than `HidP_MaxUsageListLength` reports for the
    // page, and hardcoding a capacity is the same mistake as hardcoding an
    // offset — right for this firmware, silently wrong for the next. The call
    // is a lookup in the already-parsed data, not a device transfer.
    let cap =
        // SAFETY: asks how long a usage list can be for this page; the
        // preparsed data outlives the call by its own borrow.
        unsafe { HidP_MaxUsageListLength(report_type, Some(field.page), dev.preparsed().raw()) }
            .max(1);
    let mut list = vec![0u16; cap as usize];
    let mut len = cap;
    // SAFETY: `list` has room for `len` usages — the length `HidP_MaxUsageListLength`
    // just reported — and `report` is a live slice passed with its own length.
    let status = unsafe {
        HidP_GetUsages(
            report_type,
            field.page,
            Some(field.link_collection),
            list.as_mut_ptr(),
            &mut len,
            dev.preparsed().raw(),
            report,
        )
    };
    if status.is_err() {
        return Err(Error::UsageMissing {
            page: field.page,
            usage: field.usage,
            rid: field.report_id,
            status: status.0,
        });
    }
    // Not a failure: the call succeeded and the usage is simply not among the
    // buttons currently set, which is how a flag reports false. The debug
    // line records what *was* set, because "the flag is false" and "this
    // report carries different buttons than expected" look identical here and
    // only the list of returned usages tells them apart.
    // `len` is an out-parameter of `HidP_GetUsages`, so `len <= cap` is the
    // kernel's promise and nothing this code can check after the fact. Every
    // other value that arrives across the FFI boundary is bounded before it is
    // used as a length — `enumerate_hid_paths` scans a device path against the
    // remainder of its own allocation rather than against a returned size —
    // and this was the one place that took the number on trust. It is not a
    // guard against a defect that has been observed: it is the same discipline
    // applied to the same class of value, which is what makes the absence of a
    // bound elsewhere meaningful.
    let set = list.get(..len as usize).unwrap_or(&list);
    let present = set.contains(&field.usage);
    // Told on both outcomes, unconditionally. The line is written only for a
    // flag that reads false, but that is the logger's decision to make: the
    // count behind the line says how many readings in a row the flag has been
    // false, and only a reading in which it was true can end that run. Branch
    // here and the true case is a branch somebody has to remember to write —
    // which is how it came to be missing.
    crate::evlog::debug_buttons(field.page, field.usage, field.report_id, present, set);
    Ok(present)
}

/// Writes a numeric field into a feature report buffer by usage.
///
/// The counterpart to `read_value`: `HidP_SetUsageValue` places `value` at the
/// bit offset the descriptor assigns to this field's `page:usage` inside its
/// link collection. The caller has already fetched the report, so the other
/// fields keep their current bytes — this is the write half of a read-modify-
/// write, and hardcoding the offset instead would be the same firmware-specific
/// mistake the read side avoids.
///
/// # Errors
///
/// [`Error::UsageMissing`], carrying the page, usage, report id and the `HidP`
/// status, when the field will not place — the same three causes
/// [`read_value`] distinguishes by status, reached from the write side. The
/// buffer is left as the caller fetched it, so a refused write does not send a
/// half-modified report back to the device.
pub(crate) fn write_value(
    dev: &RawDevice,
    field: ValueField,
    report: &mut [u8],
    value: u32,
) -> Result<()> {
    // SAFETY: `report` is a live mutable slice passed with its own length, and
    // the call writes only within the field the preparsed data locates in it.
    let status = unsafe {
        HidP_SetUsageValue(
            HidP_Feature,
            field.page,
            Some(field.link_collection),
            field.usage,
            value,
            dev.preparsed().raw(),
            report,
        )
    };
    if status.is_err() {
        return Err(Error::UsageMissing {
            page: field.page,
            usage: field.usage,
            rid: field.report_id,
            status: status.0,
        });
    }
    Ok(())
}

/// The two `HidP` entry points that *write* into a report buffer, declared here
/// rather than taken from `windows-rs`.
///
/// The generated bindings type the report parameter as `&[u8]` — a **shared**
/// slice — and then `transmute` its pointer to write through it. That is a
/// defect in the binding, not merely an inconvenience: writing through a
/// pointer derived from a shared reference is forbidden by Rust's aliasing
/// model regardless of whether any second reference exists, so the call cannot
/// be made sound by holding the buffer exclusively. The earlier version of this
/// module argued exactly that, which is the same class of mistake as a
/// `unsafe impl Send` justified by a rule the code does not follow: an
/// explanation that does not describe the hazard it is explaining.
///
/// Declaring the imports with their real C signature — `PCHAR Report` is an
/// out-parameter — removes the problem instead of arguing about it. The pointer
/// comes from `as_mut_ptr()` on the caller's `&mut [u8]`, so it carries write
/// provenance and the call is sound by construction. Every type crossing the
/// boundary is `#[repr(transparent)]` over its C equivalent, so the ABI is the
/// one `hid.dll` exports.
///
/// Still true at `windows` 0.62, checked against the generated source rather
/// than assumed: the parameter is `report: &[u8]` and the body reaches it with
/// `core::mem::transmute(report.as_ptr())`. Six minor versions have not moved
/// it, so this block is not a workaround waiting on an upstream fix — it is how
/// these two functions are called from this crate.
mod hid_ffi {
    use windows::Win32::Devices::HumanInterfaceDevice::{HIDP_REPORT_TYPE, PHIDP_PREPARSED_DATA};
    use windows::Win32::Foundation::NTSTATUS;

    // The same linkage the `windows` crate itself emits for `hid.dll`, rather
    // than a name for the linker to resolve through an import library. Written
    // out here because this block is hand-declared: `windows-link` expands to
    // exactly this attribute on every non-x86 Windows target, and this utility
    // builds for x86_64 only. Spelling it `name = "hid"` instead made this the
    // one place in the tree that required `hid.lib` (MSVC) or `libhid.a`
    // (mingw) on the build machine — a requirement on the environment that
    // nothing else in the project has and no document stated.
    #[link(name = "hid.dll", kind = "raw-dylib", modifiers = "+verbatim")]
    extern "system" {
        pub(crate) fn HidP_SetUsages(
            report_type: HIDP_REPORT_TYPE,
            usage_page: u16,
            link_collection: u16,
            usage_list: *mut u16,
            usage_length: *mut u32,
            preparsed_data: PHIDP_PREPARSED_DATA,
            report: *mut u8,
            report_length: u32,
        ) -> NTSTATUS;

        pub(crate) fn HidP_UnsetUsages(
            report_type: HIDP_REPORT_TYPE,
            usage_page: u16,
            link_collection: u16,
            usage_list: *mut u16,
            usage_length: *mut u32,
            preparsed_data: PHIDP_PREPARSED_DATA,
            report: *mut u8,
            report_length: u32,
        ) -> NTSTATUS;
    }
}

/// Sets or clears a button (flag) in a feature report buffer by usage.
///
/// The counterpart to `read_flag`. A flag is a button, not a byte: its position
/// in the report is whatever the descriptor assigns, so it is set through
/// `HidP_SetUsages` / cleared through `HidP_UnsetUsages` rather than by writing
/// a raw byte. The report must be fetched first so the surrounding bits — other
/// vendor flags in the same report — are preserved.
///
/// # Errors
///
/// [`Error::UsageMissing`] when the flag will not place, on the same causes
/// [`write_value`] reports — with the one exception [`flag_write_refused`]
/// names: clearing a flag that is already clear reports
/// `HIDP_STATUS_BUTTON_NOT_PRESSED` and is `Ok`, because the state the caller
/// asked for already holds.
pub(crate) fn write_flag(
    dev: &RawDevice,
    field: FlagField,
    report: &mut [u8],
    on: bool,
) -> Result<()> {
    let mut usage = field.usage;
    let mut len: u32 = 1;
    // SAFETY: both entry points have the signature declared for them in
    // `hid_ffi`, which is the one `hid.dll` exports. `usages` is a live local
    // array passed with its own length, and `report` is a live mutable slice
    // likewise — the call writes only inside the field it names.
    let status = unsafe {
        let f = if on {
            hid_ffi::HidP_SetUsages
        } else {
            hid_ffi::HidP_UnsetUsages
        };
        f(
            HidP_Feature,
            field.page,
            field.link_collection,
            &mut usage,
            &mut len,
            dev.preparsed().raw(),
            report.as_mut_ptr(),
            report.len() as u32,
        )
    };
    if flag_write_refused(status, on) {
        return Err(Error::UsageMissing {
            page: field.page,
            usage: field.usage,
            rid: field.report_id,
            status: status.0,
        });
    }
    Ok(())
}

/// Whether a status from `HidP_SetUsages` / `HidP_UnsetUsages` is a refusal.
///
/// Every failing status is, with one exception:
/// `HIDP_STATUS_BUTTON_NOT_PRESSED` arriving from the *clearing* direction.
/// `HidP_UnsetUsages` returns it when the usage it was asked to clear is not
/// set in the buffer it was handed — a statement about the buffer, not a
/// refusal to act on it, and the postcondition the caller wanted (the flag is
/// clear) already holds. Reporting it as an error made the very first write of
/// the self-test gate fail on a device whose vendor report reads back all
/// zeros, which is the resting state of report 37 and therefore the ordinary
/// case rather than an edge one.
///
/// The tolerance is deliberately one-sided. `HidP_SetUsages` has no
/// "already set" status — setting a button that is already set is plain
/// success — so this code arriving from the setting direction would mean
/// something this module does not understand, and it stays an error rather than
/// being swallowed by a symmetry the API does not have.
///
/// A separate function rather than a branch inside `write_flag`, because the
/// rule it encodes is precisely what used to be asserted in a doc comment and
/// never measured: the old text claimed `HidP` treats both directions as the
/// state the caller asked for. As a function it is a value a test can pin, and
/// the two directions can be told apart without a device.
fn flag_write_refused(status: NTSTATUS, on: bool) -> bool {
    if status.is_ok() {
        return false;
    }
    let already_clear = !on && status == HIDP_STATUS_BUTTON_NOT_PRESSED;
    !already_clear
}

/// Expands the inclusive `UsageMin..=UsageMax` of a range-form cap, clamped to
/// [`MAX_USAGE_RANGE`] entries. The flag says whether the clamp took effect.
///
/// The range is device-supplied data: a corrupt or hostile descriptor
/// declaring `UsageMin = 0, UsageMax = 0xFFFF` would expand to 65 536 entries
/// per cap, so the span is capped rather than allocated.
///
/// Returned inclusive because the HID range *is* inclusive. Expressed as an
/// exclusive `min..min + span` in `u16`, the end saturates back onto the start
/// whenever the range touches the top of the type — `UsageMin == UsageMax ==
/// 0xFFFF` is one usage, and the exclusive form yielded none at all.
fn capped_usage_range(min: u16, max: u16) -> (std::ops::RangeInclusive<u16>, bool) {
    // Counted as "usages after the first" rather than as a span, so every value
    // here stays inside `u16`. A span of 0x10000 does not fit and is what used
    // to force the arithmetic into `u32`; the count of *further* usages is at
    // most 0xFFFF and always does.
    //
    // A descriptor with max < min declares an empty range; `saturating_sub`
    // makes that one usage rather than a wrap-around, which is the reading that
    // loses nothing the device meant to declare.
    let further = max.saturating_sub(min);
    let capped = further.min(MAX_USAGE_RANGE - 1);
    // `capped <= further = max - min`, so this lands at or below `max` and
    // cannot overflow. That is the argument the `u32` detour used to stand in
    // for, and it is now short enough to make in the type instead.
    let last = min + capped;
    (min..=last, further >= MAX_USAGE_RANGE)
}

/// Link collection tree, used only to resolve which physical collection a
/// duplicated usage (Voltage, `ConfigVoltage`) sits under.
struct LinkCollections {
    nodes: Vec<(u16, u16, u16)>, // (usage_page, usage, parent_index)
}

impl LinkCollections {
    /// Reads the collection tree out of the preparsed data.
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] when `HidP_GetLinkCollectionNodes` refuses. A device
    /// with no collections at all is not an error — the tree is simply empty,
    /// and every usage then resolves at the top level.
    fn build(dev: &RawDevice) -> Result<Self> {
        use windows::Win32::Devices::HumanInterfaceDevice::{
            HidP_GetLinkCollectionNodes, HIDP_LINK_COLLECTION_NODE,
        };

        let mut count: u32 = 0;
        // First call reports the node count via STATUS_BUFFER_TOO_SMALL.
        // SAFETY: the sizing call. A null buffer with a count of zero is how
        // this function is asked how many nodes there are, and `count` is a
        // live local it writes the answer into.
        let _ = unsafe {
            HidP_GetLinkCollectionNodes(std::ptr::null_mut(), &mut count, dev.preparsed().raw())
        };
        if count == 0 {
            // A Power Device with no collections at all is not something this
            // firmware does, but an empty tree is still a usable answer: every
            // field simply comes out `Unscoped`, and the unscoped fallback in
            // `find_value` then resolves the usages that appear once. Degrading
            // is acceptable here; degrading *silently* is not, because the
            // symptom — three Voltage fields collapsing onto one — names
            // nothing about its cause.
            crate::evlog::event(
                crate::evlog::Cat::Device,
                "descriptor reports no link collections; scoped lookups will fall back to unscoped",
            );
            return Ok(Self { nodes: Vec::new() });
        }

        let mut buf = vec![HIDP_LINK_COLLECTION_NODE::default(); count as usize];
        let status =
            // SAFETY: the same call with room this time — `buf` holds `count`
            // nodes, which is the number the sizing call above reported.
            unsafe { HidP_GetLinkCollectionNodes(buf.as_mut_ptr(), &mut count, dev.preparsed().raw()) };
        // A failure here is the parse failing, exactly as for `HidP_GetValueCaps`
        // and `HidP_GetButtonCaps` above — not an empty tree. Returning `Ok`
        // with no nodes left every field `Unscoped`, so the three scoped
        // Voltage lookups all missed, `poll` saw no voltages at all and
        // declared the device unresponsive after three tries. The device then
        // went grey and reconnected forever, with nothing in the log to say the
        // descriptor had not parsed.
        if status.is_err() {
            return Err(Error::Parse("HidP_GetLinkCollectionNodes"));
        }

        let nodes = buf
            .iter()
            .take(count as usize)
            .map(|n| (n.LinkUsagePage, n.LinkUsage, n.Parent))
            .collect();
        Ok(Self { nodes })
    }

    /// Walk up the collection tree until a node with a known power-topology
    /// usage is reached. The device nests Voltage under Input / Output /
    /// `PowerSummary`, and that parent is the only way to tell them apart.
    fn scope_of(&self, mut index: u16) -> Scope {
        let mut hops = 0;
        while hops < 32 {
            // The bound and the read are one step: a node index that is not in
            // the table ends the walk, instead of being tested against the
            // length on one line and used as a subscript on the next.
            let Some(&(page, usage, parent)) = self.nodes.get(index as usize) else {
                break;
            };
            if page == PAGE_POWER {
                match usage {
                    COLL_INPUT => return Scope::Input,
                    COLL_OUTPUT => return Scope::Output,
                    COLL_POWER_SUMMARY => return Scope::PowerSummary,
                    _ => {}
                }
            }
            if parent == index {
                break;
            }
            index = parent;
            hops += 1;
        }
        Scope::Unscoped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Devices::HumanInterfaceDevice::{
        HIDP_STATUS_SUCCESS, HIDP_STATUS_USAGE_NOT_FOUND,
    };

    // A voltage usage, chosen because it is the real duplicated case: the
    // device carries Voltage under Input, Output and PowerSummary at once.
    const U_VOLTAGE: u16 = 0x30;

    fn value(usage: u16, rid: u8, scope: Scope, link: u16) -> ValueField {
        ValueField {
            page: PAGE_POWER,
            usage,
            report_id: rid,
            scope,
            link_collection: link,
        }
    }

    fn flag(usage: u16, rid: u8, scope: Scope, link: u16) -> FlagField {
        FlagField {
            page: PAGE_POWER,
            usage,
            report_id: rid,
            scope,
            link_collection: link,
        }
    }

    /// A one-usage range at the top of `u16` used to expand to nothing.
    ///
    /// `UsageMin == UsageMax == 0xFFFF` is a single usage. Written as the
    /// exclusive `min..min.saturating_add(span)` in `u16`, the end saturated
    /// back onto the start and the range came out empty, so the flag the device
    /// declared never reached `flags` — silently, and only at the one boundary
    /// no fixture ever reaches.
    #[test]
    fn a_usage_range_touching_the_top_of_u16_keeps_its_last_entry() {
        let (range, truncated) = capped_usage_range(0xFFFF, 0xFFFF);
        assert_eq!(range.collect::<Vec<_>>(), vec![0xFFFF]);
        assert!(!truncated);

        let (range, truncated) = capped_usage_range(0xFFFD, 0xFFFF);
        assert_eq!(range.collect::<Vec<_>>(), vec![0xFFFD, 0xFFFE, 0xFFFF]);
        assert!(!truncated);
    }

    /// The ordinary case, and the clamp that protects against a descriptor
    /// declaring the whole usage space.
    #[test]
    fn a_usage_range_is_inclusive_and_clamped() {
        let (range, truncated) = capped_usage_range(0x10, 0x12);
        assert_eq!(range.collect::<Vec<_>>(), vec![0x10, 0x11, 0x12]);
        assert!(!truncated);

        let (range, truncated) = capped_usage_range(0x0000, 0xFFFF);
        assert_eq!(range.count(), usize::from(MAX_USAGE_RANGE));
        assert!(truncated);
    }

    fn descriptor(values: Vec<ValueField>, flags: Vec<FlagField>) -> Descriptor {
        Descriptor {
            values,
            flags,
            feature_len: 8,
            output_len: 0,
            input_len: 0,
            vendor_out_report_id: None,
            vendor_in_report_id: None,
        }
    }

    /// The core of A1: three Voltage fields differing only by collection must be
    /// told apart by scope, and each must resolve to its own field — not to
    /// whichever happens to be first in the list.
    #[test]
    fn scope_disambiguates_a_duplicated_value() {
        let desc = descriptor(
            vec![
                value(U_VOLTAGE, 1, Scope::Input, 10),
                value(U_VOLTAGE, 2, Scope::Output, 11),
                value(U_VOLTAGE, 3, Scope::PowerSummary, 12),
            ],
            vec![],
        );
        let input = desc
            .find_value(PAGE_POWER, U_VOLTAGE, Scope::Input)
            .unwrap();
        let output = desc
            .find_value(PAGE_POWER, U_VOLTAGE, Scope::Output)
            .unwrap();
        let battery = desc
            .find_value(PAGE_POWER, U_VOLTAGE, Scope::PowerSummary)
            .unwrap();
        // Each resolves to a distinct report and link collection, so the reads
        // that follow are scoped to different fields rather than collapsing onto
        // one — the failure the first-match lookup allowed.
        assert_eq!((input.report_id, input.link_collection), (1, 10));
        assert_eq!((output.report_id, output.link_collection), (2, 11));
        assert_eq!((battery.report_id, battery.link_collection), (3, 12));
    }

    /// A scoped lookup must not silently fall back to a different collection: if
    /// the requested scope is absent, that is a resolution failure, not a
    /// licence to return some other field.
    #[test]
    fn a_scoped_value_lookup_does_not_fall_back() {
        let desc = descriptor(vec![value(U_VOLTAGE, 1, Scope::Input, 10)], vec![]);
        assert!(desc
            .find_value(PAGE_POWER, U_VOLTAGE, Scope::Output)
            .is_none());
    }

    /// A usage that appears once is resolved by an unscoped lookup regardless of
    /// the collection it happens to sit in.
    #[test]
    fn an_unscoped_value_lookup_matches_any_collection() {
        let desc = descriptor(vec![value(0x66, 7, Scope::PowerSummary, 4)], vec![]);
        let f = desc.find_value(PAGE_POWER, 0x66, Scope::Unscoped).unwrap();
        assert_eq!(f.report_id, 7);
    }

    /// An unscoped lookup takes the first declared field and nothing else.
    ///
    /// This pins the rule that used to be an unwritten two-stage priority: a
    /// field literally outside every known collection was preferred over the
    /// same usage inside one, whichever came first in the descriptor. Now
    /// `Unscoped` means "any", so declaration order decides, and both orders
    /// are checked so the answer cannot come from a hidden preference for
    /// `Scope::Unscoped` fields.
    #[test]
    fn an_unscoped_lookup_takes_the_first_declared_field() {
        let inside_first = descriptor(
            vec![
                value(0x66, 7, Scope::PowerSummary, 4),
                value(0x66, 8, Scope::Unscoped, 5),
            ],
            vec![],
        );
        assert_eq!(
            inside_first
                .find_value(PAGE_POWER, 0x66, Scope::Unscoped)
                .unwrap()
                .report_id,
            7
        );

        let outside_first = descriptor(
            vec![
                value(0x66, 8, Scope::Unscoped, 5),
                value(0x66, 7, Scope::PowerSummary, 4),
            ],
            vec![],
        );
        assert_eq!(
            outside_first
                .find_value(PAGE_POWER, 0x66, Scope::Unscoped)
                .unwrap()
                .report_id,
            8
        );
    }

    /// The scope predicate is a lookup rule, not equality: it is deliberately
    /// asymmetric, and a scoped request must never be answered by a field the
    /// parser could not place in any collection.
    #[test]
    fn a_scoped_lookup_is_not_answered_by_an_unscoped_field() {
        let desc = descriptor(vec![value(U_VOLTAGE, 1, Scope::Unscoped, 10)], vec![]);
        assert!(desc
            .find_value(PAGE_POWER, U_VOLTAGE, Scope::Input)
            .is_none());
        assert!(desc
            .find_value(PAGE_POWER, U_VOLTAGE, Scope::Unscoped)
            .is_some());
    }

    /// B4: `find_flag` must be symmetric with `find_value`. A flag present in
    /// two collections is disambiguated by scope, not taken first-match.
    #[test]
    fn scope_disambiguates_a_duplicated_flag() {
        let desc = descriptor(
            vec![],
            vec![
                flag(0x1D, 1, Scope::Input, 20),
                flag(0x1D, 2, Scope::Output, 21),
            ],
        );
        let input = desc.find_flag(PAGE_POWER, 0x1D, Scope::Input).unwrap();
        let output = desc.find_flag(PAGE_POWER, 0x1D, Scope::Output).unwrap();
        assert_eq!((input.report_id, input.link_collection), (1, 20));
        assert_eq!((output.report_id, output.link_collection), (2, 21));
    }

    #[test]
    fn a_scoped_flag_lookup_does_not_fall_back() {
        let desc = descriptor(vec![], vec![flag(0x1D, 1, Scope::Input, 20)]);
        assert!(desc.find_flag(PAGE_POWER, 0x1D, Scope::Output).is_none());
    }

    #[test]
    fn an_unscoped_flag_lookup_matches_any_collection() {
        let desc = descriptor(vec![], vec![flag(0x40, 9, Scope::PowerSummary, 3)]);
        let f = desc.find_flag(PAGE_POWER, 0x40, Scope::Unscoped).unwrap();
        assert_eq!(f.report_id, 9);
    }

    /// A collection tree written out by hand, in the shape the device
    /// declares: a root that is its own parent, the three power collections
    /// under it, and a nested collection inside one of them.
    ///
    /// Synthetic rather than read from a device, and that costs nothing here:
    /// `scope_of` walks the node table and never touches the opaque preparsed
    /// data — only `build`, which fills the table in, does. The whole reason
    /// the tree is stored as plain triples is that the walk over it is
    /// answerable without Windows.
    fn tree() -> LinkCollections {
        LinkCollections {
            nodes: vec![
                // 0: the root device collection, its own parent.
                (PAGE_POWER, 0x04, 0),
                (PAGE_POWER, COLL_INPUT, 0),
                (PAGE_POWER, COLL_OUTPUT, 0),
                (PAGE_POWER, COLL_POWER_SUMMARY, 0),
                // 4: a nested collection inside Input — a physical collection
                // with no power-topology usage of its own.
                (PAGE_POWER, 0x00, 1),
                // 5: a vendor collection carrying the *number* of the Output
                // collection on another page.
                (PAGE_VENDOR, COLL_OUTPUT, 0),
            ],
        }
    }

    /// Which collection a field sits under, resolved by walking up.
    ///
    /// This is the answer that tells the three `Voltage` fields apart, and
    /// getting it wrong does not fail: the panel shows the mains voltage on
    /// the battery row and reads as a working utility. Every case the walk can
    /// hit is asked here, because none of them announces itself.
    #[test]
    fn a_field_takes_the_scope_of_the_collection_it_sits_in() {
        let tree = tree();

        // The three power collections, named directly.
        assert_eq!(tree.scope_of(1), Scope::Input);
        assert_eq!(tree.scope_of(2), Scope::Output);
        assert_eq!(tree.scope_of(3), Scope::PowerSummary);

        // A collection nested inside one of them inherits it: the walk climbs
        // until it finds a usage it knows.
        assert_eq!(tree.scope_of(4), Scope::Input);

        // The root has no power-topology usage and is its own parent, so the
        // walk ends there rather than looping.
        assert_eq!(tree.scope_of(0), Scope::Unscoped);

        // The usage number is only meaningful on the power page. A vendor
        // collection that happens to carry the same number is not the Output
        // collection, and treating it as one would scope a vendor field onto
        // the output readings.
        assert_eq!(tree.scope_of(5), Scope::Unscoped);

        // A node index the table does not hold — a descriptor that named a
        // collection the node list does not contain — ends the walk instead of
        // being used as a subscript.
        assert_eq!(tree.scope_of(99), Scope::Unscoped);
    }

    /// The walk visits a bounded number of nodes, and the bound is 32.
    ///
    /// The cycle test above proves the walk terminates; it cannot say after
    /// how long, because two nodes pointing at each other exhaust nothing.
    /// This says where the limit is, from both sides: a collection reachable
    /// on the last permitted hop is still resolved, and one node further is
    /// not. A limit that drifted upwards would not fail anything — the device
    /// nests three deep — it would only lengthen the walk a malformed
    /// descriptor can force on the poll thread, silently.
    #[test]
    fn the_collection_walk_stops_after_thirty_two_nodes() {
        // A chain of anonymous collections `0 -> 1 -> ... -> marked`, with the
        // marked one its own parent so the walk ends there rather than
        // wrapping.
        let chain = |depth: u16| {
            let mut nodes: Vec<(u16, u16, u16)> =
                (0..depth).map(|i| (PAGE_POWER, 0x00, i + 1)).collect();
            nodes.push((PAGE_POWER, COLL_INPUT, depth));
            LinkCollections { nodes }
        };

        // The marked node is the thirty-second read, which is the last one the
        // limit allows.
        assert_eq!(chain(31).scope_of(0), Scope::Input);
        // One node further is out of reach, and an unreachable answer is
        // `Unscoped` rather than a wrong collection.
        assert_eq!(chain(32).scope_of(0), Scope::Unscoped);
    }

    /// A parent cycle terminates.
    ///
    /// The hop limit is the only thing standing between a malformed descriptor
    /// and a poll thread that never returns, and nothing else in the walk can
    /// stop this: the two nodes below are each other's parent, so neither is
    /// its own parent and neither carries a usage the walk recognises. The
    /// device this was written against does not produce such a tree, which is
    /// exactly why the limit needs a test rather than a comment.
    #[test]
    fn a_cycle_in_the_collection_tree_does_not_hang_the_walk() {
        let looped = LinkCollections {
            nodes: vec![(PAGE_POWER, 0x00, 1), (PAGE_POWER, 0x00, 0)],
        };
        assert_eq!(looped.scope_of(0), Scope::Unscoped);
    }

    /// Clearing a flag that is already clear is not a refusal.
    ///
    /// This is the whole of the self-test regression: the gate write drives the
    /// flag low first to force a rising edge, `HidP_UnsetUsages` answered
    /// `HIDP_STATUS_BUTTON_NOT_PRESSED` because report 37 reads back all zeros
    /// when no test is pending, and the session aborted before it had written
    /// anything. The condition is the resting state of the device, so the path
    /// that failed was the only one an ordinary run takes.
    #[test]
    fn clearing_a_flag_that_is_already_clear_is_not_a_refusal() {
        assert!(!flag_write_refused(HIDP_STATUS_BUTTON_NOT_PRESSED, false));
    }

    /// The same status is still a refusal when the flag was being *set*.
    ///
    /// `HidP_SetUsages` has no "already set" outcome, so this status from the
    /// setting direction is not the benign case — it is a status this module
    /// has no account of, and swallowing it would hide it. The tolerance is
    /// one-sided on purpose and this is what holds it that way.
    #[test]
    fn button_not_pressed_while_setting_a_flag_is_still_a_refusal() {
        assert!(flag_write_refused(HIDP_STATUS_BUTTON_NOT_PRESSED, true));
    }

    /// A real failure is a refusal from either direction.
    ///
    /// `HIDP_STATUS_USAGE_NOT_FOUND` means the report handed in does not carry
    /// the usage at all — the descriptor and the buffer disagree, which is this
    /// code's mistake and has to surface as one whichever way the flag was
    /// going.
    #[test]
    fn a_usage_the_report_does_not_carry_is_a_refusal_either_way() {
        assert!(flag_write_refused(HIDP_STATUS_USAGE_NOT_FOUND, true));
        assert!(flag_write_refused(HIDP_STATUS_USAGE_NOT_FOUND, false));
    }

    /// Success is a refusal from neither direction.
    #[test]
    fn a_successful_flag_write_is_not_a_refusal() {
        assert!(!flag_write_refused(HIDP_STATUS_SUCCESS, true));
        assert!(!flag_write_refused(HIDP_STATUS_SUCCESS, false));
    }
}
