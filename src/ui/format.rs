//! Value formatting shared by the panel and the tray tooltip.
//!
//! Pure string work with no windowing dependency, so it stays testable and is
//! reusable by both the painted panel and the tray tooltip.

/// Formats seconds as a localized minute count.
///
/// Always minutes, never a composite like "1h 05m" and never a fallback to
/// seconds. Runtime is read at a glance during an outage to answer one
/// question — how long is left — and a unit that changes with the value makes
/// two readings harder to compare than one that does not. Truncated rather
/// than rounded up, so the figure never overstates the reserve.
pub(crate) fn format_runtime(seconds: u32, min_unit: &str) -> String {
    format!("{} {}", seconds / 60, min_unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_above_one_minute() {
        assert_eq!(format_runtime(600, "min"), "10 min");
        assert_eq!(format_runtime(60, "min"), "1 min");
    }

    #[test]
    fn under_a_minute_stays_in_minutes() {
        // The unit never switches: a sub-minute reserve reads as "0 min",
        // which is the honest rounding of less than one minute left.
        assert_eq!(format_runtime(59, "min"), "0 min");
        assert_eq!(format_runtime(0, "min"), "0 min");
    }

    /// Hours are never introduced, however long the reserve.
    #[test]
    fn long_runtimes_stay_in_minutes() {
        assert_eq!(format_runtime(3600, "min"), "60 min");
        assert_eq!(format_runtime(7_245, "min"), "120 min");
    }

    #[test]
    fn truncates_rather_than_rounds_up() {
        // 119 s is 1 minute and change; reporting 2 would overstate reserve.
        assert_eq!(format_runtime(119, "min"), "1 min");
    }
}
