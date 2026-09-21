//! Standing complaints the panel shows above everything else.
//!
//! Not events. An event happened at a moment and goes to the log; a warning is
//! a condition that is *still true* — the INI cannot be written, the device
//! cannot be opened — and stays on screen until whatever caused it is answered.
//!
//! # Why there are two kinds and not one string
//!
//! There used to be a single `startup_warning: Option<String>`, written from
//! five places and cleared from two. Nothing about the field said which writer
//! a given message came from, so each clear cleared them all:
//!
//! * A successful connect retired "the configuration file cannot be written",
//!   which it knows nothing about. The user then had no idea their settings
//!   were not being saved.
//! * A successful save retired "several matching devices found", which it
//!   knows nothing about either — so opening Settings and pressing OK made a
//!   real ambiguity disappear from the panel while it was still ambiguous.
//!
//! Both are the same defect: one field answering two questions, and the answer
//! to one being taken for the answer to the other. Splitting the field by
//! source is what makes the clears independent, and giving each source its own
//! *type* is what stops a warning from being filed under the wrong one — the
//! slots do not accept each other's values.
//!
//! # Why the cause is stored rather than the sentence
//!
//! Each variant holds what happened, and the text is produced at the moment the
//! panel is built. Storing the rendered sentence meant it was rendered in
//! whatever language was active when the condition arose, so changing the
//! language in Settings left the old wording on screen — indefinitely for the
//! device warnings, which are only rewritten by a reconnect.

use std::path::PathBuf;

use crate::error::ConnectFailure;
use crate::hid::SelfTestNotRun;
use crate::lang::Locale;
use crate::strings::Key;
/// A complaint about the configuration file. Answered by a successful save.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ConfigWarning {
    /// The file could not be written. The panel names the path, because the
    /// usual causes — a read-only directory, an installation under Program
    /// Files, a removed drive — are only diagnosable if the user can see which
    /// file is meant.
    NotWritable(PathBuf),
    /// The save failed for a reason with no localised form. The text is the
    /// operating system's own and is shown as it came.
    SaveFailed(String),
}

/// A complaint about the device. Answered by a successful connect.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceWarning {
    /// More than one matching device is attached and the poller took the
    /// first. Not an error — the utility works — but which of their two UPSes
    /// the user is looking at is a question nobody answered.
    Multiple,
    /// The device is present but could not be opened.
    Unavailable(ConnectFailure),
}

/// The warnings currently standing, at most one per source.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Warnings {
    config: Option<ConfigWarning>,
    device: Option<DeviceWarning>,
    /// A self-test that was asked for and never ran.
    ///
    /// The fourth source, and it exists because the Last test line stopped
    /// being able to hold it. That line reports the device's `Test` register,
    /// which is the only thing that knows about *every* test including the
    /// ones started from the front panel of the UPS — and the register has no
    /// code for "the command was refused", because from the device's side
    /// nothing happened. Put on that line anyway, such an outcome was a
    /// statement about the utility wearing the label of a statement about the
    /// device, and it shadowed the register for the rest of the session.
    ///
    /// Answered by a test running: see [`Self::clear_self_test`].
    self_test: Option<SelfTestNotRun>,
    /// The log turned itself off after repeated write failures, and the path it
    /// was writing to.
    ///
    /// The third source, and the only one with no answer: the log does not try
    /// again once it has given up, so the condition holds for the rest of the
    /// session and there is no `clear_log` to pair with the setter. Without
    /// this the utility went on running with its record silently missing —
    /// and the one place that would have said so was the log itself.
    log: Option<PathBuf>,
}

impl Warnings {
    /// Records a configuration complaint, replacing any earlier one. The most
    /// recent attempt is the one that describes the current state of the file.
    pub(crate) fn set_config(&mut self, warning: ConfigWarning) {
        self.config = Some(warning);
    }

    /// The configuration file was written successfully, so nothing is wrong
    /// with it any more. Says nothing about the device.
    pub(crate) fn clear_config(&mut self) {
        self.config = None;
    }

    /// The log has stopped writing, and to which file. Idempotent: the main
    /// loop asks the log module once per pass, so this is set again on every
    /// pass for as long as the condition holds.
    pub(crate) fn set_log_disabled(&mut self, path: PathBuf) {
        self.log = Some(path);
    }

    pub(crate) fn set_device(&mut self, warning: DeviceWarning) {
        self.device = Some(warning);
    }

    /// The device is open and reporting. Says nothing about the file.
    pub(crate) fn clear_device(&mut self) {
        self.device = None;
    }

    /// A self-test was asked for and did not run. Replaces any earlier one:
    /// the most recent attempt is the one worth explaining.
    ///
    /// Takes [`SelfTestNotRun`] and not the whole outcome, so a verdict cannot
    /// be filed here by mistake: a test that ran is reported by the device's
    /// register, on the Last test line, where it stays current.
    pub(crate) fn set_self_test(&mut self, reason: SelfTestNotRun) {
        self.self_test = Some(reason);
    }

    /// A test is running, so whatever stopped the last one from starting no
    /// longer stands.
    ///
    /// The answer to this source, and it comes from the device rather than
    /// from the utility's own record of what it asked for: a test started from
    /// the front panel of the UPS answers the complaint just as well, because
    /// the complaint was only ever "nothing is testing when you asked for a
    /// test".
    pub(crate) fn clear_self_test(&mut self) {
        self.self_test = None;
    }

    /// The standing warnings as sentences, in a fixed order: the device first,
    /// because it is what the window is about, then the settings file, then the
    /// self-test, then the log.
    ///
    /// Empty when there is nothing to say, which is the ordinary case, and an
    /// empty `Vec` does not allocate.
    pub(crate) fn lines(&self, locale: Locale) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(device) = self.device {
            out.push(match device {
                DeviceWarning::Multiple => locale.t(Key::ErrorMultipleDevices).to_owned(),
                DeviceWarning::Unavailable(failure) => locale.t(failure.lang_key()).to_owned(),
            });
        }
        if let Some(config) = &self.config {
            out.push(match config {
                ConfigWarning::NotWritable(path) => {
                    locale.t1(Key::ErrorConfigNotWritable, &path.to_string_lossy())
                }
                ConfigWarning::SaveFailed(text) => text.clone(),
            });
        }
        if let Some(reason) = self.self_test {
            out.push(locale.t1(Key::ErrorSelfTestNotRun, locale.t(reason.lang_key())));
        }
        if let Some(path) = &self.log {
            out.push(locale.t1(Key::ErrorLogNotWritable, &path.to_string_lossy()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale() -> Locale {
        Locale::english()
    }

    /// A self-test that never ran stands as its own complaint, and is answered
    /// only by a test running.
    ///
    /// The fourth source, and it exists because the Last test line stopped
    /// being able to hold it: that line reports the device's `Test` register,
    /// which has no code for a refused command, because from the device's side
    /// nothing happened. Put there anyway, such an outcome shadowed the
    /// register for the rest of the session, and a test started from the front
    /// panel of the UPS changed nothing on screen.
    ///
    /// The two clears are asserted against each other for the reason the test
    /// below gives for config and device: a device coming back must not retire
    /// a complaint about a refused test.
    #[test]
    fn a_self_test_that_never_ran_is_answered_only_by_one_that_does() {
        let mut w = Warnings::default();
        w.set_self_test(SelfTestNotRun::NotAcknowledged);
        w.set_device(DeviceWarning::Multiple);
        assert_eq!(w.lines(locale()).len(), 2);

        w.clear_device();
        assert_eq!(
            w.lines(locale()).len(),
            1,
            "a device that came back says nothing about a test that was refused"
        );

        w.clear_self_test();
        assert!(w.lines(locale()).is_empty());

        // The reason reaches the line: the wrapper alone would name no cause.
        w.set_self_test(SelfTestNotRun::ChannelError);
        let line = w.lines(locale()).remove(0);
        assert!(
            line.contains(locale().t(SelfTestNotRun::ChannelError.lang_key())),
            "the line must name why: {line}"
        );
    }

    /// The defect this module exists for: neither clear may answer for the
    /// other.
    ///
    /// Both directions, because both happened. A connect used to retire the
    /// unwritable-config complaint, leaving the user with settings that
    /// silently failed to save; a save used to retire the several-devices
    /// complaint, so pressing OK in Settings made a real ambiguity vanish
    /// while it was still ambiguous.
    #[test]
    fn each_source_is_cleared_only_by_its_own_answer() {
        let mut w = Warnings::default();
        w.set_config(ConfigWarning::NotWritable("C:\\ups-monitor.ini".into()));
        w.set_device(DeviceWarning::Multiple);
        assert_eq!(w.lines(locale()).len(), 2);

        w.clear_device();
        let lines = w.lines(locale());
        assert_eq!(
            lines.len(),
            1,
            "a working device says nothing about the configuration file"
        );
        assert!(lines[0].contains("ups-monitor.ini"));

        w.set_device(DeviceWarning::Multiple);
        w.clear_config();
        let lines = w.lines(locale());
        assert_eq!(
            lines.len(),
            1,
            "a saved file says nothing about how many devices are attached"
        );
        assert!(!lines[0].contains("ups-monitor.ini"));
    }

    /// The log warning is its own source, cleared by neither of the others.
    ///
    /// It has no answer of its own either: the log does not resume once it has
    /// given up, so a successful save and a working device must both leave it
    /// standing.
    #[test]
    fn a_dead_log_is_not_answered_by_anything_else() {
        let mut w = Warnings::default();
        w.set_log_disabled("C:\\ups-monitor.log".into());
        w.set_config(ConfigWarning::NotWritable("C:\\ups-monitor.ini".into()));
        w.set_device(DeviceWarning::Multiple);
        assert_eq!(w.lines(locale()).len(), 3);

        w.clear_config();
        w.clear_device();
        let lines = w.lines(locale());
        assert_eq!(lines.len(), 1, "the log complaint has no other answer");
        assert!(lines[0].contains("ups-monitor.log"));
    }

    /// Nothing wrong means nothing shown, and no allocation for the case that
    /// holds on every ordinary run.
    #[test]
    fn no_warnings_is_the_quiet_case() {
        let w = Warnings::default();
        assert!(w.lines(locale()).is_empty());
        assert_eq!(w.lines(locale()).capacity(), 0);
    }

    /// A second complaint from the same source replaces the first: it is the
    /// same condition, re-observed.
    #[test]
    fn a_source_holds_one_warning_at_a_time() {
        let mut w = Warnings::default();
        w.set_device(DeviceWarning::Multiple);
        w.set_device(DeviceWarning::Unavailable(ConnectFailure::Busy));
        assert_eq!(w.lines(locale()).len(), 1);
        assert_eq!(
            w.lines(locale())[0],
            locale().t(ConnectFailure::Busy.lang_key()),
            "the newer observation is the one that describes the device now"
        );
    }

    /// The text follows the interface language rather than the language that
    /// happened to be active when the condition arose.
    ///
    /// Storing the rendered sentence left a warning in the old language after
    /// the user switched — permanently, for a device warning, since only a
    /// reconnect ever rewrote it.
    #[test]
    fn the_wording_follows_the_current_language() {
        let mut w = Warnings::default();
        w.set_device(DeviceWarning::Multiple);

        let english = w.lines(Locale::english());
        let code = Locale::available()
            .iter()
            .map(|l| l.code)
            .find(|c| *c != "en")
            .expect("the build ships more than one language");
        let translated = w.lines(Locale::by_code(code).value);

        assert_ne!(
            english, translated,
            "the same warning must render differently in a different language, \
             or it is not being rendered at display time"
        );
    }
}
