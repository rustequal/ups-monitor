mod descriptor;
mod device;
mod selftest;
mod ups;

pub(crate) use selftest::{SelfTestNotRun, SelfTestOutcome, MIN_CHARGE_PERCENT};
// `TestResult` used to be reachable only through the `Reading::test_result`
// field, so this re-export was gated on `test` and existed for fixtures alone.
// The panel now names the type: the Last test row asks the device's register
// whether a test is running, which is the only question that covers a test
// started from the front panel of the UPS.
pub(crate) use ups::TestResult;
pub(crate) use ups::{Beeper, Identity, Reading, Ups};
#[cfg(test)]
mod tests {
    /// The `Test` feature is never written, from anywhere in this layer.
    ///
    /// This is the layer's one safety invariant, and it is not about tidiness.
    /// Writing `2` to `UPS.Output.Test` on this firmware starts a discharge
    /// that does not end by itself: 120 s of observation showed no change, and
    /// mains came back only after an explicit `3`. A code path that writes it
    /// by accident flattens the battery of whatever the user has plugged in.
    /// `1`, which NUT uses for a quick test, is simply ignored here — so there
    /// is nothing to gain from writing this feature at all, and everything to
    /// lose. The self-test goes over the vendor ASCII channel instead.
    ///
    /// The invariant is stated as a closed list of write sites rather than as
    /// "no line mentions `U_TEST` near a write". A grep for the usage would
    /// pass the day someone writes it through a variable; enumerating the
    /// places that write *at all* fails on any new one and forces the decision
    /// to be made deliberately, by whoever adds it.
    ///
    /// **Structural, and permanently so.** The subject of this test *is* the
    /// source text, which is what separates it from the source-reading tests
    /// this suite has been retiring. Those stood in for a behaviour that could
    /// be lifted into a function and asserted on as a value. This one does not
    /// stand in for anything: the property is "no place in the tree other than
    /// the two listed writes a feature report", and a place in the tree is not
    /// a value any function can return. A new call site is caught here whether
    /// or not it ever executes — which for this property is the point, not a
    /// weakness.
    #[test]
    fn test_feature_is_never_written() {
        /// Every function in this layer permitted to reach `set_feature`, and
        /// what each one writes.
        const WRITE_SITES: [(&str, &str, &str); 2] = [
            ("ups.rs", "pub(crate) fn set_beeper(", "AudibleAlarmControl"),
            (
                "selftest.rs",
                "fn write_gate(",
                "the vendor gate, report 37",
            ),
        ];

        let sources = [
            ("ups.rs", include_str!("ups.rs")),
            ("selftest.rs", include_str!("selftest.rs")),
            ("descriptor.rs", include_str!("descriptor.rs")),
        ];

        for (file, src) in sources {
            let mut found = 0usize;
            for (site_file, signature, _) in WRITE_SITES {
                if site_file == file {
                    let body = crate::testsupport::fn_body(src, signature);
                    found += body.matches("set_feature(").count();
                    assert!(
                        !body.contains("U_TEST"),
                        "{signature} in {file} names the Test usage; writing it \
                         starts a discharge that does not stop"
                    );
                }
            }
            assert_eq!(
                src.matches("set_feature(").count(),
                found,
                "{file} reaches `set_feature` outside the sites listed in \
                 WRITE_SITES; every write path has to be accounted for here, \
                 because the one usage that must never be written is invisible \
                 until someone looks"
            );
        }
    }
}
