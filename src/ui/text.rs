//! Fonts, script runs, and the measuring and drawing of text.
//!
//! Measurement and painting live in one file on purpose. They are a pair:
//! [`text_width`] must split a string into runs exactly the way [`draw_text`]
//! does, or a column is sized in one typeface and painted in another, and the
//! text is clipped or trailed by a band of empty background. That failure has
//! happened more than once here, and every fix for it has been to move the two
//! closer together — a shared [`needs_script_pass`], a shared
//! [`item_pieces`]. Splitting them across files would undo that.
//!
//! `script` is a parameter throughout, never read from a global inside these
//! functions. Almost every caller passes the active language's family; the one
//! that does not is a dropdown option's native name, which is set in the
//! *option's* font whatever the interface language is.

use std::cell::RefCell;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DrawTextW, GetTextExtentExPointW, GetTextExtentPoint32W, SelectObject,
    SetTextColor, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CALCRECT, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK,
    FF_DONTCARE, FW_BOLD, FW_NORMAL, HFONT, OUT_DEFAULT_PRECIS,
};

use super::gdi::{Canvas, MemDc, Selection};
use super::row::DropdownItem;
use crate::color::Color;
use crate::ui::rect::Rect;
use crate::ui::Dpi;
/// One cached font: family, device height in pixels, and the regular/bold pair
/// built from them.
///
/// Height rather than the point size and DPI it came from, because height is
/// the whole of what those two decide — [`font_height`] is the only thing
/// `create_font` does with them. Keying on the pair meant the key held an
/// `f32`, and an `f32` key is compared with `==`: a theme change and a
/// differently-scaled monitor were both handled, but two sizes that round to
/// the same pixel height were cached twice, and a `font_points` that arrived as
/// `NaN` would have matched nothing ever again and grown the cache without
/// bound. An integer key is exactly comparable and is the thing the font
/// actually depends on.
type CachedFont = (&'static str, i32, (HFONT, HFONT));

thread_local! {
    /// Regular/bold font pairs, keyed by family and created on first use.
    ///
    /// One cache for every family including the default: a separate `OnceCell`
    /// for Segoe UI was the same lookup written twice once the per-language
    /// families arrived.
    static SCRIPT_FONTS: RefCell<Vec<CachedFont>> = const { RefCell::new(Vec::new()) };

    /// Set once the cache has passed [`SCRIPT_FONT_CEILING`], so the report
    /// below is written once per session rather than on every repaint.
    static FONT_CEILING_REPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The number of cached pairs beyond which the cache is no longer behaving as
/// its own doc-comment describes.
///
/// The set is bounded on two of its three axes and not on the third. Families
/// are five — Segoe UI plus the four scripts it cannot draw. Point sizes are
/// one per loaded theme. DPI is whatever `WM_DPICHANGED` delivers, and while
/// the shell only produces its own scale steps (100% through 400%, about a
/// dozen values), nothing in this code says so. Five families times a dozen
/// scales is sixty pairs, so sixty-four is the ceiling this cache is claimed
/// to live under: about a hundred and thirty GDI objects, against a default
/// per-process limit of ten thousand.
///
/// Passing it does not evict, and that is deliberate rather than unfinished.
/// The handles are handed out: `WindowState` keeps a copy of the pair it was
/// created with and re-reads it only on `WM_DPICHANGED`, and `draw_text`
/// selects a script font into the device context it is drawing through. There
/// is no point at which this module can know that a given `HFONT` is not
/// named by a live window or currently selected into a DC, so `DeleteObject`
/// here would replace bounded growth with a window drawing through a freed
/// handle — a worse failure, and a silent one. What the ceiling buys instead
/// is that the growth stops being invisible: crossing it is reported once, so
/// a machine that really does produce an unbounded stream of DPI values shows
/// up in the log rather than as slowly rising handle usage.
const SCRIPT_FONT_CEILING: usize = 64;

/// The UI fonts for the active language, at a given DPI.
///
/// Twenty of the twenty-four languages draw in Segoe UI; the other four name
/// a family that covers their script. The pair is cached per family and DPI,
/// so switching language creates two handles once per DPI in use and reuses
/// them thereafter — and a window moved to another monitor gets its own
/// pair rather than one sized for the monitor it left.
///
/// Reading the language here rather than threading it through every call
/// works because the windows are destroyed and rebuilt when the language
/// changes — the same mechanism that already re-measures the layout.
pub(super) fn fonts(script: Script) -> (HFONT, HFONT) {
    // Always Segoe UI, in every language. It ships with every supported
    // Windows and covers Latin, Cyrillic and Greek, so digits, units and
    // model codes are set in one typeface throughout.
    //
    // The four scripts it cannot draw are handled per run by `draw_text`,
    // which splits the string with `script_runs` and selects `script_font`
    // for the foreign runs; the interface font is not swapped wholesale.
    // Swapping it was the earlier design and it was wrong: Microsoft YaHei
    // and its siblings carry their own Latin glyphs, so selecting one
    // globally reset every number and unit in a different typeface.
    //
    // An embedded Noto Sans was the original plan and was dropped: nothing is
    // embedded here, because Segoe UI and the four script fonts are present
    // on every Windows edition this utility can run on.
    font_pair("Segoe UI", script.dpi, script.points)
}

/// A font for a named family at the UI size and a given DPI, created once and
/// cached.
///
/// Keyed by family name and DPI because the set is small and fixed — the
/// four scripts Segoe UI does not cover, times however many distinct
/// monitor DPIs are in use. Cached because `CreateFontW` on every option of
/// every repaint would be a GDI handle allocation inside the paint loop.
pub(super) fn script_font(family: &'static str, script: Script) -> HFONT {
    font_pair(family, script.dpi, script.points).0
}

/// The font choices a drawing call needs beyond its device context.
///
/// Two values that always travel together and were arriving separately: the
/// family through a parameter and the DPI through a thread-local the painter
/// set at the top of every repaint. That thread-local was a side channel
/// between two functions several calls apart — `paint` wrote it, `draw_text`'s
/// callee read it — and it existed for a value `paint` already held in
/// `state.dpi`. A side channel that carries a value its own caller has is a
/// parameter that was not passed.
///
/// It also made the drawing helpers untestable in the ordinary way: what they
/// drew depended on process state nobody in the call chain mentioned, so two
/// tests could not run at once without agreeing about it. Carried as an
/// argument, the dependency is visible in the signature and cannot be forgotten
/// or left stale from a previous window — which matters here, because
/// per-monitor DPI means the panel and the settings dialog can genuinely be at
/// two different scales at the same moment.
///
/// `Eq` is not derived, and cannot be: `points` is an `f32`. Nothing here needs
/// it — the one comparison is the measurement cache's, and two measurements are
/// the same only if the sizes they were made at have the same bits, which is
/// what `PartialEq` on `f32` answers for every value a theme can hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Script {
    /// Family the active language needs, or `None` for Segoe UI, which covers
    /// Latin, Cyrillic and Greek.
    pub family: Option<&'static str>,
    /// DPI of the window being drawn, so a script font is built at the scale it
    /// will be seen at.
    pub dpi: Dpi,
    /// Size of the UI font in points, from the theme being drawn with.
    ///
    /// Carried rather than read from a thread-local at the bottom of the call
    /// chain, for the reason the DPI beside it is: a font size that arrives out
    /// of band is a font size that can be stale, and every caller of this type
    /// already holds the theme it belongs to. The static it replaces was
    /// written in two places and read in two others, none of which appeared in
    /// any signature between them.
    pub points: f32,
}

/// The regular/bold pair for a family at a given DPI, created once and
/// cached.
///
/// One cache rather than one per weight: the two are always wanted together
/// (an interface needs both) and keying them separately meant two lookups and
/// two vectors for the same four families.
fn font_pair(family: &'static str, dpi: Dpi, points: f32) -> (HFONT, HFONT) {
    SCRIPT_FONTS.with(|c| {
        let mut map = c.borrow_mut();
        // A theme changing `font_points` and a window at another DPI both
        // move the height, so both miss the cache; nothing else can.
        let height = font_height(points, dpi);
        if let Some(f) = map.iter().find(|(n, h, _)| *n == family && *h == height) {
            return f.2;
        }
        let pair = (
            create_font(family, FW_NORMAL.0 as i32, height),
            create_font(family, FW_BOLD.0 as i32, height),
        );
        map.push((family, height, pair));
        if map.len() > SCRIPT_FONT_CEILING && !FONT_CEILING_REPORTED.with(std::cell::Cell::get) {
            FONT_CEILING_REPORTED.with(|c| c.set(true));
            crate::evlog::event(
                crate::evlog::Cat::Error,
                &format!(
                    "font cache holds {} pairs, past the expected ceiling of \
                     {SCRIPT_FONT_CEILING}; it is never evicted, so this will keep growing",
                    map.len()
                ),
            );
        }
        pair
    })
}

/// The `CreateFontW` height for a point size on a monitor at `dpi`.
///
/// Negative because Win32 reads a negative height as the character height
/// rather than the cell height, which is what a point size means. This is the
/// one place points become pixels, and it is why a font grows with a scaled
/// display instead of staying a fixed physical size on a 200% monitor.
///
/// `dpi` is the caller's — `GetDpiForWindow` on the window the font is for —
/// not read here from `GetDeviceCaps`: the process is per-monitor DPI aware
/// (`set_dpi_awareness`), so there is no single "current" DPI to ask a device
/// context for, only the DPI of a particular window on a particular monitor.
/// Passing it in is what lets two panels open on two differently-scaled
/// monitors each get the pixel size that is actually correct for the one they
/// are on.
///
/// The size is 11.25pt by default, not the 9pt that a first reading of the
/// old hard-coded `-15` suggests. That constant was a *pixel* height at 96
/// DPI, and -15px is 11.25pt, not 9pt — deriving 9pt from it silently shrank
/// every window by a fifth. The lesson is that a raw negative height carries
/// its DPI assumption invisibly; the point size here states it.
fn font_height(points: f32, dpi: Dpi) -> i32 {
    -dpi.points_to_pixels(points)
}

/// Creates one font of `family` at `weight`, sized by the device `height`
/// [`font_height`] returned.
///
/// It takes the height already in pixels rather than the point size and DPI it
/// came from, because the height is the whole of what those two decide and is
/// what [`CachedFont`] is keyed on: two point sizes that round to the same
/// pixel height are one font, and the key says so.
fn create_font(family: &str, weight: i32, height: i32) -> HFONT {
    let mut name: Vec<u16> = family.encode_utf16().collect();
    name.push(0);
    // SAFETY: `name` is a NUL-terminated local that outlives the call, which
    // copies the family name it needs. The font that comes back lives in the
    // thread-local cache for the life of the thread.
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            u32::from(FF_DONTCARE.0),
            PCWSTR(name.as_ptr()),
        )
    }
}

/// Splits text into runs that one font can draw, in order.
///
/// Each run is `(text, needs_script_font)`. The interface font is Segoe UI for
/// every language; only the characters it cannot draw are handed to the
/// language's own family.
///
/// This exists because tying the *whole* interface to the language's font was
/// wrong, and visibly so. Microsoft YaHei, Malgun Gothic and their siblings
/// carry their own Latin glyphs — a reworked Segoe UI, close but not
/// identical — so selecting one of them globally redrew every number, unit
/// and model code in subtly different letterforms. It read as a distorted
/// font, and no amount of checking `CreateFontW` would have found it, because
/// nothing was distorted: the text was simply being set in another typeface.
///
/// Splitting by script keeps digits, `V`, `W`, `VA` and serial numbers in one
/// typeface across all twenty-four languages, and gives the language's font
/// only the characters that actually need it.
fn script_runs(text: &str) -> Vec<(String, bool)> {
    // Everything Segoe UI covers: Latin, Latin Extended, Greek, Cyrillic, and
    // the punctuation and symbol blocks below Armenian.
    const SEGOE_COVERS: u32 = 0x0530;
    let mut runs: Vec<(String, bool)> = Vec::new();
    for ch in text.chars() {
        // Spaces and ASCII punctuation join whichever run precedes them:
        // breaking a run at every space would double the number of draw calls
        // for no visible difference, since both fonts render a space the same.
        let foreign = ch as u32 >= SEGOE_COVERS && !ch.is_whitespace();
        match runs.last_mut() {
            Some((s, f)) if *f == foreign => s.push(ch),
            _ => runs.push((ch.to_string(), foreign)),
        }
    }
    runs
}

/// Whether a split string needs the per-run font pass at all.
///
/// One predicate, called by both the measurer and the painter. They are a pair
/// — a string measured one way and drawn the other is the defect this whole
/// area keeps producing — and until now each spelled the condition out in its
/// own expression, differing in the empty-slice guard. Two expressions kept in
/// step by eye are the same failure mode as the two `match` tables `Row::height`
/// was rid of.
fn needs_script_pass(runs: &[(String, bool)]) -> bool {
    runs.len() > 1 || runs.first().is_some_and(|(_, foreign)| *foreign)
}

/// The pieces one dropdown option is drawn from, in order, each paired with
/// the font family its non-Latin runs must be set in.
///
/// The single rule for how an option is typeset, used by the painter and by
/// the measurer alike. The native name is why it has to exist: it is set in
/// the *option's* font — Japanese in Yu Gothic UI whatever the interface
/// language is — while the label and the parentheses around it follow the
/// active language like every other string in the window.
///
/// That rule used to live in two places and disagree with itself. The painter
/// selected `item.font` around a call that then substituted the *active*
/// language's font for exactly the runs it had selected it for, so the winner
/// was decided by nesting order; the measurer never saw `item.font` at all and
/// charged the column for the native name set in whatever family the interface
/// language happened to want — Segoe UI's fallback with an English interface. The column
/// was therefore sized in one typeface and painted in another, which shows up
/// as native names clipped on the right, or as a band of empty space beside
/// them.
///
/// The strings are owned rather than borrowed because the head piece is
/// composed here; the alternative is for both callers to compose it, which is
/// the duplication this is removing.
fn item_pieces(item: &DropdownItem, script: Script) -> Vec<(String, Option<&'static str>)> {
    let active = script.family;
    if item.native.is_empty() {
        return vec![(item.label.clone(), active)];
    }
    vec![
        (format!("{} (", item.label), active),
        // `or`, not `and_then`: an option with no font of its own is ordinary
        // interface text and follows the active language.
        (item.native.clone(), item.font.or(active)),
        (")".to_owned(), active),
    ]
}

/// Draws one option: Latin label, then the native name in parentheses.
///
/// Walks [`item_pieces`], so the fonts used here are the same ones
/// [`TextMetrics::option_width`] measured the column with, by construction
/// rather than by two lists agreeing.
pub(super) fn draw_item(
    canvas: Canvas<'_>,
    item: &DropdownItem,
    rect: Rect,
    fg: Color,
    bg: Color,
    script: Script,
) {
    let mut x = rect.left;
    // Each piece names its own family — an option's native name may need a font
    // the interface does not — but they are all drawn at the same DPI, so the
    // piece's family is put back into the caller's `Script` rather than into a
    // second notion of what DPI means.
    for (text, family) in item_pieces(item, script) {
        let piece = Script { family, ..script };
        draw_text(
            canvas,
            &text,
            Rect { left: x, ..rect },
            fg,
            DT_LEFT,
            bg,
            piece,
        );
        x += text_width(canvas, &text, piece);
    }
}

/// The width `draw_text` will draw this string at, given the same `script`.
///
/// Script-aware for the same reason `draw_text` is, and paired with it
/// deliberately: a string measured in one font and drawn in another is how
/// the columns ended up wider than the window that was sized for them. The
/// two functions split runs the same way — through [`needs_script_pass`] —
/// or the layout drifts.
///
/// `script` is the family the non-Latin runs are set in, passed in rather than
/// resolved here. Resolving it internally meant a caller could not say "this
/// particular string is set in *that* font", which is exactly what a dropdown
/// option's native name needs; see [`item_pieces`].
///
/// The caller selects the interface font into `canvas` first. That selection
/// is what the width is measured in, so a context carrying another font
/// answers a question the caller did not ask — which is how a column comes out
/// narrower than the text drawn into it.
pub(super) fn text_width(canvas: Canvas<'_>, text: &str, script: Script) -> i32 {
    if let Some(family) = script.family {
        let runs = script_runs(text);
        if needs_script_pass(&runs) {
            return runs_width_selected(canvas, &runs, family, script);
        }
    }
    measure_one(canvas, text)
}

/// Where each character of `text` ends, in pixels from its start.
///
/// One entry per `char`, so entry `i` is the width of the first `i + 1`
/// characters — the position the caret takes when it sits after that character.
///
/// One GDI call for the whole string. The caret used to be located by building
/// a `String` for every prefix and measuring it, which is a string allocation
/// and a `GetTextExtentPoint32W` per character boundary: quadratic work, and
/// quadratic in the wrong currency, since each step is a round trip into the
/// font engine. `GetTextExtentExPointW` answers the same question once and
/// returns the whole array, which is what it exists for.
///
/// Measured in the font currently selected into `dc`, with no per-run font
/// switching. That is the same measurement [`text_width`] performs for text
/// with no foreign runs in it, and the only text this is used on is the poll
/// interval — digits, which are never a foreign run in any of the shipped
/// locales. A field holding mixed scripts would need the run path and does not
/// exist.
///
/// The array GDI fills is indexed by UTF-16 code unit, not by `char`, so the
/// two are walked together rather than assumed equal: outside the BMP one
/// character occupies two units, and taking the wrong one would put the caret
/// inside a surrogate pair.
///
/// The caller selects the field's font into `canvas` first — the same font the
/// text is drawn in, or the returned boundaries name positions the caret does
/// not sit at.
pub(super) fn caret_offsets(canvas: Canvas<'_>, text: &str) -> Vec<i32> {
    let wide = crate::wide::unterminated(text);
    if wide.is_empty() {
        return Vec::new();
    }
    let mut widths = vec![0i32; wide.len()];
    let mut size = SIZE::default();
    // The length is passed separately because the buffer is not NUL-terminated
    // — `unterminated` is what its name says — so a `PCWSTR` alone would run
    // past the end looking for a zero that is not there.
    let Ok(count) = i32::try_from(wide.len()) else {
        return Vec::new();
    };
    // SAFETY: `canvas` proves the context is live; `wide` and `widths` are live
    // locals, and `widths` has one entry per character, which is the count
    // passed alongside it.
    let measured = unsafe {
        GetTextExtentExPointW(
            canvas.raw(),
            PCWSTR(wide.as_ptr()),
            count,
            // No width limit: every character is wanted, not as many as fit.
            i32::MAX,
            None,
            Some(widths.as_mut_ptr()),
            &mut size,
        )
    };
    if !measured.as_bool() {
        return Vec::new();
    }
    caret_offsets_from_widths(text, &widths)
}

/// Maps GDI's per-unit width array onto one offset per `char` of `text`.
///
/// The half of [`caret_offsets`] that has nothing to do with a device context:
/// `widths` is indexed by UTF-16 code unit, the caret is placed per character,
/// and outside the BMP those are not the same walk. Separated so the mapping
/// can be checked against a string with a surrogate pair in it and a short
/// array — see `a_caret_past_the_measured_units_falls_back`.
///
/// The index is spent through `get` rather than by subscript, and that is the
/// whole point of the separation being worth making. It is derived from a
/// `char` count and bounded by a buffer GDI filled, which are two different
/// measurements of the same string; a caret that stops one character short is
/// a wrong caret, and a caret that aborts the process is a wrong program.
fn caret_offsets_from_widths(text: &str, widths: &[i32]) -> Vec<i32> {
    let mut unit = 0usize;
    text.chars()
        .map(|ch| {
            unit += ch.len_utf16();
            unit.checked_sub(1)
                .and_then(|i| widths.get(i).copied())
                .unwrap_or(0)
        })
        .collect()
}

/// Width of `text` in the font the caller selected into `canvas`.
fn measure_one(canvas: Canvas<'_>, text: &str) -> i32 {
    let wide = crate::wide::unterminated(text);
    let mut size = SIZE::default();
    // SAFETY: `canvas` proves the context is live; `wide` is passed with its
    // own length and `size` is a live local.
    if unsafe { GetTextExtentPoint32W(canvas.raw(), &wide, &mut size) }.as_bool() {
        size.cx
    } else {
        0
    }
}

/// Draws a single line, switching fonts per script run where `script` names a
/// family the interface font cannot supply.
///
/// Run splitting lives here rather than at the call sites because there are a
/// dozen of them, and a rule applied at eleven of twelve is a rule that
/// produces one row in the wrong typeface and no error anywhere. *Which*
/// family to use is the caller's to say, though: almost every one passes the
/// window's own, and the one that does not — a dropdown option's native name —
/// is the reason this is a parameter. It used to be resolved here, so a caller
/// that had already selected another font had it silently overridden for
/// precisely the runs it selected it for.
///
/// The caller selects the interface font into `canvas` first. This function
/// selects the script family into it for the foreign runs and puts the
/// previous selection back, so the context is left as it was found.
pub(super) fn draw_text(
    canvas: Canvas<'_>,
    text: &str,
    rect: Rect,
    color: Color,
    align: DRAW_TEXT_FORMAT,
    bg: Color,
    script: Script,
) {
    if text.is_empty() {
        return;
    }
    // GDI text has no alpha channel, so a translucent theme colour must be
    // resolved against whatever it sits on before it reaches SetTextColor.
    // SAFETY: `canvas` proves the context is live; the colour is by value.
    unsafe { SetTextColor(canvas.raw(), COLORREF(color.over(bg).to_colorref())) };

    if let Some(family) = script.family {
        let runs = script_runs(text);
        if needs_script_pass(&runs) {
            // Centred text is laid out from its measured width, since the
            // runs are drawn left to right by hand and DrawTextW's own
            // centring only applies to one call at a time.
            let total = runs_width_selected(canvas, &runs, family, script);
            let mut x = if align == DT_CENTER {
                rect.left + ((rect.right - rect.left) - total) / 2
            } else {
                rect.left
            };
            for (run, foreign) in &runs {
                // The guard, not a hand-matched pair. The two `SelectObject`
                // calls this replaced were correct, and correct only because
                // nothing between them returned or panicked — which is the
                // property `Selection` exists to stop depending on.
                let _script =
                    foreign.then(|| Selection::shared(canvas, script_font(family, script).into()));
                draw_one(canvas, run, Rect { left: x, ..rect }, DT_LEFT);
                x += measure_one(canvas, run);
            }
            return;
        }
    }
    draw_one(canvas, text, rect, align);
}

/// One `DrawTextW` in the font the caller selected into `canvas`.
fn draw_one(canvas: Canvas<'_>, text: &str, rect: Rect, align: DRAW_TEXT_FORMAT) {
    let mut wide = crate::wide::unterminated(text);
    if wide.is_empty() {
        return;
    }
    let mut r: RECT = rect.into();
    // SAFETY: `canvas` proves the context is live; `wide` and `r` are live
    // locals the call reads and writes within their own bounds.
    unsafe {
        DrawTextW(
            canvas.raw(),
            &mut wide,
            &mut r,
            align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        )
    };
}

/// Total width of pre-split runs, measuring each in the font that will draw
/// it.
///
/// Each foreign run's font is selected into `canvas` and the previous
/// selection restored, so the context is left as it was found.
fn runs_width_selected(
    canvas: Canvas<'_>,
    runs: &[(String, bool)],
    family: &'static str,
    script: Script,
) -> i32 {
    let mut w = 0;
    for (run, foreign) in runs {
        // As in `draw_text` above: the restore is the guard's, not the author's.
        let _script =
            foreign.then(|| Selection::shared(canvas, script_font(family, script).into()));
        w += measure_one(canvas, run);
    }
    w
}

/// Draws wrapped text within `rect`. The caller sized `rect` with
/// [`TextMetrics::wrapped_height`], which is this same `DrawTextW` call under
/// `DT_CALCRECT` on this same context — so the block is exactly what the text
/// fills, and the caller's cursor advances by exactly what was drawn. Long
/// sentences wrap rather than being clipped, which is what a translation of
/// any length is owed.
///
/// The caller selects into `canvas` the font it measured with: the block is
/// sized by that measurement, so a different font here draws text the block
/// does not fit.
pub(super) fn draw_wrapped(canvas: Canvas<'_>, text: &str, rect: Rect, color: Color, bg: Color) {
    let mut wide = crate::wide::unterminated(text);
    if wide.is_empty() {
        return;
    }
    // SAFETY: `canvas` proves the context is live; the colour is by value.
    unsafe { SetTextColor(canvas.raw(), COLORREF(color.over(bg).to_colorref())) };
    let mut r: RECT = rect.into();
    // SAFETY: as above; `wide` and `r` are live locals.
    unsafe {
        DrawTextW(
            canvas.raw(),
            &mut wide,
            &mut r,
            DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
        )
    };
}

/// The two text measurements the layout cannot derive from the theme.
///
/// Both are questions only the font the text will be drawn in can answer: how
/// wide one line is, and how tall a paragraph becomes once wrapped. The painter
/// answers them from the device context it is drawing into; the sizing pass
/// from one created with the same fonts at the same DPI; the arithmetic tests
/// from a stand-in that says so in its name.
///
/// [`wrapped_height`](TextMetrics::wrapped_height) replaces an estimate — the
/// average glyph taken as 40 % of the row height, characters counted, lines
/// divided out. That estimate was calibrated on Cyrillic and Latin and had no
/// way to be right about anything else. A CJK glyph advances a full em, close
/// to twice the assumed average, so the estimate reported roughly half the
/// lines a Chinese, Japanese or Korean paragraph needs — and since the painter
/// clips wrapped text to the block reserved for it, the surplus line was cut
/// off rather than merely overflowing.
///
/// No refinement of the estimate could have closed that. The interface font is
/// Segoe UI in every language, so CJK text is drawn by GDI's own font
/// fallback, in a face this code never names and whose metrics it cannot know.
/// `DT_CALCRECT` on the same device context performs the same fallback and
/// returns the height the same call will consume. The measurement is not a
/// better guess; it is the thing itself.
pub(crate) trait TextMetrics {
    /// Width of `text` set on one line, in the active language's fonts.
    fn line_width(&mut self, text: &str, bold: bool) -> i32;
    /// Width of one dropdown option, label and native name together.
    ///
    /// Its own method rather than a `line_width` of the concatenation,
    /// because an option is not set in one font: the native name follows
    /// [`DropdownItem::font`] and the rest follows the active language. That
    /// is [`item_pieces`], and this is the measuring half of it — the painting
    /// half is [`draw_item`]. Charging the column through `line_width` was the
    /// defect: the width came back in whatever font the interface happened to
    /// be using, and the text was then painted in another.
    fn option_width(&mut self, item: &DropdownItem) -> i32;
    /// Height `text` occupies wrapped into `width`.
    fn wrapped_height(&mut self, text: &str, width: i32) -> i32;
}

/// [`TextMetrics`] answered by GDI, over a device context that already holds
/// the interface fonts.
///
/// Borrows the context rather than owning one, so the painter can measure with
/// the very context it is about to draw into — which is what makes the measured
/// height and the drawn height the same number by construction rather than by
/// two calls agreeing.
pub(super) struct GdiMetrics<'a> {
    /// The font choices every measurement here is made under, so a width
    /// measured is a width that will be drawn.
    pub(super) script: Script,
    /// The context every measurement is taken in, borrowed for as long as this
    /// value lives — so the guard that owns it cannot be dropped first.
    pub(super) canvas: Canvas<'a>,
    pub(super) font: HFONT,
    pub(super) bold: HFONT,
}

impl TextMetrics for GdiMetrics<'_> {
    fn line_width(&mut self, text: &str, bold: bool) -> i32 {
        // SAFETY: `self.canvas` proves the context is live for as long as this
        // value exists, and the font is a thread-local that outlives it.
        unsafe {
            SelectObject(
                self.canvas.raw(),
                if bold {
                    self.bold.into()
                } else {
                    self.font.into()
                },
            );
            let w = text_width(self.canvas, text, self.script);
            SelectObject(self.canvas.raw(), self.font.into());
            w
        }
    }

    fn option_width(&mut self, item: &DropdownItem) -> i32 {
        // SAFETY: `self.canvas` proves the context is live for as long as this
        // value exists, and the font is a thread-local that outlives it.
        unsafe {
            SelectObject(self.canvas.raw(), self.font.into());
            item_pieces(item, self.script)
                .iter()
                .map(|(text, family)| {
                    text_width(
                        self.canvas,
                        text,
                        Script {
                            family: *family,
                            ..self.script
                        },
                    )
                })
                .sum()
        }
    }

    fn wrapped_height(&mut self, text: &str, width: i32) -> i32 {
        if text.is_empty() {
            return 0;
        }
        // SAFETY: `self.canvas` proves the context is live and the font is a
        // thread-local that outlives it; `wide` and `r` below are live locals
        // the call measures into.
        unsafe {
            SelectObject(self.canvas.raw(), self.font.into());
            let mut wide = crate::wide::unterminated(text);
            let mut r = RECT {
                left: 0,
                top: 0,
                right: width.max(1),
                bottom: 0,
            };
            // The same flags `draw_wrapped` draws with, plus `DT_CALCRECT`.
            // Any difference between the two sets would be a difference
            // between the height measured and the height drawn.
            DrawTextW(
                self.canvas.raw(),
                &mut wide,
                &mut r,
                DT_CALCRECT | DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
            );
            r.bottom - r.top
        }
    }
}

/// Runs `f` with text metrics taken from the interface fonts at `dpi`.
///
/// The scratch context exists only to hold a font: measuring needs a device
/// context, and creating one per measurement inside a layout pass would be a
/// GDI allocation per string. Created, used and destroyed here so no caller
/// has to remember to restore the selection or delete the context.
///
/// Safe, and that follows from the same ownership: the context is created here
/// and released here, so there is nothing a caller could pass that would make
/// the call unsound. It was `unsafe` only because its body called Win32.
pub(super) fn with_metrics<T>(script: Script, f: impl FnOnce(&mut dyn TextMetrics) -> T) -> T {
    // A context in the display's format, asked for as such. It used to be
    // borrowed from the desktop window and then made compatible with, which is
    // two calls and a guard to say what `for_screen` now says in one.
    let scratch = MemDc::for_screen();
    let (font, bold) = fonts(script);
    let Some(scratch) = scratch else {
        // No context to measure in. `TextMetrics` answers in pixels and has no
        // way to say "unknown", so the caller is given zeroes — which sizes a
        // window to its minimum rather than to its content, and is the only
        // answer available. Reached on a session with no display at all.
        return f(&mut ZeroMetrics);
    };
    let canvas = scratch.canvas();
    let _selected = Selection::shared(canvas, font.into());
    f(&mut GdiMetrics {
        script,
        canvas,
        font,
        bold,
    })
}

/// The measurements of a program that cannot reach a device context.
///
/// Every answer is zero, which sizes a window to its declared minimum. Not a
/// fallback anybody wants, and not one anybody can improve on: text has no
/// width until something has a font to measure it with. It exists so
/// [`with_metrics`] has an answer other than a handle that is not valid, which
/// is what the code before it went on to use.
struct ZeroMetrics;

impl TextMetrics for ZeroMetrics {
    fn line_width(&mut self, _text: &str, _bold: bool) -> i32 {
        0
    }

    fn option_width(&mut self, _item: &DropdownItem) -> i32 {
        0
    }

    fn wrapped_height(&mut self, _text: &str, _width: i32) -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{caret_offsets_from_widths, script_runs};

    /// One offset per `char`, taken from the UTF-16 unit that ends it.
    ///
    /// The two walks are the same length only while every character is one
    /// unit. `😀` is two, so the character after it must read entry 2 of the
    /// width array and not entry 1 — the surrogate's own boundary, which is
    /// not a position the caret ever sits at.
    #[test]
    fn a_caret_offset_is_taken_at_the_last_unit_of_its_char() {
        // Widths after unit 0, 1, 2, 3 of "a😀b": a=10, high surrogate=10,
        // low surrogate=30, b=40.
        let widths = [10, 10, 30, 40];
        assert_eq!(caret_offsets_from_widths("a😀b", &widths), vec![10, 30, 40]);
    }

    /// A width array shorter than the text yields a short caret, not a panic.
    ///
    /// GDI's array is bounded by what it measured and the count comes from
    /// `chars()`; the two are different measurements of one string, and this
    /// is the case where they disagree. Subscripting here would end the
    /// process, which in a release build with `panic = "abort"` is the whole
    /// utility. The value asserted is the fallback, not merely its survival.
    #[test]
    fn a_caret_past_the_measured_units_falls_back() {
        assert_eq!(caret_offsets_from_widths("abc", &[7, 14]), vec![7, 14, 0]);
        assert_eq!(caret_offsets_from_widths("😀", &[5]), vec![0]);
        assert!(caret_offsets_from_widths("", &[1, 2]).is_empty());
    }

    /// Latin text is one run and never touches the language font.
    ///
    /// This is the guarantee that fixes the reported "distorted font": digits,
    /// units and model codes are set in the interface font in every language,
    /// so `220 V` and `CP1350EPFCLCD` look the same on Chinese as on English.
    #[test]
    fn latin_text_stays_in_one_run() {
        for text in ["220 V", "CP1350EPFCLCD", "1350 VA / 810 W", "207-253 V"] {
            let runs = script_runs(text);
            assert_eq!(runs.len(), 1, "{text:?} should not be split");
            assert!(!runs[0].1, "{text:?} must not ask for the script font");
        }
    }

    /// Cyrillic and Greek are Latin-font territory too: Segoe UI covers them,
    /// so a Russian or Greek interface never leaves it.
    #[test]
    fn cyrillic_and_greek_stay_in_the_interface_font() {
        for text in ["Напряжение входа", "Ελληνικά", "Čeština"] {
            let runs = script_runs(text);
            assert_eq!(runs.len(), 1, "{text:?} should not be split");
            assert!(!runs[0].1, "{text:?} must not ask for the script font");
        }
    }

    /// Mixed text splits at the script boundary, and only the foreign part
    /// asks for the language font.
    #[test]
    fn mixed_text_splits_at_the_script_boundary() {
        let runs = script_runs("负载 (W)");
        assert!(runs.len() >= 2, "mixed text must split");
        assert!(runs[0].1, "the CJK head takes the script font");
        assert!(
            runs.iter().any(|(r, foreign)| !foreign && r.contains('W')),
            "the Latin tail stays in the interface font"
        );
        // Reassembling the runs must give back exactly the original.
        let joined: String = runs.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(joined, "负载 (W)");
    }

    /// Splitting never loses or reorders characters, whatever the input.
    #[test]
    fn runs_reassemble_to_the_original() {
        for text in [
            "",
            "AVR 启用",
            "已充满",
            "1350 VA / 810 W",
            "UPS 故障",
            "日本語 66 分",
        ] {
            let joined: String = script_runs(text).iter().map(|(r, _)| r.as_str()).collect();
            assert_eq!(joined, text, "runs must reassemble to {text:?}");
        }
    }
}
