//! A value resolved from hand-edited configuration, carrying whether the
//! requested code was recognised.
//!
//! Both the language and the theme are selected by a string code in the INI,
//! and both fall back to a default when the code names nothing. The caller
//! needs two things back: the value to use, and whether a fallback happened,
//! so it can log that the setting was ignored. That pair was returned as a
//! bare `(T, bool)` from each loader — readable at the call site, but the
//! `bool` meant nothing on its own and the two loaders stated the same idea
//! in two shapes. `Resolved<T>` gives the idea one name and one shape.

/// A configuration value together with whether its requested code was known.
///
/// `recognised` is `false` when the code was not found and `value` is the
/// default. [`Resolved::found`] is a distinct constructor from
/// [`Resolved::fell_back`] so a loader cannot forget to set the flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Resolved<T> {
    /// The value to use: either the one the code named, or the default.
    pub value: T,
    /// Whether the requested code was recognised. `false` means `value` is a
    /// fallback default and the caller should log that the setting was ignored.
    pub recognised: bool,
}

impl<T> Resolved<T> {
    /// The code was recognised; `value` is what it named.
    pub(crate) fn found(value: T) -> Self {
        Self {
            value,
            recognised: true,
        }
    }

    /// The code was not recognised; `value` is the fallback default.
    pub(crate) fn fell_back(value: T) -> Self {
        Self {
            value,
            recognised: false,
        }
    }

    /// Runs `on_fallback` when the code was not recognised, then yields the
    /// value. The one place a caller both logs the fallback and takes the
    /// value, so the two cannot drift apart.
    pub(crate) fn or_log(self, on_fallback: impl FnOnce()) -> T {
        if !self.recognised {
            on_fallback();
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value comes back either way; the line is written only on a
    /// fallback.
    ///
    /// Both halves are silent when wrong. Never running `on_fallback` leaves
    /// a hand-edited INI naming a language that does not exist working in
    /// English with nothing in the log to say the setting was ignored —
    /// which reads as the setting not being saved. Running it always fills
    /// the log with a complaint about a setting that was honoured, and
    /// teaches the reader to skip that line for the one time it is true.
    #[test]
    fn only_a_fallback_is_announced() {
        let mut announced = 0;

        let kept = Resolved::found("ru").or_log(|| announced += 1);
        assert_eq!(kept, "ru", "a recognised code yields what it named");
        assert_eq!(announced, 0, "a setting that was honoured is not news");

        let default = Resolved::fell_back("en").or_log(|| announced += 1);
        assert_eq!(default, "en", "a fallback yields the default");
        assert_eq!(announced, 1, "and says so exactly once");
    }
}
