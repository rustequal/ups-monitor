//! Routing: what a click, a keystroke or a tray gesture is asking for, and
//! which of the shell's own operations answers it.
//!
//! Nothing here draws or creates a window. The decisions come from
//! `ui::winstate`, the operations they name live in `super::windows`, and this
//! module is the map between them.

use windows::Win32::UI::WindowsAndMessaging::PostQuitMessage;

use super::Ui;
use crate::app::App;
use crate::ui::row::{HOT_BEEPER, HOT_CONFIRM_NO, HOT_CONFIRM_YES, HOT_SELFTEST, HOT_SETTINGS};
use crate::ui::winstate::{Effect, Input};
use crate::{evlog, ui};
/// Folds one settings action into the running one for a pass.
///
/// A terminal action — `Commit` or `Cancel` — is sticky: once an edit in the
/// pass asks to close the dialog, no later non-terminal edit downgrades it. The
/// old code let the last edit win outright, so a keystroke arriving after Enter
/// erased the commit, and the click's own action was clobbered by the first
/// edit unconditionally. Below the terminal actions, `Changed` dominates
/// `None`, so a pass that changed something repaints and one that changed
/// nothing does not — the old code turned every `None` into `Changed`, forcing
/// a needless resize on edits that changed no state.
fn merge_settings_action(
    current: ui::settings::Action,
    next: ui::settings::Action,
) -> ui::settings::Action {
    use ui::settings::Action;
    match (current, next) {
        (Action::Commit | Action::Cancel, _) => current,
        (_, Action::Commit | Action::Cancel) => next,
        (Action::Changed, _) | (_, Action::Changed) => Action::Changed,
        _ => Action::None,
    }
}

impl Ui {
    /// Routes a pass of clicks and keystrokes into the settings dialog.
    ///
    /// Split out of `tick` when the modal became one field of two kinds: the
    /// settings half has a draft behind it and does not belong inline beside
    /// the two-line confirmation half.
    ///
    /// Keyboard focus is not carried across here. It belongs to the window,
    /// which draws the ring and the caret from it directly, so a focus move
    /// needs no draft update and no rebuild — the window repaints itself.
    /// Mirroring it into the draft is what this function used to spend most of
    /// its length on, and the mirror was a second copy of one fact.
    pub(super) fn route_settings_input(&mut self) {
        let Some(window) = self.modal.as_ref().map(|m| &m.window) else {
            return;
        };
        let click = window.take_click();
        let edits = window.take_edits();
        if click.is_none() && edits.is_empty() {
            return;
        }
        self.handle_settings_input(click, edits);
    }

    /// Routes a click into the self-test confirmation. Only its two buttons do
    /// anything, and every other click is ignored.
    ///
    /// Keyboard and pointer arrive as the same event: Space or Enter on the
    /// focused button reports the same `clicked` a mouse release does, so Yes
    /// and No are reachable both ways through this one match. The edits queue
    /// is drained and dropped — Tab moves focus inside the window and needs
    /// nothing from here, and the modal has no text field to type into.
    pub(super) fn route_confirm_input(&mut self) {
        let Some(window) = self.modal.as_ref().map(|m| &m.window) else {
            return;
        };
        let click = window.take_click();
        let _ = window.take_edits();
        match click {
            Some(HOT_CONFIRM_YES) => {
                // Start the test first, then release modality: the panel
                // underneath must show the in-progress state as soon as it is
                // reachable again.
                self.app.run_self_test();
                self.dispatch(Input::ModalCommit);
                self.redraw_panel();
            }
            Some(HOT_CONFIRM_NO) => {
                self.dispatch(Input::ModalClose);
            }
            _ => {}
        }
    }

    pub(super) fn handle_tray(&mut self) -> bool {
        let Some(tray) = &self.tray else {
            return true;
        };
        let events = tray.poll_events();

        // First, before anything that might return early: without a
        // registration there is no icon to click, no tooltip to read and no
        // balloon to show, so every other branch below is meaningless until
        // this one has run.
        if events.taskbar_recreated {
            tray.readd();
            crate::evlog::event(
                crate::evlog::Cat::Session,
                "explorer restarted; tray icon re-registered",
            );
        }

        // A DPI or non-client-metrics change moves the small-icon size the
        // tray bitmaps are rendered at. Handled before the icon is next set,
        // so the refresh below picks up the new bitmaps rather than showing
        // the old size for one more pass.
        if events.metrics_changed {
            self.app.on_metrics_changed();
            self.refresh_icon();
        }

        // Hot-plug: skip the reconnect backoff.
        if events.device_changed {
            self.app.on_device_change();
        }
        // Every route into the two windows goes through the same decision
        // table, so the tray, the menu and the panel button cannot disagree
        // about what is allowed while the modal dialog is up. The table is
        // unit-tested in `ui::winstate`; this function only carries out what
        // it decides.
        if events.left_click && !self.dispatch(Input::TrayLeftClick) {
            return false;
        }
        let input = match events.command {
            Some(ui::tray_window::CMD_PANEL) => Some(Input::MenuPanel),
            Some(ui::tray_window::CMD_SETTINGS) => Some(Input::MenuSettings),
            Some(ui::tray_window::CMD_EXIT) => Some(Input::MenuExit),
            _ => None,
        };
        if let Some(input) = input {
            if !self.dispatch(input) {
                return false;
            }
        }
        true
    }

    /// Runs one user action through the decision table and performs the
    /// effect. Returns false to quit.
    pub(super) fn dispatch(&mut self, input: Input) -> bool {
        let effect = ui::winstate::decide(self.windows(), input);
        match effect {
            Effect::OpenPanel => self.open_panel(),
            Effect::ClosePanel => self.close_panel(),
            Effect::OpenSettings => self.open_settings(),
            Effect::OpenConfirmSelfTest => self.open_confirm_self_test(),
            Effect::CloseModal => self.close_modal(),
            Effect::FocusModal => {
                // Refusing silently would read as a dead click, so the modal
                // that is blocking the request is surfaced instead — whichever
                // kind it is.
                if let Some(modal) = &self.modal {
                    modal.window.focus();
                }
            }
            Effect::Quit => {
                // SAFETY: posts `WM_QUIT` to this thread's own message queue.
                // No arguments beyond the exit code, which is passed by value.
                unsafe { PostQuitMessage(0) };
                return false;
            }
            Effect::Nothing => {}
        }
        true
    }

    /// Applies a click on the status panel.
    pub(super) fn handle_panel_click(&mut self, id: ui::row::HotspotId) {
        match id {
            HOT_BEEPER => {
                self.app.toggle_beeper();
                self.redraw_panel();
            }
            HOT_SETTINGS => {
                self.dispatch(Input::PanelSettingsButton);
            }
            HOT_SELFTEST => {
                // A self-test moves the UPS onto its battery for a dozen
                // seconds, so it is never started on a stray click: this opens
                // the confirmation modal and returns. The button is already
                // greyed unless the pre-test conditions hold, and the session
                // re-checks them regardless, so the confirmation is about
                // intent, not safety. Running happens only if the user then
                // presses Yes, in `route_confirm_input`.
                self.dispatch(Input::PanelSelfTestButton);
            }
            _ => {}
        }
    }
    fn handle_settings_input(
        &mut self,
        click: Option<ui::row::HotspotId>,
        edits: Vec<(ui::row::HotspotId, ui::row::Edit)>,
    ) {
        use ui::settings::Action;

        let languages = self.app.languages;

        // Scroll-only fast path.
        //
        // Dragging a thumb produces nothing but `ScrollTo`, and a scroll
        // changes one integer: no row's text, no column width, no window
        // dimension. The general path below cannot know that — it rebuilds
        // every row from the config and the locale, re-measures the result
        // and resizes the frame, which for the settings dialog means about
        // seventy heap allocations and 26µs of work per mouse move, to move
        // a highlight by one line.
        //
        // Handled here rather than inside the draft because the draft has no
        // way to say "and nothing else changed"; that is a property of the
        // whole batch of events, which only this function sees.
        if click.is_none() {
            if let Some((id, offset)) = scroll_only(&edits) {
                let themes = self.app.themes;
                if let Some(draft) = self.app.draft.as_mut() {
                    // The draft still has to learn the new offset, or the
                    // next full rebuild would undo the scroll.
                    draft.on_list_edit(id, ui::row::Edit::ScrollTo(offset), themes, languages);
                }
                if let Some(modal) = &self.modal {
                    // The return value is deliberately ignored. It reports
                    // whether the window's own copy of the rows changed, and
                    // "it did not" is not a reason to fall through to the
                    // slow path — the draft has been updated either way, and
                    // rebuilding every row to reach the same conclusion is
                    // the exact work this path exists to avoid.
                    let _ = modal.window.set_list_scroll(id, offset);
                }
                return;
            }
        }

        let mut action = Action::None;
        let themes = self.app.themes;

        // Merge rule: see `merge_settings_action`. A terminal action is sticky;
        // otherwise Changed dominates None.
        if let Some(draft) = self.app.draft.as_mut() {
            if let Some(id) = click {
                action = merge_settings_action(action, draft.on_click(id, themes, languages));
            }
            // Keyboard edits are applied after the click, so clicking a field
            // and typing into it in the same pass works.
            for (id, edit) in edits {
                // Events aimed at a list carry that list's id, not the
                // focused field's, and the list must handle them or Enter on
                // a highlighted language would commit the whole dialog.
                // `option_count` answers the routing question — is this id a
                // list at all — and `on_list_edit` derives the length it
                // needs from the same two slices, so the number is not
                // carried across the boundary a second time.
                let a = match ui::settings::option_count(id, themes, languages) {
                    Some(_) => draft.on_list_edit(id, edit, themes, languages),
                    None => draft.on_edit(id, edit),
                };
                action = merge_settings_action(action, a);
            }
        }

        match action {
            Action::Commit => self.commit_settings(),
            Action::Cancel => {
                self.dispatch(Input::ModalClose);
            }
            Action::Changed => self.redraw_settings(),
            Action::None => {}
        }
    }

    /// Validates the draft and saves it. A rejected interval keeps the dialog
    /// open with the error shown, so the edit is never silently discarded.
    fn commit_settings(&mut self) {
        let Some(mut draft) = self.app.draft.take() else {
            return;
        };
        let locale_ok = {
            let App { config, locale, .. } = &mut self.app;
            draft.apply_to(config, *locale)
        };

        if !locale_ok {
            // Put the draft back so the error message is rendered.
            self.app.draft = Some(draft);
            self.redraw_settings();
            return;
        }

        self.app.apply_settings();
        // A new language changes the menu labels and a new theme changes the
        // icon, so both are pushed before anything is redrawn.
        self.sync_tray_labels();
        self.last_icon = None;
        self.refresh_icon();
        // Close last: the panel underneath must be redrawn in the new theme
        // and language, and it is only reachable once modality is released.
        self.dispatch(Input::ModalCommit);
        self.redraw_panel();
    }

    pub(super) fn deliver_notifications(&mut self) {
        let events = self.app.take_events();
        let Some(tray) = &self.tray else {
            return;
        };
        for ev in events {
            let title = self.app.locale.t(ev.title_key);
            let body = self.app.locale.t(ev.body_key);
            // Logged with the reading behind it, so the line stands on its own
            // as a record of what the user was told and what the device said
            // at that moment.
            let metrics = self
                .app
                .reading
                .as_ref()
                .map(evlog::metrics)
                .unwrap_or_default();
            let detail = if metrics.is_empty() {
                format!("notified: {title} — {body}")
            } else {
                format!("notified: {title} — {body} [{metrics}]")
            };
            evlog::event(evlog::Cat::Notify, &detail);
            tray.balloon(title, body, ev.severity);
        }
    }
}

/// The list and the single scroll offset a batch of edits asks for, if that is
/// *all* it asks for.
///
/// Both halves come back together because both are decided here. Returning
/// only the offset left the caller to recover the list id as `edits[0].0`,
/// which was correct solely because this function answers `Some` for a
/// non-empty batch — an invariant spanning two functions and written down in
/// neither, and an index that becomes a panic in the UI thread the moment this
/// one learns to answer `Some` for an empty batch. One value, one source.
///
/// Strict by design. Returns `None` the moment the batch contains anything
/// else — a keystroke, a hover, an Enter — because the fast path it guards
/// skips rebuilding the rows, and any other edit may change their text. A
/// predicate that were merely usually right would drop keystrokes typed in
/// the same pass as a scroll, which is exactly what happens when a user
/// spins the wheel while a field has focus.
///
/// Hover is excluded too, even though it changes only a highlight: hover and
/// scroll arrive together while dragging over a list, and the highlight is
/// drawn from the rows, so letting hover through here would move the thumb
/// and leave the highlight behind.
fn scroll_only(
    edits: &[(ui::row::HotspotId, ui::row::Edit)],
) -> Option<(ui::row::HotspotId, usize)> {
    let mut id = None;
    let mut offset = None;
    for (row_id, edit) in edits {
        match edit {
            ui::row::Edit::ScrollTo(n) => {
                // All from one list, or the fast path would patch the wrong
                // one. Two lists cannot be open at once, so this is a
                // consistency check rather than a real case.
                if *id.get_or_insert(*row_id) != *row_id {
                    return None;
                }
                // The last one wins: a burst of moves in a single pass ends
                // where the pointer ended.
                offset = Some(*n);
            }
            _ => return None,
        }
    }
    // `id` is set by the same arm that sets `offset`, so the two are `Some`
    // together; zipping them says so in the type instead of asserting it.
    id.zip(offset)
}

#[cfg(test)]
mod scroll_only_tests {
    use super::*;
    use crate::testsupport::hot;
    use ui::row::Edit;
    /// A pure scroll batch takes the fast path, and reports where it ended.
    #[test]
    fn a_pure_scroll_batch_is_recognised() {
        let e = vec![(hot(7), Edit::ScrollTo(3)), (hot(7), Edit::ScrollTo(5))];
        assert_eq!(
            scroll_only(&e),
            Some((hot(7), 5)),
            "the last offset in the pass wins, on the list that asked for it"
        );
    }

    /// Anything else in the batch disqualifies it.
    ///
    /// The fast path skips rebuilding the rows, so an edit that could change
    /// their text must never reach it. Being wrong here does not cost
    /// performance, it drops the user's keystroke.
    #[test]
    fn anything_but_a_scroll_disqualifies_the_batch() {
        for other in [
            Edit::Insert('7'),
            Edit::Backspace,
            Edit::Commit,
            Edit::Cancel,
            Edit::Highlight(ui::row::Move::Next),
            Edit::Hover(2),
            Edit::Scroll(1),
        ] {
            let e = vec![(hot(7), Edit::ScrollTo(3)), (hot(7), other)];
            assert_eq!(
                scroll_only(&e),
                None,
                "{other:?} alongside a scroll must force the full path"
            );
        }
    }

    /// An empty batch is not a scroll.
    #[test]
    fn an_empty_batch_is_not_a_scroll() {
        assert_eq!(scroll_only(&[]), None);
    }

    /// Scrolls aimed at two different lists force the slow path rather than
    /// patching whichever happened to come last.
    #[test]
    fn scrolls_from_two_lists_are_rejected() {
        let e = vec![(hot(7), Edit::ScrollTo(1)), (hot(9), Edit::ScrollTo(2))];
        assert_eq!(scroll_only(&e), None);
    }
}

#[cfg(test)]
mod settings_action_tests {
    use super::merge_settings_action;
    use crate::ui::settings::Action;

    /// A commit reached earlier in a pass survives a later keystroke. This is
    /// the bug: typing into a field after pressing Enter used to erase the
    /// commit, because the last edit won outright.
    #[test]
    fn a_commit_is_not_erased_by_a_later_edit() {
        let after = merge_settings_action(Action::Commit, Action::Changed);
        assert_eq!(after, Action::Commit);
    }

    /// A commit arriving after ordinary edits still wins.
    #[test]
    fn a_commit_later_in_the_pass_wins() {
        let after = merge_settings_action(Action::Changed, Action::Commit);
        assert_eq!(after, Action::Commit);
    }

    /// Cancel is terminal too, and equally sticky.
    #[test]
    fn a_cancel_is_sticky() {
        assert_eq!(
            merge_settings_action(Action::Cancel, Action::Changed),
            Action::Cancel
        );
        assert_eq!(
            merge_settings_action(Action::None, Action::Cancel),
            Action::Cancel
        );
    }

    /// `None` folded with `None` stays `None`: a pass that changed nothing must
    /// not trigger a repaint or resize. The old code turned this into `Changed`.
    #[test]
    fn nothing_changed_stays_none() {
        assert_eq!(
            merge_settings_action(Action::None, Action::None),
            Action::None
        );
    }

    /// Changed dominates None in either order.
    #[test]
    fn changed_dominates_none() {
        assert_eq!(
            merge_settings_action(Action::None, Action::Changed),
            Action::Changed
        );
        assert_eq!(
            merge_settings_action(Action::Changed, Action::None),
            Action::Changed
        );
    }
}
