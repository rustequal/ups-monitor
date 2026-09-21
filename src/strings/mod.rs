//! Every user-visible string, in every language the utility ships.
//!
//! Compiled into the binary rather than read from `lang/*.ini`. Three
//! things follow from that and none of them are incidental:
//!
//! - The utility depends on no external file to render its own interface.
//!   A missing or truncated language file used to mean a window drawn in
//!   raw key names; that failure no longer has a way to happen.
//! - A language is complete or it does not compile. `strings` is a fixed
//!   array, so a forgotten translation is a length mismatch at build time
//!   rather than a blank label found by a user.
//! - Nothing is parsed or allocated at startup. The table lives in
//!   `.rodata` and lookup is an index into it.
//!
//! Log lines are deliberately **not** here. They stay English in the code
//! that emits them, for a reason of their own: the log is read by whoever is
//! diagnosing a problem, often not the owner of the machine and considerably
//! later.
//!
//! # Layout
//!
//! Split into two files, because the two things this module holds are of
//! entirely different kinds. [`keys`] is the *schema*: one enum, one name
//! table, a few hundred lines that are read and edited whenever a string is
//! added. [`languages`] is the *data*: twenty-four rows of a hundred and
//! forty-one translations, some three and a half thousand lines that nobody
//! reads top to bottom and that regenerate wholesale when a language is added.
//! Holding both in one file made every visit to the schema a scroll past the
//! data, and made a diff touching either look like a diff touching the module.
//!
//! The two constants below stay here rather than in either half: both files
//! need `KEY_COUNT` (it is the length of the name table *and* of every
//! language's string array), and putting it in one of them would make that
//! file the other's dependency for no reason beyond where it was typed.
//!
//! Everything is re-exported, so the paths callers use — `strings::Key`,
//! `strings::LANGUAGES`, `strings::KEY_COUNT` — are exactly what they were.

/// Number of translatable strings. Every language supplies exactly this
/// many, in the order of `KEYS`.
pub(crate) const KEY_COUNT: usize = 137;

/// Number of shipped languages, and the declared length of [`LANGUAGES`].
///
/// Named for the same reason `KEY_COUNT` is: the array literal must hold
/// exactly this many entries or it does not compile, and having the number
/// available as a `const` lets other modules reason about it — `static`s
/// cannot be read from const context.
pub(crate) const LANGUAGE_COUNT: usize = 24;

mod keys;
mod languages;

/// Dotted key names, test-only like the table itself.
#[cfg(test)]
pub(crate) use keys::KEYS;
pub(crate) use keys::{Key, Strings};
pub(crate) use languages::{Language, LANGUAGES};
