//! Tray icon cache.
//!
//! The four state icons are rendered once when the theme or the system icon
//! metric changes, never per poll. `Reading` maps to a state by the priority
//! rule: disconnected > critical > on battery > normal.

use std::collections::HashMap;

use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

use crate::app::Presence;
use crate::hid::Reading;
use crate::icon::{render, IconSet, IconSpec};
use crate::ui::theme::Theme;
pub(crate) struct IconCache {
    icons: HashMap<IconState, Vec<u8>>,
    size: u32,
    /// What the cached bitmaps were rendered from. Compared by value, not by
    /// theme name: a name identifies a file, and the same file recoloured and
    /// re-applied is a different set of pixels under an identical name. The
    /// name comparison this replaces left the tray on stale colours after an
    /// edit-and-apply of the current theme.
    set: IconSet,
}

impl IconCache {
    pub(crate) fn new(theme: &Theme) -> Self {
        let size = system_icon_size();
        Self {
            icons: render_all(theme, size),
            size,
            set: theme.icons,
        }
    }

    /// Rebuilds only when the icon definitions or the system icon size
    /// actually changed.
    pub(crate) fn refresh(&mut self, theme: &Theme) {
        let size = system_icon_size();
        if size == self.size && theme.icons == self.set {
            return;
        }
        self.icons = render_all(theme, size);
        self.size = size;
        self.set = theme.icons;
    }

    pub(crate) fn get(&self, state: IconState) -> Option<(&[u8], u32)> {
        self.icons.get(&state).map(|b| (b.as_slice(), self.size))
    }
}

fn render_all(theme: &Theme, size: u32) -> HashMap<IconState, Vec<u8>> {
    IconState::ALL
        .iter()
        .map(|&state| {
            let spec = theme.icons.spec(state);
            (state, render(spec, size))
        })
        .collect()
}
/// The four tray states, resolved by priority: the first match wins.
///
/// Lives here rather than in `icon.rs` because it is not part of the drawing:
/// the renderer is handed one [`IconSpec`] and never asks which state it came
/// from. `icon.rs` is also compiled into the build script, which draws the exe
/// mark and has no notion of a tray state at all — an item it can never reach
/// is one it must be told to ignore, and being told to ignore things is how a
/// lint stops being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum IconState {
    Normal,
    OnBattery,
    Critical,
    Disconnected,
}

impl IconState {
    pub(crate) const ALL: [IconState; 4] = [
        IconState::Normal,
        IconState::OnBattery,
        IconState::Critical,
        IconState::Disconnected,
    ];
}

impl IconSet {
    /// The spec for one tray state.
    ///
    /// An inherent method on a type declared elsewhere, and deliberately: the
    /// set is shared with the build script, the mapping from a state to a
    /// member of it is not.
    pub(crate) fn spec(&self, state: IconState) -> IconSpec {
        match state {
            IconState::Normal => self.normal,
            IconState::OnBattery => self.on_battery,
            IconState::Critical => self.critical,
            IconState::Disconnected => self.disconnected,
        }
    }
}

/// Small-icon metric rather than a fixed 16 or 32, so DPI scaling works.
///
/// `pub(super)` because the tray window renders its own icon to the same shell
/// surface and needs the same number. That is the whole extent of the audience:
/// both callers live in `ui/`, and the visibility says so.
pub(super) fn system_icon_size() -> u32 {
    // SAFETY: reads one system metric by index and returns it by value.
    let px = unsafe { GetSystemMetrics(SM_CXSMICON) };
    if px <= 0 {
        16
    } else {
        (px as u32).clamp(16, 64)
    }
}

/// Priority order is fixed: the first matching condition wins.
pub(crate) fn state_for(reading: Option<&Reading>, presence: Presence) -> IconState {
    if presence != Presence::Open {
        return IconState::Disconnected;
    }
    let Some(r) = reading else {
        return IconState::Disconnected;
    };
    if r.is_critical() {
        return IconState::Critical;
    }
    if r.on_battery() {
        return IconState::OnBattery;
    }
    IconState::Normal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online() -> Reading {
        Reading {
            ac_present: Some(true),
            ..Default::default()
        }
    }

    /// A connected UPS on mains is Normal from the very first reading.
    ///
    /// The icon used to stay grey after a successful connect because the poll
    /// thread's wake-up was gated on a window being open, so the first
    /// `Update` sat unread and this function was never reached. The rule
    /// itself was always right; nothing was asking it. Kept as a test so a
    /// future gate cannot quietly reintroduce "the icon reports the state of
    /// the UI rather than of the device".
    #[test]
    fn connected_on_mains_is_normal_immediately() {
        assert_eq!(
            state_for(Some(&online()), Presence::Open),
            IconState::Normal
        );
    }

    #[test]
    fn connected_without_a_reading_yet_is_disconnected() {
        // Before the first poll there is genuinely nothing to report.
        assert_eq!(state_for(None, Presence::Open), IconState::Disconnected);
    }

    #[test]
    fn priority_order_is_first_match_wins() {
        assert_eq!(
            state_for(Some(&online()), Presence::Absent),
            IconState::Disconnected
        );

        let on_batt = Reading {
            ac_present: Some(false),
            ..online()
        };
        assert_eq!(
            state_for(Some(&on_batt), Presence::Open),
            IconState::OnBattery
        );

        // Critical outranks on-battery, which is the case that matters: a UPS
        // on battery *and* low must not show the milder of the two.
        let critical = Reading {
            below_capacity_limit: Some(true),
            ..on_batt
        };
        assert_eq!(
            state_for(Some(&critical), Presence::Open),
            IconState::Critical
        );

        for r in [
            Reading {
                internal_failure: Some(true),
                ..online()
            },
            Reading {
                overload: Some(true),
                ..online()
            },
        ] {
            assert_eq!(state_for(Some(&r), Presence::Open), IconState::Critical);
        }
    }

    /// B8: every flag the panel colours critical must also drive the tray to
    /// Critical. They diverged before — the tray counted three flags, the panel
    /// six — so a runtime-limit or out-of-range fault showed red in the window
    /// while the tray stayed green. Both now route through `Reading::is_critical`,
    /// and this pins that each of the six flags trips it.
    #[test]
    fn every_panel_critical_flag_makes_the_tray_critical() {
        let flags: [fn(&mut Reading); 6] = [
            |r| r.internal_failure = Some(true),
            |r| r.overload = Some(true),
            |r| r.below_capacity_limit = Some(true),
            |r| r.runtime_limit_expired = Some(true),
            |r| r.voltage_out_of_range = Some(true),
            |r| r.frequency_out_of_range = Some(true),
        ];
        for set in flags {
            let mut r = online();
            set(&mut r);
            assert!(r.is_critical(), "flag must count as critical");
            assert_eq!(state_for(Some(&r), Presence::Open), IconState::Critical);
        }
    }

    /// A8: an unread flag is not a fault. A `None` fault flag must leave the
    /// tray Normal, not Critical — the visual form of the phantom-notification
    /// bug is a red icon for a flag that never read.
    #[test]
    fn an_unread_fault_flag_is_not_critical() {
        let r = Reading {
            internal_failure: None,
            overload: None,
            below_capacity_limit: None,
            ..online()
        };
        assert!(!r.is_critical());
        assert_eq!(state_for(Some(&r), Presence::Open), IconState::Normal);
    }

    /// A8: an unread mains flag is not on-battery. `None` must resolve to
    /// Normal, matching the panel, not to the on-battery icon.
    #[test]
    fn an_unread_mains_flag_is_not_on_battery() {
        let r = Reading {
            ac_present: None,
            ..online()
        };
        assert!(!r.on_battery());
        assert_eq!(state_for(Some(&r), Presence::Open), IconState::Normal);
    }
}
