//! The keyboard focus ring: which controls Tab visits, in what order, and
//! where focus starts.
//!
//! Everything here is a pure function of the rows a window is drawing. That is
//! the whole design: the tab order is not a list maintained beside the layout,
//! it *is* the layout, read top to bottom. A control added to a window joins
//! the ring by being drawn, and cannot be forgotten here — there is nothing
//! here to forget it in.
//!
//! No Win32, no theme, no window handle, so every rule below is testable
//! without creating a window. The window procedure decides *when* focus moves;
//! this decides *where* it moves to.

use crate::ui::row::{HotspotId, Row};
/// The ids `row` contributes to the focus ring, in left-to-right order.
///
/// The one place a row is asked what the keyboard can reach inside it, and the
/// only exhaustive `match` over [`Row`] in this module: everything else here
/// is phrased in terms of this. The `match` has no wildcard, so a new variant
/// does not compile until its author has said whether the keyboard can reach
/// it — a wildcard would silently answer "no" for every control added from now
/// on, and the symptom, one control that Tab skips, is invisible to anyone who
/// does not try it.
///
/// Two kinds of control are deliberately absent. A disabled button (the
/// self-test button while a test is running) is skipped because focus on a
/// control that cannot act is a dead stop in the ring, and it registers no
/// hotspot to draw a ring around either. Headings, values, notices and the
/// open list's scrollbar are skipped because they are not controls: the
/// scrollbar in particular is reached by the arrow keys that already belong to
/// the list it scrolls.
///
/// A pair rather than a `Vec`: no row draws more than two controls, and the
/// callers below run on every mouse press.
fn ids(row: &Row) -> (Option<HotspotId>, Option<HotspotId>) {
    match row {
        Row::Checkbox { id, .. }
        | Row::Dropdown { id, .. }
        | Row::Field { id, .. }
        | Row::TitleButton { id, .. } => (Some(*id), None),
        Row::LabeledButton { id, enabled, .. } => (enabled.then_some(*id), None),
        Row::Buttons {
            left_id, right_id, ..
        } => (Some(*left_id), Some(*right_id)),
        Row::Header(_)
        | Row::Pair { .. }
        | Row::Wrapped { .. }
        | Row::Notice { .. }
        | Row::Space(_) => (None, None),
    }
}

/// The row that draws control `id`, if any row does.
///
/// What every question about a focused control goes through. The alternative —
/// asking the painted hotspots — was how "is this a button" came to depend on
/// a repaint having happened: `WM_KEYDOWN` outranks `WM_PAINT` in the queue, so
/// Enter pressed in the first moments of a dialog would have been answered
/// from an empty hotspot list and pressed OK instead of Cancel. The rows are
/// there from the moment the window is created.
fn owner(rows: &[Row], id: HotspotId) -> Option<&Row> {
    rows.iter().find(|row| {
        let (first, second) = ids(row);
        first == Some(id) || second == Some(id)
    })
}

/// Every control in `rows` that can hold keyboard focus, in the order the eye
/// meets them: top to bottom, and left before right within a row.
///
/// An iterator, not a `Vec`. The only caller in the program is [`step`], which
/// wants one element out of it, and building a vector to take one element from
/// it meant an allocation on every Tab — in the message handler of a keystroke,
/// where the work is a handful of comparisons.
pub(crate) fn ring(rows: &[Row]) -> impl Iterator<Item = HotspotId> + '_ {
    rows.iter().flat_map(|row| {
        let (first, second) = ids(row);
        first.into_iter().chain(second)
    })
}

/// Whether `id` is a control the keyboard can reach in `rows`.
pub(crate) fn contains(rows: &[Row], id: HotspotId) -> bool {
    owner(rows, id).is_some()
}

/// Whether `id` names a text field — the question typing asks, and the one
/// that decides whether Space is a character or an activation.
pub(crate) fn is_field(rows: &[Row], id: HotspotId) -> bool {
    matches!(owner(rows, id), Some(Row::Field { .. }))
}

/// Whether `id` names a dropdown — the question Alt+Down asks.
pub(crate) fn is_dropdown(rows: &[Row], id: HotspotId) -> bool {
    matches!(owner(rows, id), Some(Row::Dropdown { .. }))
}

/// Whether `id` names a button — the question Enter asks before deciding
/// between pressing what is focused and confirming the dialog.
///
/// Answered by [`Row::is_button`], the same table the painter's hover feedback
/// and the hotspots are built from, so the keyboard cannot come to a different
/// conclusion about what a button is than the pointer does.
pub(crate) fn is_button(rows: &[Row], id: HotspotId) -> bool {
    owner(rows, id).is_some_and(Row::is_button)
}

/// The control Tab moves to from `current`, or Shift+Tab when `back` is set.
///
/// The ring is closed: past the last control is the first. From nothing —
/// or from a control that is no longer there — it is the first control going
/// forward and the last going back, which is what makes the very first Tab
/// land at the top of the window.
///
/// Closing the ring is what makes the dialog's start state work. Focus opens
/// on Cancel, the last control there is; the first Tab wraps past it to the
/// interval field at the top, without Cancel having to be a special case.
pub(crate) fn step(rows: &[Row], current: Option<HotspotId>, back: bool) -> Option<HotspotId> {
    // Both arms read the same way: take the neighbour on the side we are moving
    // toward, and if there is none, wrap to the far end. The wrap is also what
    // answers the two cases that have no neighbour to speak of — no current
    // control, or a control no longer in the ring — because the search then
    // consumes the whole ring and finds nothing, which is precisely the
    // "start from the end we are coming from" answer those cases need.
    if back {
        ring(rows)
            .take_while(|id| Some(*id) != current)
            .last()
            .or_else(|| ring(rows).last())
    } else {
        ring(rows)
            .skip_while(|id| Some(*id) != current)
            .nth(1)
            .or_else(|| ring(rows).next())
    }
}

/// Where focus sits when a window opens, and where it returns when the control
/// holding it disappears.
///
/// The dismissing button of a dialog — Cancel in settings, No in the
/// confirmation — which is the right half of its [`Row::Buttons`] pair. A
/// window without such a row opens with no focus at all; the status panel is
/// that window, and it has nothing whose accidental activation would need to
/// be harmless.
///
/// Reading it off the rows rather than being told it by each window is the
/// same principle as the ring itself: the pair of dialog buttons is already
/// declared, once, in the row that draws them, and a second declaration
/// naming which of the two is the safe one could only ever drift from it.
/// Enter acts on the focused control, so opening here is exactly what makes
/// Enter on an untouched dialog dismiss it.
///
/// # Invariant
///
/// A window holds at most one [`Row::Buttons`]. This function and
/// [`confirm`] both answer from the first one they meet, so a second row of
/// buttons would be silently ignored — Enter and the opening focus would
/// belong to whichever pair happened to be built first, with nothing
/// anywhere reporting the ambiguity. The invariant holds by construction:
/// exactly three places build such a row (the settings dialog, the
/// self-test confirmation, and the panel's own), each builds one, and
/// `a_window_never_offers_two_rows_of_buttons` checks that they still do.
pub(crate) fn initial(rows: &[Row]) -> Option<HotspotId> {
    rows.iter().find_map(|row| match row {
        Row::Buttons { right_id, .. } => Some(*right_id),
        _ => None,
    })
}

/// The button Enter presses when focus is not itself on a button: the
/// confirming half of the dialog's button pair — OK, or Yes.
///
/// Enter after typing an interval means "and save that", which is what every
/// dialog with a text field in it does; without this it would mean nothing at
/// all, and the value would have to be committed with the mouse.
///
/// Answers from the first [`Row::Buttons`] in the window, under the same
/// one-per-window invariant [`initial`] documents.
pub(crate) fn confirm(rows: &[Row]) -> Option<HotspotId> {
    rows.iter().find_map(|row| match row {
        Row::Buttons { left_id, .. } => Some(*left_id),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::testsupport::hot;
    use crate::ui::row::{DropdownItem, Gap};
    /// No window offers two rows of buttons.
    ///
    /// `initial` and `confirm` both answer from the first `Row::Buttons` they
    /// meet, so a second one would be silently ignored: Enter and the opening
    /// focus would belong to whichever pair was built first, and nothing
    /// anywhere would say so. That is the kind of invariant that holds until
    /// the day someone adds a second pair, so it is checked against every
    /// builder in the crate rather than left as a property of the current
    /// layout.
    #[test]
    fn a_window_never_offers_two_rows_of_buttons() {
        fn button_rows(rows: &[Row]) -> usize {
            rows.iter()
                .filter(|r| matches!(r, Row::Buttons { .. }))
                .count()
        }

        let locale = crate::lang::Locale::english();
        let theme = crate::ui::theme::Theme::default();

        let settings =
            crate::ui::settings::SettingsDraft::from_config(&crate::config::Config::default())
                .rows(
                    locale,
                    &theme,
                    &crate::ui::theme::Builtin::ALL,
                    crate::lang::Locale::available(),
                );
        assert!(
            button_rows(&settings) <= 1,
            "the settings dialog builds {} rows of buttons",
            button_rows(&settings)
        );

        let confirm = crate::shell::confirm_rows(locale, &theme);
        assert!(
            button_rows(&confirm) <= 1,
            "the self-test confirmation builds {} rows of buttons",
            button_rows(&confirm)
        );

        let panel = crate::ui::panel::build(
            locale,
            &theme,
            &crate::ui::panel::PanelData {
                reading: None,
                identity: None,
                presence: crate::app::Presence::Unprobed,
                beeper: crate::app::BeeperView::Unknown,
                warnings: Vec::new(),
                self_test_running: false,
                probing: false,
            },
        );
        assert!(
            button_rows(&panel) <= 1,
            "the status panel builds {} rows of buttons",
            button_rows(&panel)
        );
    }

    fn field(id: HotspotId) -> Row {
        Row::Field {
            label: String::new(),
            value: String::new(),
            caret: 0,
            sel_start: 0,
            sel_end: 0,
            id,
        }
    }

    fn checkbox(id: HotspotId) -> Row {
        Row::Checkbox {
            label: String::new(),
            checked: false,
            depth: 0,
            id,
        }
    }

    fn dropdown(id: HotspotId) -> Row {
        Row::Dropdown {
            label: String::new(),
            options: vec![DropdownItem {
                label: "a".into(),
                native: String::new(),
                font: None,
            }],
            selected: 0,
            highlighted: 0,
            open: false,
            scroll: 0,
            id,
        }
    }

    fn labeled_button(id: HotspotId, enabled: bool) -> Row {
        Row::LabeledButton {
            label: String::new(),
            value: String::new(),
            color: Color::from_rgb(0, 0, 0),
            button: String::new(),
            id,
            enabled,
        }
    }

    fn buttons(left_id: HotspotId, right_id: HotspotId) -> Row {
        Row::Buttons {
            left: String::new(),
            left_id,
            right: String::new(),
            right_id,
        }
    }

    /// A dialog shaped like the settings window: a field, a list, a switch,
    /// then OK and Cancel.
    fn dialog() -> Vec<Row> {
        vec![
            Row::Header("t".into()),
            Row::Space(Gap::Half),
            field(hot(1)),
            dropdown(hot(2)),
            checkbox(hot(3)),
            buttons(hot(4), hot(5)),
        ]
    }

    /// The ring is the drawing order, and nothing but the controls is in it.
    #[test]
    fn the_ring_follows_the_drawing_order() {
        assert_eq!(
            ring(&dialog()).collect::<Vec<_>>(),
            vec![hot(1), hot(2), hot(3), hot(4), hot(5)]
        );
    }

    /// Both buttons of a `Buttons` row are reachable, left before right.
    #[test]
    fn both_dialog_buttons_are_in_the_ring() {
        let ids: Vec<HotspotId> = ring(&[buttons(hot(10), hot(11))]).collect();
        assert_eq!(ids, vec![hot(10), hot(11)]);
    }

    /// A greyed button is not a stop: focus on a control that cannot act
    /// would be a dead end, and it draws no hotspot to ring either.
    #[test]
    fn a_disabled_button_is_skipped() {
        let rows = vec![labeled_button(hot(7), false), labeled_button(hot(8), true)];
        assert_eq!(ring(&rows).collect::<Vec<_>>(), vec![hot(8)]);
    }

    /// Tab walks forward and wraps; Shift+Tab walks back and wraps.
    #[test]
    fn tab_wraps_at_both_ends() {
        let rows = dialog();
        assert_eq!(step(&rows, Some(hot(1)), false), Some(hot(2)));
        assert_eq!(step(&rows, Some(hot(5)), false), Some(hot(1)));
        assert_eq!(step(&rows, Some(hot(1)), true), Some(hot(5)));
        assert_eq!(step(&rows, Some(hot(2)), true), Some(hot(1)));
    }

    /// The first Tab of a fresh dialog reaches the first control, because
    /// focus starts on Cancel — the last one — and the ring is closed.
    #[test]
    fn the_first_tab_from_cancel_reaches_the_first_control() {
        let rows = dialog();
        let start = initial(&rows);
        assert_eq!(start, Some(hot(5)));
        assert_eq!(step(&rows, start, false), Some(hot(1)));
    }

    /// With focus nowhere, Tab enters at the top and Shift+Tab at the bottom.
    #[test]
    fn tab_enters_the_ring_from_either_end() {
        let rows = dialog();
        assert_eq!(step(&rows, None, false), Some(hot(1)));
        assert_eq!(step(&rows, None, true), Some(hot(5)));
    }

    /// A remembered id that is no longer drawn behaves as no focus at all
    /// rather than stalling the walk.
    #[test]
    fn a_vanished_control_does_not_trap_the_walk() {
        let rows = dialog();
        assert!(!contains(&rows, hot(99)));
        assert_eq!(step(&rows, Some(hot(99)), false), Some(hot(1)));
    }

    /// A window with no controls has nowhere to put focus, and Tab is inert
    /// rather than panicking on an empty ring.
    #[test]
    fn a_window_without_controls_has_no_focus() {
        let rows = vec![Row::Header("t".into()), Row::Space(Gap::Single)];
        assert_eq!(ring(&rows).collect::<Vec<_>>(), Vec::<HotspotId>::new());
        assert_eq!(step(&rows, None, false), None);
        assert_eq!(initial(&rows), None);
        assert_eq!(confirm(&rows), None);
    }

    /// The panel has no dialog buttons, so it opens with no focus and Enter
    /// has nothing to confirm.
    #[test]
    fn a_window_without_a_button_row_opens_unfocused() {
        let rows = vec![
            Row::TitleButton {
                title: String::new(),
                button: String::new(),
                id: HotspotId::new(20),
            },
            labeled_button(hot(21), true),
        ];
        assert_eq!(ring(&rows).collect::<Vec<_>>(), vec![hot(20), hot(21)]);
        assert_eq!(initial(&rows), None);
        assert_eq!(confirm(&rows), None);
    }

    /// The two halves of the button pair are told apart: Enter with nothing
    /// focused confirms, an untouched dialog dismisses.
    #[test]
    fn the_button_pair_names_both_roles() {
        let rows = dialog();
        assert_eq!(confirm(&rows), Some(hot(4)));
        assert_eq!(initial(&rows), Some(hot(5)));
    }
}
