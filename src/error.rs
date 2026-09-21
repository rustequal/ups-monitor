use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("device not found (vid={vid:#06x} pid={pid:#06x})")]
    DeviceNotFound { vid: u16, pid: u16 },

    /// The poll thread was asked to exit part-way through a series of
    /// transfers, and gave up rather than finishing it.
    ///
    /// Not a fault of the device and not reported as one. `connect` and `poll`
    /// each perform between six and ten control transfers, and on a UPS that
    /// has stopped answering every one of them returns on the driver's own
    /// timeout — so a user pressing Exit waited out the whole series, with no
    /// window left on screen to say what was being waited for. The flag is
    /// consulted between transfers, never inside one: a transfer already
    /// issued is allowed to finish, because cancelling one would leave the
    /// device's endpoint in a state this code cannot reason about.
    ///
    /// Its own variant because every other error here answers the question
    /// "what is wrong with the device", and the answer for this one is
    /// "nothing". Folded into `DeviceUnresponsive` it would have logged a
    /// disconnect and greyed the tray icon on the way out of the process.
    #[error("the poll thread was asked to stop")]
    Stopped,

    #[error("failed to enumerate HID interfaces: {0}")]
    Enumeration(#[source] windows::core::Error),

    #[error("HidP call failed: {0}")]
    Parse(&'static str),

    /// A feature transfer failed, with the reason the OS gave.
    ///
    /// The code and message are carried rather than dropped because they are
    /// what separates causes that need different responses: a device that has
    /// stopped answering (`ERROR_GEN_FAILURE`, 0x1F), a buffer this code
    /// sized wrongly (`ERROR_INVALID_PARAMETER`, 0x57), an unplugged cable
    /// (`ERROR_DEVICE_NOT_CONNECTED`, 0x48F). Reported as a bare "read
    /// failed", all three look the same to whoever is reading the log.
    #[error("feature report {rid} read failed: {cause} ({code:#010x})")]
    FeatureRead { rid: u8, code: u32, cause: String },

    /// A feature write failed — the beeper control, the only write the
    /// utility performs. Its own variant rather than reusing `FeatureRead`:
    /// the log verdicts here are precise on purpose, and "read failed" on a
    /// line about a refused *write* sends whoever is diagnosing it to the
    /// wrong operation.
    #[error("feature report {rid} write failed: {cause} ({code:#010x})")]
    FeatureWrite { rid: u8, code: u32, cause: String },

    /// The usage is not in the descriptor at all.
    ///
    /// Separate from `UsageMissing`, which is the descriptor listing the field
    /// while the runtime call refuses it. This one is permanent and a fact
    /// about the firmware — the field will never appear on this device — and
    /// the other is usually a fault in how the report was fetched or sized.
    /// Merging them would report a firmware limitation and a bug in this code
    /// with the same sentence.
    #[error("usage {usage:#04x} on page {page:#04x} is absent from the descriptor")]
    UsageAbsent { page: u16, usage: u16 },

    /// `HidP_GetUsageValue` or `HidP_GetUsages` refused the field.
    ///
    /// Two distinct situations reach this and the `status` tells them apart.
    /// `HIDP_STATUS_USAGE_NOT_FOUND` (0xC0110004) means the usage is not in
    /// the report that was passed — normally that the report id was resolved
    /// from a different report than the one fetched. `HIDP_STATUS_INVALID_
    /// REPORT_LENGTH` (0xC0110003) means the buffer length did not match what
    /// the descriptor declares, which is this code's error and not the
    /// device's. `HIDP_STATUS_INCOMPATIBLE_REPORT_ID` (0xC0110001) means the
    /// field exists but lives in another report entirely.
    #[error("usage {usage:#04x} on page {page:#04x} not resolved in report {rid}: {status:#010x}")]
    UsageMissing {
        page: u16,
        usage: u16,
        rid: u8,
        status: i32,
    },

    /// Every voltage the descriptor *declares* failed to read in one poll, so
    /// the device is treated as gone. Distinct from `FeatureRead`: no single
    /// transfer is being reported here, and inventing a report id and a Win32
    /// code to fit the other variant would put two fabricated numbers in the
    /// log. The individual failures were already reported with their real
    /// causes.
    ///
    /// "Declares" is the load-bearing word. A collection the firmware never
    /// published also reads as `None`, and counting those made a firmware
    /// limitation indistinguishable from a dead device; that case is
    /// `UsageAbsent`, raised once at connect.
    #[error("device unresponsive: no voltage report could be read this poll")]
    DeviceUnresponsive,

    /// The self-test vendor channel could not be opened, written or read.
    ///
    /// One variant for the whole channel rather than one per operation: unlike
    /// the feature-report reads, where the failing operation tells the reader
    /// what to check, a self-test aborts on the first channel failure and
    /// reports a single outcome. The message names which step failed so the
    /// log still distinguishes a refused write from a silent read.
    #[error("self-test channel: {0}")]
    SelfTestChannel(String),

    /// The descriptor does not expose the vendor self-test channel at all —
    /// no gate flag, or no command/reply report. A firmware fact, like
    /// `UsageAbsent`: this device cannot run a vendor self-test.
    #[error("self-test channel is not present on this device")]
    SelfTestUnavailable,

    /// The configuration could not be written.
    ///
    /// The OS reason is carried alongside the path rather than discarded. It
    /// is what separates the causes that call for different answers: a
    /// read-only directory, a full disk, a file another process holds open, a
    /// removable drive pulled out. Reported as a bare "not writable" all four
    /// look the same to whoever is trying to work out why their settings do
    /// not stick. The panel still shows only the localised sentence with the
    /// path; the cause goes to the log.
    #[error("config file {path} is not writable: {cause}")]
    ConfigNotWritable { path: PathBuf, cause: String },
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

/// A connection failure, reduced to what the user can act on.
///
/// The panel needs a localised line, not the raw `Error` display string, which
/// is English and carries Win32 codes and report ids meant for the log. This is
/// the small, translatable classification the UI shows; the full technical
/// detail still goes to the log, unchanged. The mapping is `Error::connect_failure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectFailure {
    /// The device is present but another process holds it, or the OS refused
    /// the open. The user can close the other program and it will reconnect.
    Busy,
    /// The device was found but stopped answering — a transfer failed or the
    /// descriptor could not be read. Usually a cable, a hub, or a device that
    /// needs re-plugging.
    Unresponsive,
    /// The device answered but its reports did not match what the utility
    /// expects — a descriptor or parse failure. Nothing the user can fix; the
    /// log carries the specifics for a bug report.
    Incompatible,
}

impl ConnectFailure {
    /// Localisation key for the panel line.
    pub(crate) fn lang_key(self) -> crate::strings::Key {
        use crate::strings::Key;
        match self {
            Self::Busy => Key::ConnectBusy,
            Self::Unresponsive => Key::ConnectUnresponsive,
            Self::Incompatible => Key::ConnectIncompatible,
        }
    }
}

impl Error {
    /// Classifies a connect-time failure into the user-facing category the
    /// panel shows. `DeviceNotFound` is handled separately (it is the ordinary
    /// disconnected state, not an error), so it maps to `Unresponsive` here only
    /// as a safe default; callers route it before reaching this.
    pub(crate) fn connect_failure(&self) -> ConnectFailure {
        match self {
            // A refused open or a sharing violation is the "busy" case. The
            // Win32 codes are wrapped HRESULT (0x8007xxxx); the low word is the
            // original error: 0x05 ERROR_ACCESS_DENIED, 0x20 ERROR_SHARING_VIOLATION.
            Error::FeatureRead { code, .. } | Error::FeatureWrite { code, .. }
                if matches!(code & 0xFFFF, 0x0005 | 0x0020) =>
            {
                ConnectFailure::Busy
            }
            // A descriptor that parsed wrong, or a field the report did not
            // contain as declared: the device is incompatible with this build.
            Error::Parse(_) | Error::UsageAbsent { .. } | Error::UsageMissing { .. } => {
                ConnectFailure::Incompatible
            }
            // Everything else at connect time — a failed transfer, an
            // unresponsive device, an enumeration failure — is the device not
            // answering as expected.
            _ => ConnectFailure::Unresponsive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_failure(code: u32) -> Error {
        Error::FeatureRead {
            rid: 7,
            code,
            cause: "test".to_owned(),
        }
    }

    /// The three categories the panel can show are told apart by cause, and
    /// each says something different about what the user can do.
    ///
    /// This is the only classification the user ever sees: `Busy` means close
    /// the other program, `Incompatible` means this build does not understand
    /// the device, `Unresponsive` means the cable or the unit. Collapsed onto
    /// one answer the line stops being advice and becomes a shrug — and it
    /// collapses silently, because every branch still produces a sentence in
    /// the panel.
    #[test]
    fn a_connect_failure_is_classified_by_what_the_user_can_do() {
        // A refused open is `Busy` and nothing else: another process holds the
        // device, and it will reconnect once that process lets go.
        assert_eq!(
            read_failure(0x8007_0005).connect_failure(),
            ConnectFailure::Busy
        );
        assert_eq!(
            read_failure(0x8007_0020).connect_failure(),
            ConnectFailure::Busy
        );
        // The guard reads the low word, so a bare Win32 code classifies the
        // same as the HRESULT-wrapped one it arrives as.
        assert_eq!(
            read_failure(0x0000_0005).connect_failure(),
            ConnectFailure::Busy
        );
        // Written failures are the same case seen from the other direction.
        assert_eq!(
            Error::FeatureWrite {
                rid: 7,
                code: 0x8007_0020,
                cause: "test".to_owned(),
            }
            .connect_failure(),
            ConnectFailure::Busy
        );

        // A transfer that failed for any other reason is the device not
        // answering, not a device somebody else is holding: telling the user
        // to close another program when the cable is out sends them after
        // nothing.
        assert_eq!(
            read_failure(0x8007_001F).connect_failure(),
            ConnectFailure::Unresponsive
        );
        assert_eq!(
            Error::DeviceUnresponsive.connect_failure(),
            ConnectFailure::Unresponsive
        );

        // A descriptor this build cannot make sense of is not something the
        // user can fix by unplugging anything.
        assert_eq!(
            Error::Parse("HidP_GetCaps").connect_failure(),
            ConnectFailure::Incompatible
        );
        assert_eq!(
            Error::UsageAbsent {
                page: 0x84,
                usage: 0x30
            }
            .connect_failure(),
            ConnectFailure::Incompatible
        );
        assert_eq!(
            Error::UsageMissing {
                page: 0x84,
                usage: 0x30,
                rid: 7,
                status: -1_072_889_852,
            }
            .connect_failure(),
            ConnectFailure::Incompatible
        );
    }

    /// Each category carries its own line, so the panel cannot show one
    /// diagnosis while the log holds another.
    #[test]
    fn each_category_has_its_own_wording() {
        use crate::strings::Key;
        let keys = [
            ConnectFailure::Busy.lang_key(),
            ConnectFailure::Unresponsive.lang_key(),
            ConnectFailure::Incompatible.lang_key(),
        ];
        assert_eq!(
            keys,
            [
                Key::ConnectBusy,
                Key::ConnectUnresponsive,
                Key::ConnectIncompatible
            ]
        );
    }
}
