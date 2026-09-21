//! Localization over the string table compiled into the binary.
//!
//! There is no file to load and no fallback chain to walk: every language
//! carries a complete `[&str; KEY_COUNT]`, so a lookup is an index and cannot
//! miss. What used to be "key absent from this locale, fall back to English"
//! is now a compile error in `strings.rs`.

use crate::strings::{self, Language};

/// The active language: an index into [`strings::LANGUAGES`].
///
/// Deliberately not a `&'static Language`. The index is `Copy`, is the same
/// thing the settings list is keyed by, and keeps `Locale` at word size, so
/// passing it around costs nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Locale {
    lang: usize,
}

impl Default for Locale {
    fn default() -> Self {
        Self::english()
    }
}

impl Locale {
    /// English, which is index 0 by the test in `strings`.
    pub(crate) fn english() -> Self {
        Self { lang: 0 }
    }

    /// The locale for a language code, and whether that code was recognised.
    ///
    /// An unknown code is ordinary input rather than corruption: the INI is
    /// hand-edited, and a typo there should leave the utility running in a
    /// language somebody can read. The flag lets the caller log the fallback
    /// instead of failing silently.
    pub(crate) fn by_code(code: &str) -> crate::resolved::Resolved<Self> {
        use crate::resolved::Resolved;
        match strings::LANGUAGES.iter().position(|l| l.code == code) {
            Some(lang) => Resolved::found(Self { lang }),
            None => Resolved::fell_back(Self::english()),
        }
    }

    /// The language this locale renders.
    ///
    /// # Why this indexes
    ///
    /// `lang` is not a number this type accepts; it is one this type produced.
    /// The only constructors are [`Self::by_code`], which takes it from
    /// `position` over the very table indexed here, and [`Self::english`],
    /// which is zero — and the field is private, so no third source exists.
    /// The index is therefore in range by construction, and `get` here would
    /// be a fallback for a state the type cannot be in, plus a decision about
    /// what to return from it. Both would be fiction. This is one of the two
    /// places in the tree where a subscript is the honest spelling; the other
    /// is [`Self::t`] below.
    pub(crate) fn language(self) -> &'static Language {
        &strings::LANGUAGES[self.lang]
    }

    /// The font this language needs, or `None` for the Segoe UI default.
    pub(crate) fn font(self) -> Option<&'static str> {
        self.language().font
    }

    /// Resolve a [`strings::Key`] to its text in this locale.
    ///
    /// The key is an enum whose discriminant is the index into the language's
    /// `strings` array, so this is a direct index with no search and no way to
    /// miss: a key that does not exist is a compile error at the call site, not
    /// a marker string found by eye in the window.
    ///
    /// That is also why it indexes rather than reaching for `get`. Both the
    /// enum and every language's array are generated from one list by one
    /// macro, and a language whose array were short would fail to compile —
    /// the arrays are `[&str; KEY_COUNT]`. There is no runtime state in which
    /// this can miss, so a fallback would be a string chosen for a case that
    /// cannot arise. See [`Self::language`].
    pub(crate) fn t(self, key: strings::Key) -> &'static str {
        self.language().strings[key as usize]
    }

    /// One-argument substitution of `{}` in a localized string.
    pub(crate) fn t1(self, key: strings::Key, arg: &str) -> String {
        self.t(key).replacen("{}", arg, 1)
    }

    /// Every shipped language, in display order.
    pub(crate) fn available() -> &'static [Language] {
        &strings::LANGUAGES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_resolves() {
        let l = Locale::english();
        assert_eq!(l.t(strings::Key::MenuPanel), "Panel");
        assert_eq!(l.t(strings::Key::UnitVolt), "V");
        assert_eq!(l.t(strings::Key::MenuExit), "Exit");
    }

    #[test]
    fn a_known_code_selects_that_language() {
        let r = Locale::by_code("ru");
        assert!(r.recognised);
        assert_eq!(r.value.language().code, "ru");
        // Compared against English rather than against a Russian word
        // spelled out here. The property is "a known code selects that
        // language's own table"; a literal would be a second copy of the
        // translation, in a file that has no reason to carry one and every
        // reason to fall out of step with `languages.rs`.
        assert_ne!(
            r.value.t(strings::Key::MenuExit),
            Locale::by_code("en").value.t(strings::Key::MenuExit),
            "a known code must answer from its own table, not from English"
        );
    }

    /// A hand-edited INI naming a language that does not exist leaves the
    /// utility readable rather than showing raw keys, and says so.
    #[test]
    fn an_unknown_code_falls_back_to_english_and_reports_it() {
        let r = Locale::by_code("xx");
        assert!(!r.recognised, "the caller must be able to log the fallback");
        assert_eq!(r.value.language().code, "en");
    }

    #[test]
    fn substitution_replaces_first_placeholder_only() {
        let l = Locale::english();
        let s = l.t1(strings::Key::SettingsInvalidInterval, "1000");
        assert!(s.contains("1000"));
        assert!(!s.contains("{}"));
    }

    /// Every language answers every key with real text. The compiler enforces
    /// the count; this enforces that the count was met with translations
    /// rather than with placeholders.
    #[test]
    fn every_language_answers_every_key() {
        for lang in Locale::available() {
            let r = Locale::by_code(lang.code);
            assert!(r.recognised);
            let l = r.value;
            for key in strings::Key::ALL {
                let text = l.t(key);
                assert!(!text.is_empty(), "{}: {} blank", lang.code, key.label());
            }
        }
    }

    /// A language whose script Segoe UI cannot draw names its own font, and
    /// one whose script it can draw names none.
    ///
    /// Nothing else in the tree checks this: the panel asks for the font,
    /// falls back to the default when the answer is `None`, and draws
    /// whatever comes out. An answer of `None` for Chinese renders the
    /// interface as boxes; a font name for Russian replaces a face that
    /// covers Cyrillic with one chosen for a different script. Both are
    /// visible only to somebody running that language, which is the one
    /// person least likely to be the author.
    ///
    /// The four are named individually rather than as "some language has a
    /// font", because a table with the right *number* of fonts and the wrong
    /// rows is exactly the failure this is for.
    #[test]
    fn a_language_names_the_font_its_script_needs() {
        let font_of = |code: &str| Locale::by_code(code).value.font();

        assert_eq!(font_of("zh"), Some("Microsoft YaHei UI"));
        assert_eq!(font_of("ja"), Some("Yu Gothic UI"));
        assert_eq!(font_of("ko"), Some("Malgun Gothic"));
        assert_eq!(font_of("hi"), Some("Nirmala UI"));

        // Latin, Cyrillic and Greek are covered by the default, and asking for
        // a substitute would be asking for a worse one.
        for code in ["en", "ru", "el", "de"] {
            assert_eq!(font_of(code), None, "{code} is drawn by the default face");
        }

        // A named font is a real name, not an empty string standing in for
        // one: the empty string reaches `CreateFontIndirectW` as a request for
        // whatever the system picks.
        for lang in Locale::available() {
            if let Some(name) = lang.font {
                assert!(!name.is_empty(), "{} names an empty font", lang.code);
            }
        }
    }

    #[test]
    fn the_table_is_the_declared_size() {
        assert_eq!(strings::KEYS.len(), strings::KEY_COUNT);
        for l in Locale::available() {
            assert_eq!(l.strings.len(), strings::KEY_COUNT);
        }
    }
}
