//! Helpers shared by the structural tests — the ones that assert a property
//! about the *source* of a function (an ordering, a field being read) rather
//! than about its result.
//!
//! A few functions cannot be exercised directly in a unit test: `App` owns a
//! poll thread and a tray icon, `Ups::poll` needs a real HID handle. For those
//! the property under test is visible in the source — "logging comes before
//! the notification gate", "poll reads no configuration field" — and the test
//! reads the source to check it.
//!
//! Extracting a function body by searching for the *next doc comment* was the
//! previous approach and was fragile: the marker `"\n    /// "` misses whenever
//! the item after the function opens with a plain `//` comment or no comment at
//! all, so the "body" then ran to the end of the file and swept in unrelated
//! code. [`fn_body`] instead matches braces from the signature, so it ends
//! exactly where the function ends regardless of what follows.

/// The body of the first function whose signature contains `signature`, from
/// the opening `{` to its matching `}` inclusive.
///
/// `signature` need only be a distinctive substring of the `fn` line (e.g.
/// `"fn emit_events("`). Panics if the signature is not found or its braces do
/// not balance, both of which mean the test is pointed at the wrong place.
///
/// Braces inside comments and literals are skipped, and that is not tidiness.
/// The matcher used to count every `{` and `}` it saw, with a note saying that
/// a literal brace would "fail loudly here rather than silently mis-slice".
/// Half of that was true. An unbalanced `{` does raise the depth and end in the
/// panic below; an unbalanced `}` drops the depth to zero early and returns a
/// body that stops short, with nothing to say so.
///
/// The tests reading these bodies mostly assert that something is *absent* —
/// `!body.contains("U_TEST")` is the one guarding the invariant that writing
/// the Test usage starts a discharge which does not stop. A body cut short is
/// exactly the input that makes such an assertion pass without checking
/// anything. Measured rather than argued: a `'}'` placed in `Ups::set_beeper`
/// after its `set_feature` call, with `U_TEST` named on the next line, left
/// `hid::tests::test_feature_is_never_written` green.
pub(crate) fn fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("signature not found: {signature}"));

    // Byte indices throughout. Every delimiter this looks for is ASCII, and a
    // multi-byte UTF-8 sequence never contains an ASCII byte, so scanning
    // bytes cannot mistake part of a character for a delimiter — and every
    // index it stops at is a character boundary, which is what makes the
    // slicing below valid.
    let bytes = src.as_bytes();
    let mut i = start;
    let mut open = None;
    let mut depth = 0usize;

    while i < bytes.len() {
        if let Some(past) = skip_non_code(bytes, i) {
            i = past;
            continue;
        }
        match bytes[i] {
            b'{' => {
                depth += 1;
                if open.is_none() {
                    open = Some(i);
                }
            }
            b'}' => {
                let body_start =
                    open.unwrap_or_else(|| panic!("closing brace before any opening: {signature}"));
                depth -= 1;
                if depth == 0 {
                    return &src[body_start..=i];
                }
            }
            _ => {}
        }
        i += 1;
    }

    assert!(open.is_some(), "no opening brace after: {signature}");
    panic!("unbalanced braces after: {signature}");
}

/// The index just past the comment or literal beginning at `i`, or `None` when
/// what begins there is ordinary code.
///
/// Covers everything Rust allows to contain a brace that is not a brace: line
/// and block comments, strings raw and cooked, and character literals — the
/// last including `'{'` and `'}'` themselves and the `\u{...}` escape, whose
/// braces are the reason it cannot simply be skipped two bytes at a time.
fn skip_non_code(bytes: &[u8], i: usize) -> Option<usize> {
    let at = |k: usize| bytes.get(k).copied();

    match (at(i), at(i + 1)) {
        (Some(b'/'), Some(b'/')) => {
            let end = bytes[i..].iter().position(|&b| b == b'\n');
            return Some(end.map_or(bytes.len(), |n| i + n));
        }
        (Some(b'/'), Some(b'*')) => {
            // Rust nests block comments, so a `/*` inside one does not end at
            // the first `*/`.
            let mut k = i + 2;
            let mut depth = 1usize;
            while k < bytes.len() {
                match (at(k), at(k + 1)) {
                    (Some(b'/'), Some(b'*')) => {
                        depth += 1;
                        k += 2;
                    }
                    (Some(b'*'), Some(b'/')) => {
                        depth -= 1;
                        k += 2;
                        if depth == 0 {
                            return Some(k);
                        }
                    }
                    _ => k += 1,
                }
            }
            return Some(bytes.len());
        }
        _ => {}
    }

    if at(i) == Some(b'r') {
        let mut k = i + 1;
        while at(k) == Some(b'#') {
            k += 1;
        }
        if at(k) == Some(b'"') {
            let hashes = k - (i + 1);
            let mut j = k + 1;
            while j < bytes.len() {
                if bytes[j] == b'"' && bytes[j + 1..].iter().take(hashes).all(|&b| b == b'#') {
                    return Some(j + 1 + hashes);
                }
                j += 1;
            }
            return Some(bytes.len());
        }
    }

    if at(i) == Some(b'"') {
        let mut j = i + 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => return Some(j + 1),
                _ => j += 1,
            }
        }
        return Some(bytes.len());
    }

    if at(i) == Some(b'\'') {
        return char_literal_end(bytes, i);
    }

    None
}

/// The index just past a character literal opening at `i`, or `None` when the
/// quote opens a lifetime instead.
///
/// The two are told apart by looking for the closing quote where a literal
/// would have to put it, which is the only thing that distinguishes `'a'` from
/// the `'a` in `&'a str`.
fn char_literal_end(bytes: &[u8], i: usize) -> Option<usize> {
    let at = |k: usize| bytes.get(k).copied();

    let content_end = if at(i + 1) == Some(b'\\') {
        if at(i + 2) == Some(b'u') {
            // `\u{1F600}` — braces inside an escape, which is the whole reason
            // this case is spelled out.
            let close = bytes[i + 3..].iter().position(|&b| b == b'}')?;
            i + 3 + close + 1
        } else {
            i + 3
        }
    } else {
        // One whole character, whose width the UTF-8 lead byte states.
        let lead = at(i + 1)?;
        let width = match lead {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        };
        i + 1 + width
    };

    (at(content_end) == Some(b'\'')).then_some(content_end + 1)
}

/// A [`HotspotId`](crate::ui::row::HotspotId) from the number a test writes it
/// as.
///
/// The ids in test fixtures are authored numbers — `field(1)`, `buttons(4, 5)`,
/// a ring expected to be `[1, 2, 3]` — and spelling `HotspotId::new` at each of
/// the hundred-odd places they appear would bury the assertion in the
/// conversion. One name, defined here beside the other fixtures, rather than
/// the same three-line helper copied into seven test modules.
pub(crate) fn hot(n: u32) -> crate::ui::row::HotspotId {
    crate::ui::row::HotspotId::new(n)
}

/// A [`TextMetrics`](crate::ui::text::TextMetrics) stand-in for the
/// layout tests.
///
/// Deliberately not GDI. What these tests are about is the arithmetic around
/// the measurement — which rows exist, how their heights sum, where the columns
/// begin, whether the painter and the sizing pass reach the same number — and
/// that holds for any consistent set of measurements. The measurement itself is
/// one call into the platform, made from one place, by both readers.
///
/// Proportional to the character count, because that is the only property the
/// arithmetic depends on: a longer string is wider, and a paragraph that does
/// not fit wraps into more lines. Seven pixels per character is what the
/// closures these replace all used.
///
/// Every answer it gives has to be *distinguishable*, and that is a property of
/// the stub rather than a nicety: a measurer that returns the same number for
/// two different questions turns a test into a check that some number came
/// back. That is what let the weight argument go unexercised — `line_width`
/// ignored `bold`, so a caller charging a heading in the regular face measured
/// exactly as one charging it in the bold face, and no assertion anywhere could
/// tell the two apart.
pub(crate) struct StubMetrics {
    pub char_width: i32,
    /// What each character costs on top of its base width when set bold.
    ///
    /// One orthogonal knob rather than a second full width table: bold is a
    /// surcharge in a real font too, and expressing it that way keeps it
    /// independent of whether the glyph is a wide one.
    pub bold_extra: i32,
    pub line_height: i32,
    /// Width charged to a glyph outside the Latin/Cyrillic/Greek range.
    pub wide_char_width: i32,
}

impl StubMetrics {
    /// A stand-in that charges East Asian glyphs a full em, which is the shape
    /// of the real font's answer and what the per-language width tests are
    /// about.
    pub(crate) fn wide_aware() -> Self {
        Self {
            wide_char_width: 14,
            ..Self::default()
        }
    }
}

impl Default for StubMetrics {
    fn default() -> Self {
        Self {
            char_width: 7,
            bold_extra: 2,
            line_height: 21,
            wide_char_width: 7,
        }
    }
}

impl crate::ui::text::TextMetrics for StubMetrics {
    fn line_width(&mut self, text: &str, bold: bool) -> i32 {
        let extra = if bold { self.bold_extra } else { 0 };
        text.chars()
            .map(|c| {
                extra
                    + if c as u32 > 0x2E80 {
                        self.wide_char_width
                    } else {
                        self.char_width
                    }
            })
            .sum()
    }

    /// Charged per piece, exactly as the painter draws it.
    ///
    /// The stub has no fonts to select, so what it can still reproduce — and
    /// what the layout arithmetic actually depends on — is that the option is
    /// measured from the same pieces it is drawn from, native name included.
    fn option_width(&mut self, item: &crate::ui::row::DropdownItem) -> i32 {
        let label = self.line_width(&item.label, false);
        if item.native.is_empty() {
            return label;
        }
        label + self.line_width(&item.native, false) + self.line_width(" ()", false)
    }

    fn wrapped_height(&mut self, text: &str, width: i32) -> i32 {
        if text.is_empty() {
            return 0;
        }
        // Widths, so unsigned: `line_width` sums per-character widths and the
        // wrap width is floored at 1. Unsigned is also where `div_ceil` lives
        // — the signed form is still gated behind an unstable feature — and
        // the hand-rolled `(a + b - 1) / b` it replaces is exactly what
        // `clippy::manual_div_ceil` is about. The crate uses `div_ceil`
        // elsewhere; this was the one place still spelling it out.
        let total = self.line_width(text, false).max(0) as u32;
        let per_line = width.max(1) as u32;
        total.div_ceil(per_line) as i32 * self.line_height
    }
}

/// The device this project was written against, reporting everything it
/// reports, with nothing wrong.
///
/// One copy, here, because there were fourteen: `panel.rs` and `layout.rs` each
/// carried their own, and a third was written out inline inside
/// `skeleton_matches_the_populated_height`. Fourteen spellings of "the same
/// UPS" are fourteen things that can drift apart, and one pair already had —
/// two of them disagreed about `charging` with nothing anywhere saying whether
/// that was deliberate.
///
/// The values are the ones a real CP1350EPFCLCD returned: 224 V in and out,
/// 27.1 V on a 24 V battery, 11 % load. Round numbers would have hidden the
/// scaling bug this fixture was first written to pin.
pub(crate) fn live_reading() -> crate::hid::Reading {
    crate::hid::Reading {
        input_voltage: Some(224.0),
        output_voltage: Some(224.0),
        battery_voltage: Some(27.1),
        battery_nominal_voltage: Some(24.0),
        input_nominal_voltage: Some(230.0),
        load_percent: Some(11),
        load_watts: Some(97),
        load_va: Some(146),
        charge_percent: Some(100),
        runtime_seconds: Some(3600),
        test_result: Some(crate::hid::TestResult::NotRun),
        beeper: Some(crate::hid::Beeper::Enabled),
        low_transfer_voltage: Some(207.0),
        high_transfer_voltage: Some(253.0),
        capacity_limit_percent: Some(5),
        warning_capacity_percent: Some(10),
        runtime_limit_seconds: Some(300),
        nominal_va: Some(1350),
        nominal_power_w: Some(810),
        ac_present: Some(true),
        charging: Some(false),
        fully_charged: Some(true),
        ..Default::default()
    }
}

/// The nameplate of the same device.
pub(crate) fn live_identity() -> crate::hid::Identity {
    crate::hid::Identity {
        model: Some("CP1350EPFCLCD".into()),
        serial: Some("CY2RP2000118".into()),
        manufacturer: Some("CPS".into()),
        chemistry: Some("PbAcid".into()),
        firmware: Some("CR02205DCT14".into()),
    }
}

/// A panel that has never heard from a device: nothing read, nothing probed,
/// no complaints.
///
/// The baseline for struct-update syntax, so a test writes only the fields it
/// is actually about:
///
/// ```ignore
/// let data = PanelData { presence: Presence::Open, ..panel_data() };
/// ```
///
/// A free function rather than a `Default` impl on `PanelData` itself. The
/// struct is built for real in `main.rs`, and it is built there by listing
/// every field — which is what makes adding a field a compile error at the one
/// site that has to decide about it. A `Default` impl would take that away from
/// production code in order to shorten tests.
pub(crate) fn panel_data<'a>() -> crate::ui::panel::PanelData<'a> {
    crate::ui::panel::PanelData {
        reading: None,
        identity: None,
        presence: crate::app::Presence::Unprobed,
        beeper: crate::app::BeeperView::Unknown,
        warnings: Vec::new(),
        probing: false,
        self_test_running: false,
    }
}

/// A panel showing a connected, fully reported device with its buzzer on.
///
/// The shape twelve tests in `panel.rs` needed and wrote out in full. Like
/// [`panel_data`], it is meant to be narrowed with struct update:
///
/// ```ignore
/// let data = PanelData { self_test_running: true, ..live_panel(&r, &id) };
/// ```
pub(crate) fn live_panel<'a>(
    reading: &'a crate::hid::Reading,
    identity: &'a crate::hid::Identity,
) -> crate::ui::panel::PanelData<'a> {
    crate::ui::panel::PanelData {
        reading: Some(reading),
        identity: Some(identity),
        presence: crate::app::Presence::Open,
        // The mode the reading carries, and current because that reading is
        // the latest observation. The two travel together in the running
        // utility — `App` folds every reading's mode into the view — so a
        // fixture that set only one of them would be a state the program
        // cannot produce.
        beeper: reading.beeper.map_or(
            crate::app::BeeperView::Unknown,
            crate::app::BeeperView::Current,
        ),
        ..panel_data()
    }
}

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Serialises the tests that drive the process-wide log.
///
/// [`crate::evlog`] keeps one `Log` for the process — deliberately, because
/// every thread in the utility logs to the same file — so its debug switch,
/// its throttle map and its output file are shared state that parallel tests
/// would otherwise interleave. Any test that turns the debug level on, reads
/// the log file, or asserts on the throttle map holds this first.
///
/// It lives here rather than in `evlog`'s own test module because more than
/// one module's tests reach that singleton: the self-test trace is written
/// through `evlog` from `hid::selftest`, and two locks would serialise two
/// disjoint sets of tests against the same state, which is no lock at all.
///
/// A previous panic is tolerated: a poisoned mutex here means another test
/// failed, and reporting that as a second failure only obscures the first.
pub(crate) fn serial_log() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A directory of our own under the system temporary directory, removed
/// when the guard drops.
///
/// The name carries the process id and a counter, so two tests running in
/// parallel — and two runs of the suite on the same machine — never share a
/// file. `Drop` rather than a cleanup call at the end of each test: a
/// failing assertion unwinds past the cleanup call and leaves the directory
/// behind, which is how a temporary file becomes permanent.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ups-monitor-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the system temp directory must be writable");
        Self(dir)
    }

    /// The directory itself, for a test to name a file inside it.
    ///
    /// It used to hand back the INI path directly, which was one caller's
    /// business rather than this guard's: the log tests want a `.log` in the
    /// same kind of directory, and a second accessor per file type would have
    /// made a general RAII guard into a list of the files that happen to use
    /// it.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{char_literal_end, fn_body};

    /// A lifetime is not an unterminated character literal.
    ///
    /// `&'a str` opens a quote that never closes, and treating it as a literal
    /// swallows the rest of the file.
    #[test]
    fn lifetimes_are_not_character_literals() {
        let src = "fn a<'x>(v: &'x str) -> &'x str { v }";
        assert_eq!(fn_body(src, "fn a<"), "{ v }");
    }

    #[test]
    fn extracts_between_matching_braces() {
        let src = "fn a() { let x = 1; } fn b() { let y = 2; }";
        assert_eq!(fn_body(src, "fn a("), "{ let x = 1; }");
        assert_eq!(fn_body(src, "fn b("), "{ let y = 2; }");
    }

    #[test]
    fn handles_nested_braces() {
        let src = "fn a() { if c { g(); } h(); }\n// trailing comment\n";
        assert_eq!(fn_body(src, "fn a("), "{ if c { g(); } h(); }");
    }

    /// A brace in a literal or a comment is text, not structure.
    ///
    /// One case per thing Rust lets a brace hide in. The closing brace is the
    /// dangerous direction: it ends the body early and returns something that
    /// still looks like a body, so a test asserting an absence over it passes
    /// without having read the part that was cut off.
    #[test]
    fn braces_that_are_not_structure_are_skipped() {
        for src in [
            r#"fn a() { let s = "}"; g(); }"#,
            r#"fn a() { let s = "{"; g(); }"#,
            "fn a() { let c = '}'; g(); }",
            "fn a() { let c = '{'; g(); }",
            "fn a() { let c = '\\u{7D}'; g(); }",
            "fn a() { // }\n g(); }",
            "fn a() { /* } */ g(); }",
            "fn a() { /* /* } */ */ g(); }",
        ] {
            let body = fn_body(src, "fn a(");
            assert!(body.ends_with("g(); }"), "cut short: {body:?}");
            assert_eq!(body, &src[src.find('{').unwrap()..], "{src:?}");
        }
    }

    /// A raw string ends at its own delimiter, not at the first quote inside.
    ///
    /// The content carries an odd number of quotes, which is what makes this
    /// discriminating: parsed as an ordinary string, the quotes pair up
    /// differently and the `}` between them lands outside any literal.
    #[test]
    fn raw_strings_are_skipped_whole() {
        let src = r##"fn a() { let s = r#"x"y}"#; g(); }"##;
        assert_eq!(fn_body(src, "fn a("), &src[src.find('{').unwrap()..]);
    }

    /// A block comment inside a block comment does not end at the inner `*/`.
    ///
    /// The brace sits after the inner close, so a matcher that stops there
    /// treats it as code and ends the body early.
    #[test]
    fn block_comments_nest() {
        let src = "fn a() { /* /* x */ } */ g(); }";
        assert_eq!(fn_body(src, "fn a("), &src[src.find('{').unwrap()..]);
    }

    /// Where each kind of character literal ends.
    ///
    /// Checked on the helper rather than through an extracted body, because at
    /// body level these cases are invisible: the braces of a `\u{...}` escape
    /// always come in a pair, so miscounting them moves the depth up and back
    /// down and the body still ends in the right place. That makes the escape
    /// harmless *and* untestable from outside — which is a reason to pin the
    /// contract here, not a reason to leave the case unhandled and rely on the
    /// braces staying balanced.
    #[test]
    fn character_literals_end_where_they_end() {
        assert_eq!(char_literal_end(b"'x'", 0), Some(3));
        assert_eq!(char_literal_end(b"'{'", 0), Some(3));
        assert_eq!(char_literal_end(b"'\\''", 0), Some(4));
        assert_eq!(char_literal_end(b"'\\n'", 0), Some(4));
        assert_eq!(char_literal_end("'—'".as_bytes(), 0), Some(5));
        assert_eq!(char_literal_end(b"'\\u{7D}'", 0), Some(8));
        // A lifetime, which opens a quote it never closes.
        assert_eq!(char_literal_end(b"'a str", 0), None);
    }

    /// The matcher survives being pointed at itself.
    ///
    /// It parses braces and so its own body is full of them in literals; the
    /// brace counter it replaced could not balance this at all, which is how
    /// the defect was first noticed.
    #[test]
    fn extracts_its_own_body() {
        let body = fn_body(include_str!("testsupport.rs"), "pub(crate) fn fn_body<");
        assert!(body.starts_with('{') && body.ends_with('}'));
        assert!(body.contains("unbalanced braces after"));
        assert!(
            !body.contains("fn skip_non_code"),
            "body ran past the end of the function"
        );
    }

    #[test]
    #[should_panic(expected = "signature not found")]
    fn missing_signature_panics() {
        fn_body("fn a() {}", "fn z(");
    }
}
