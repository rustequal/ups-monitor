//! Owned GDI handles, released by leaving scope.
//!
//! Every object here was previously created and destroyed by a matching pair of
//! calls written out by hand — `CreateSolidBrush` … `DeleteObject`,
//! `CreateCompatibleDC` … `DeleteDC`, `SelectObject` … `SelectObject` — about
//! ten pairs across the painter. A pair is only correct while nothing returns
//! between its halves, and nothing enforced that: an early `return` added to
//! `fill_bordered` or `draw_arrow` for any reason at all would have leaked a
//! kernel object on every repaint, once a second for as long as the panel is
//! open, with no symptom until the process runs out of handles.
//!
//! This project already answered that question three times — `DevInfo` for
//! `HDEVINFO`, `Event` for an event handle, `SelfTestSession` for the vendor
//! gate — each with the same reasoning written down: tying the release to a
//! guard makes it unconditional by construction. The painter was the one place
//! the reasoning had not been applied.
//!
//! # What each type guarantees
//!
//! * The handle is released exactly once, on every path out of the scope,
//!   including a panic.
//! * A [`Selection`] borrows the object it selected, so the object cannot be
//!   dropped while the device context still refers to it — deleting a selected
//!   GDI object is undefined behaviour and was previously prevented only by the
//!   order the lines happened to be written in.
//! * Construction is a safe function. The `unsafe` is inside, once per object,
//!   with the precondition stated beside it; callers get a value or `None` and
//!   no obligations. This is the project's rule that platform `unsafe` is
//!   isolated behind a safe module API, applied to GDI as it already was to
//!   HID.
//! * A [`Canvas`] is a device context together with a proof that it is live,
//!   so the routines that draw into one are safe functions rather than
//!   functions with a precondition. See its own documentation for why that
//!   distinction is the whole point of this module.
//!
//! # Why the boundary is here and not in a module of its own
//!
//! The fifth audit proposed collecting every Win32 call behind a `src/win/`
//! boundary. This module is that boundary for the part of Win32 where it pays:
//! GDI, where a handle is owned, a context has a lifetime, and the same
//! justification would otherwise be repeated at nearly sixty call sites.
//!
//! It stops there deliberately. The Win32 calls left outside — `CreateWindowExW`,
//! `TrackPopupMenu`, `RegisterDeviceNotificationW`, `SetTimer` — are made once
//! each. Wrapping a single call does not encapsulate its `unsafe`; it moves it
//! one floor down and adds a layer that has to be read to find out what
//! happens. The test worth applying is whether the justification repeats: for a
//! device context it repeated fifty-eight times and the type paid for itself
//! many times over, and for `CreatePopupMenu` it is written once.
//!
//! # What it deliberately does not do
//!
//! It does not wrap the drawing calls themselves. `FillRect`, `DrawTextW` and
//! `Polygon` take a device context and read it; they own nothing and leak
//! nothing, so a wrapper around them would add a layer without adding a
//! guarantee. What was worth taking away from the painter is the bookkeeping,
//! not the drawing.

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HBRUSH, HDC, HGDIOBJ, HPEN, PAINTSTRUCT,
    PS_SOLID,
};

use crate::color::Color;
use crate::ui::rect::Rect;
/// A solid-colour brush.
pub(crate) struct Brush(HBRUSH);

impl Brush {
    /// Creates a brush of `colour`, or `None` if GDI refuses.
    ///
    /// `None` rather than a brush that draws nothing: a caller that cannot have
    /// a brush cannot draw the thing it wanted, and silently drawing in some
    /// other colour would be worse than drawing nothing at all.
    pub(crate) fn solid(colour: Color) -> Option<Self> {
        // SAFETY: `CreateSolidBrush` reads the COLORREF by value and returns an
        // owned handle or a null one; there is no pointer and no lifetime
        // involved. The handle is released by `Drop` below.
        let h = unsafe { CreateSolidBrush(COLORREF(colour.to_colorref())) };
        (!h.is_invalid()).then_some(Self(h))
    }

    /// The raw handle, for the drawing calls that take an `HBRUSH` directly.
    pub(crate) fn raw(&self) -> HBRUSH {
        self.0
    }
}

impl Drop for Brush {
    fn drop(&mut self) {
        // SAFETY: the handle was produced by `CreateSolidBrush` and is owned by
        // this value, so it is live and unreleased here. A brush selected into
        // a DC cannot reach this point: `Selection` borrows the brush.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

/// A one-pixel solid pen.
///
/// The only kind this program draws with — every outline it makes is a single
/// hairline — so the width is not a parameter. A pen of another width would be
/// a different decision about how the interface looks, and it would be made in
/// the painter rather than here.
pub(crate) struct Pen(HPEN);

impl Pen {
    pub(crate) fn hairline(colour: Color) -> Option<Self> {
        // SAFETY: as `Brush::solid` — by-value arguments, an owned handle back.
        let h = unsafe { CreatePen(PS_SOLID, 1, COLORREF(colour.to_colorref())) };
        (!h.is_invalid()).then_some(Self(h))
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        // SAFETY: owned handle from `CreatePen`, not selected into any DC —
        // see `Brush::drop`.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

/// An off-screen bitmap compatible with a device context.
pub(crate) struct Bitmap(HBITMAP);

impl Bitmap {
    /// A `width` × `height` bitmap in the format of `canvas`.
    ///
    /// Non-positive dimensions are rejected here rather than handed to GDI: a
    /// zero-width bitmap is not an error the caller can do anything about, and
    /// it is the shape a client rectangle takes while a window is minimised.
    pub(crate) fn compatible(canvas: Canvas<'_>, width: i32, height: i32) -> Option<Self> {
        let (width, height) = drawable_extent(width, height)?;
        // SAFETY: `canvas` proves the context is live for the call, and the
        // dimensions are positive. The returned handle is owned here.
        let h = unsafe { CreateCompatibleBitmap(canvas.raw(), width, height) };
        (!h.is_invalid()).then_some(Self(h))
    }

    /// A top-down 32-bpp bitmap holding `bgra`, `size` pixels square.
    ///
    /// A DIB section rather than `CreateBitmap`, because `CreateBitmap` takes
    /// the bits as being in the device's own format: on a 32-bpp desktop that
    /// happens to line up, but nothing in the call states the layout, so the
    /// alpha channel travels on an assumption. A DIB section names its format
    /// in a `BITMAPINFOHEADER` — 32 bits, `BI_RGB`, negative height for
    /// top-down rows — so the buffer copied in means what it says.
    ///
    /// The pointer the section hands back never leaves this function, which is
    /// the reason the constructor takes the pixels rather than returning the
    /// pointer: the one thing a caller could get wrong is how much it writes
    /// through it, and that question is settled here against the length the
    /// header declares.
    pub(crate) fn dib_bgra(size: u32, bgra: &[u8]) -> Option<Self> {
        let expected = (size as usize).checked_mul(size as usize)?.checked_mul(4)?;
        if bgra.len() != expected {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `info` is a fully initialised header describing a
        // `size * size` true-colour surface. No device context is needed —
        // one is consulted only for `DIB_PAL_COLORS` — so a null `HDC` is the
        // documented argument here.
        let handle = unsafe {
            CreateDIBSection(
                // `None`, not a null handle wrapped in `Some`: the parameter
                // means "no reference DC, use the screen's format", and the
                // binding now says that in the type. The two are the same call
                // at the ABI, and only one of them says what is meant.
                None,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )
            .ok()?
        };
        let owned = Self(handle);
        if bits.is_null() {
            // `owned` releases the section on the way out; the older form had
            // to remember to delete it on this path by hand.
            return None;
        }
        // SAFETY: the section is exactly `expected` bytes — the header above
        // says so and GDI allocated to it — and `bgra` is that long, checked
        // above. The regions cannot overlap: one is ours, one is GDI's.
        unsafe {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        }
        Some(owned)
    }

    /// A one-bit-per-pixel mask bitmap, `size` pixels square.
    ///
    /// Device-dependent on purpose: a monochrome bitmap has one possible
    /// layout, so there is no format to declare and nothing to get wrong.
    ///
    /// The length is checked here, as `dib_bgra` checks its own, and for the
    /// same reason: `CreateBitmap` reads `stride * size` bytes through the
    /// pointer, so a buffer even one row short is a read past the allocation —
    /// undefined behaviour reached from safe code. The earlier justification
    /// named what the *caller* produces, which is precisely the thing a safe
    /// signature must not have to assume.
    ///
    /// The check doubles as the format's statement: rows of a device-dependent
    /// bitmap are padded to a 16-bit boundary, which is the padding rule the
    /// tray icon's mask once got wrong.
    pub(crate) fn monochrome(size: u32, bits: &[u8]) -> Option<Self> {
        let stride = (size as usize).div_ceil(16).checked_mul(2)?;
        if bits.len() != stride.checked_mul(size as usize)? {
            return None;
        }
        // SAFETY: `CreateBitmap` reads one bit per pixel over `size` rows of
        // `stride` bytes, which is exactly `bits.len()` as checked above, and
        // only for the duration of the call.
        let h = unsafe { CreateBitmap(size as i32, size as i32, 1, 1, Some(bits.as_ptr().cast())) };
        (!h.is_invalid()).then_some(Self(h))
    }

    /// The raw handle, for the calls that take an `HBITMAP` in a struct.
    pub(crate) fn raw(&self) -> HBITMAP {
        self.0
    }
}

/// The dimensions GDI can actually make a bitmap at, or `None`.
///
/// A free function rather than two comparisons inside `Bitmap::compatible`, so
/// the rule can be checked as a value. The alternative was a test that builds a
/// device context first — and a context is not guaranteed on every machine the
/// suite runs on, so such a test has to either skip itself or fail for a reason
/// that has nothing to do with the rule. Arithmetic that needs no context
/// should not be tested through one.
fn drawable_extent(width: i32, height: i32) -> Option<(i32, i32)> {
    (width > 0 && height > 0).then_some((width, height))
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: owned handle from `CreateCompatibleBitmap`. The `Selection`
        // that put it into the memory DC borrows this value, so it has been
        // restored before this runs.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

/// A device context that is live for `'a`, and the proof that it is.
///
/// Every drawing routine in this program takes one of these rather than an
/// `HDC`, which is what lets them be safe functions. An `HDC` is a bare
/// integer: nothing about it says whether the context still exists, so a
/// routine taking one has to state "the context must be live" as a precondition
/// and trust the caller. That precondition was written out on seventeen
/// functions and could only ever be checked by reading — and outlived its own
/// parameter: the sixth audit found all seventeen still saying "`dc` must be a
/// live device context" long after `dc` had become this type.
///
/// A `Canvas` cannot be built from a raw handle. It comes from a value that
/// owns a context and releases it on drop — [`MemDc`] or [`PaintDc`] — and
/// borrows that value, so the borrow checker refuses the one
/// arrangement the precondition was there to forbid: drawing into a context
/// that has already been released.
///
/// It is `Copy` and word-sized, and is passed by value everywhere. Taking it by
/// reference costs an indirection and says nothing extra — but a signature that
/// stores the lifetime, as [`Selection`] does, must name it (`Canvas<'a>`)
/// rather than elide it, or the proof arrives and is immediately discarded.
///
/// # What it does not prove
///
/// Which font is selected. Measuring and drawing must use the same one or a
/// column comes out narrower than the text in it, but that is a correctness
/// requirement, not a soundness one: getting it wrong misdraws the window, it
/// does not corrupt the process. It belongs in ordinary documentation and is
/// stated there, on the routines it constrains.
#[derive(Clone, Copy)]
pub(crate) struct Canvas<'a> {
    dc: HDC,
    _owner: std::marker::PhantomData<&'a ()>,
}
/// Win32 wants a colour as a `COLORREF`, and only Win32 does.
///
/// An inherent method on a type declared in `color.rs`, written here because
/// that module is also compiled into the build script — which draws pixels
/// into a buffer and never speaks to GDI. A conversion the build script can
/// never call is one it would have to be told to ignore, and being told to
/// ignore things is how a lint stops being read.
impl Color {
    /// `0x00BBGGRR`, the byte order every Win32 GDI call wants. Note this is
    /// the reverse of the usual `#RRGGBB` reading order, which is exactly the
    /// kind of detail worth centralising in one place.
    #[must_use]
    pub(crate) const fn to_colorref(self) -> u32 {
        (self.b as u32) << 16 | (self.g as u32) << 8 | self.r as u32
    }
}

impl Canvas<'_> {
    /// The handle, for the FFI calls this module and the painter make.
    ///
    /// Handing out the raw handle does not reopen the hole it closed: the
    /// caller has to hold a live `Canvas` to get one, and cannot keep it past
    /// the borrow.
    pub(crate) fn raw(self) -> HDC {
        self.dc
    }
}

/// An off-screen device context, the back half of the double buffer.
pub(crate) struct MemDc(HDC);

impl MemDc {
    /// A context in the same pixel format as `canvas`.
    ///
    /// Compatibility is what makes the final `BitBlt` a copy rather than a
    /// conversion: the back buffer and the window agree on colour depth and
    /// layout because the buffer was made from the window's own context.
    pub(crate) fn compatible(canvas: Canvas<'_>) -> Option<Self> {
        Self::create(Some(canvas.raw()))
    }

    /// A context compatible with the screen.
    ///
    /// The measuring path wants a context in the display's format and has no
    /// window to take one from — passing no reference context is how that is
    /// asked for, rather than by handing in a null handle wrapped as if it
    /// were one.
    pub(crate) fn for_screen() -> Option<Self> {
        Self::create(None)
    }

    fn create(reference: Option<HDC>) -> Option<Self> {
        // SAFETY: `reference` is either live for the call — it came from a
        // `Canvas` — or absent, which asks for the screen's format. The result
        // is an owned context this value releases on drop.
        let h = unsafe { CreateCompatibleDC(reference) };
        (!h.is_invalid()).then_some(Self(h))
    }

    /// Borrows this context as something that can be drawn into.
    pub(crate) fn canvas(&self) -> Canvas<'_> {
        Canvas {
            dc: self.0,
            _owner: std::marker::PhantomData,
        }
    }
}

impl Drop for MemDc {
    fn drop(&mut self) {
        // SAFETY: owned DC from `CreateCompatibleDC`. Anything selected into it
        // is restored first, because every `Selection` borrows this value.
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// The device context `BeginPaint` lends for one `WM_PAINT`, returned by drop.
///
/// The last of the four Win32 pairs in this program to get a guard. It was the
/// pair whose halves sat furthest apart — `BeginPaint`, a call into the whole
/// of `paint`, then `EndPaint` — and the only one whose context was passed on
/// as a bare `HDC`, which is precisely why the painter needed a precondition
/// instead of a proof.
pub(crate) struct PaintDc {
    hwnd: HWND,
    ps: PAINTSTRUCT,
    dc: HDC,
}

impl PaintDc {
    /// Begins painting `hwnd`.
    ///
    /// # Safety
    ///
    /// Must be called from `hwnd`'s own window procedure while handling
    /// `WM_PAINT`. `BeginPaint` is defined only there: it consumes the update
    /// region, and calling it anywhere else leaves the window marked dirty forever.
    pub(crate) unsafe fn begin(hwnd: HWND) -> Self {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: `ps` is a live local the call fills in, and the caller
        // guarantees this is `WM_PAINT` for `hwnd`.
        let dc = unsafe { BeginPaint(hwnd, &mut ps) };
        Self { hwnd, ps, dc }
    }

    /// The rectangle Windows asked to have repainted.
    ///
    /// Converted on the way out: `rcPaint` is where a Win32 rectangle enters
    /// the painter, and this is the boundary, so it converts here rather than
    /// letting the foreign type travel one function further.
    pub(crate) fn dirty(&self) -> Rect {
        self.ps.rcPaint.into()
    }

    /// Borrows this context as something that can be drawn into.
    pub(crate) fn canvas(&self) -> Canvas<'_> {
        Canvas {
            dc: self.dc,
            _owner: std::marker::PhantomData,
        }
    }
}

impl Drop for PaintDc {
    fn drop(&mut self) {
        // SAFETY: the pair of the `BeginPaint` above, with the same window and
        // the same `PAINTSTRUCT`, which has not been touched since.
        unsafe {
            let _ = EndPaint(self.hwnd, &self.ps);
        }
    }
}

/// An object selected into a device context, restored when the guard drops.
///
/// The lifetime is the point. A GDI object must not be deleted while a context
/// still holds it, and this borrows the object for as long as the selection
/// stands, so the compiler refuses the ordering that would do it. Written as
/// two hand-matched `SelectObject` calls, that ordering was correct only
/// because the lines happened to be in the right sequence.
pub(crate) struct Selection<'a> {
    dc: HDC,
    previous: HGDIOBJ,
    /// Borrows the selected object without naming its type: brushes, pens,
    /// bitmaps and fonts all go through here, and what matters is that the
    /// value outlives the selection, not which of them it is.
    _object: std::marker::PhantomData<&'a ()>,
}

/// A GDI object that can be selected into a device context.
///
/// Exists so [`Selection::new`] can take the handle out of the same value it
/// borrows. The previous signature took the two separately —
/// `new(dc, &brush, pen.as_gdi())` compiled and borrowed the wrong object — so
/// the one guarantee the type exists to give rested on the caller passing a
/// matching pair. Now there is no pair to mismatch.
pub(crate) trait GdiObject {
    /// The handle this value owns.
    fn as_gdi(&self) -> HGDIOBJ;
}

impl GdiObject for Brush {
    fn as_gdi(&self) -> HGDIOBJ {
        self.0.into()
    }
}

impl GdiObject for Pen {
    fn as_gdi(&self) -> HGDIOBJ {
        self.0.into()
    }
}

impl GdiObject for Bitmap {
    fn as_gdi(&self) -> HGDIOBJ {
        self.0.into()
    }
}

impl<'a> Selection<'a> {
    /// Selects `object` into `canvas` until the returned guard drops.
    ///
    /// The handle comes out of the borrowed value itself, which is what makes
    /// the borrow mean something: the object selected and the object held are
    /// the same value by construction, not by the caller passing them
    /// consistently. The context is borrowed for the same span, so neither half
    /// of the selection can go away while the other still refers to it.
    pub(crate) fn new<T: GdiObject>(canvas: Canvas<'a>, object: &'a T) -> Self {
        // SAFETY: `canvas` proves the context is live for `'a`, and the handle
        // belongs to `object`, which this value borrows for `'a` as well, so it
        // cannot be released while selected.
        let previous = unsafe { SelectObject(canvas.raw(), object.as_gdi()) };
        Self {
            dc: canvas.raw(),
            previous,
            _object: std::marker::PhantomData,
        }
    }

    /// Selects an object this module does not own the lifetime of.
    ///
    /// The fonts are the whole of this case: they are created once per thread
    /// into a `OnceCell`, outlive every window on that thread and are never
    /// destroyed, so there is no value to borrow. Only the context's lifetime
    /// is left to track, and `canvas` carries it.
    pub(crate) fn shared(canvas: Canvas<'a>, object: HGDIOBJ) -> Self {
        // SAFETY: `canvas` proves the context is live for `'a`; `object` is a
        // thread-local font that is never released.
        let previous = unsafe { SelectObject(canvas.raw(), object) };
        Self {
            dc: canvas.raw(),
            previous,
            _object: std::marker::PhantomData,
        }
    }
}

impl Drop for Selection<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.previous` is what this context held before, returned by
        // the `SelectObject` above, and `self.dc` is still live: `'a` on this
        // guard comes from the `Canvas` it was built with, which borrows the
        // value that owns the context, so that value cannot have been dropped
        // while this guard exists.
        //
        // The reason matters, and an earlier version of this note gave the
        // wrong one — that the selected object had not been dropped yet. That
        // says nothing about the *context*, which is what is being written to
        // here, and it is vacuous for `shared`, where there is no borrowed
        // object at all.
        unsafe {
            SelectObject(self.dc, self.previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bitmap with no area is refused before GDI is asked.
    ///
    /// A zero-area bitmap is not something GDI can produce, and the answer must
    /// be `None` rather than an invalid handle that `Drop` would later hand to
    /// `DeleteObject`. Checked against [`drawable_extent`] rather than through
    /// `Bitmap::compatible`, because the latter needs a device context and this
    /// rule does not: a test that skips itself when no context is available
    /// passes green having checked nothing, which is the worst way for a test
    /// to fail.
    #[test]
    fn a_bitmap_with_no_area_is_refused() {
        assert_eq!(drawable_extent(0, 10), None);
        assert_eq!(drawable_extent(10, 0), None);
        assert_eq!(drawable_extent(-1, -1), None);
        assert_eq!(drawable_extent(1, 1), Some((1, 1)));
    }

    /// A mask shorter than its own dimensions is refused rather than read past.
    ///
    /// `CreateBitmap` reads `stride * size` bytes through the pointer, so this
    /// is the check standing between a caller's arithmetic slip and a read
    /// outside the allocation. Rows pad to 16 bits: at 16 pixels square that is
    /// two bytes a row, 32 in all.
    #[test]
    fn a_mask_of_the_wrong_length_is_refused() {
        assert!(Bitmap::monochrome(16, &[0u8; 31]).is_none());
        assert!(Bitmap::monochrome(16, &[0u8; 33]).is_none());
        assert!(Bitmap::monochrome(16, &[]).is_none());
    }
}
