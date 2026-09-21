//! Which windows exist, and what each user action does to that.
//!
//! Extracted from `main` as plain data because this is precisely the logic
//! that was wrong for twelve iterations. While the panel and the settings
//! view shared one window, "close settings" had to decide what to put back in
//! the frame, and it chose to show the panel — so dismissing settings
//! resurrected a window the user had closed, or had never opened. That bug
//! was unreachable by any test, because the only way to observe it was to
//! create real windows on a real Windows desktop.
//!
//! Here the decision is a function from (state, action) to an effect. The
//! window handles stay in `main`; the rules live here where they can be
//! checked without a message pump.
//!
//! There is exactly one modal *window* at a time, and the model says so with a
//! single `modal` flag rather than one boolean per modal kind. The settings
//! dialog and the self-test confirmation are two contents of that one window,
//! not two mechanisms: both disable the owner, both sit above the panel, both
//! are dismissed the same ways. What the table decides is identical for both —
//! freeze the panel, surface the modal, let Exit through — so it does not, and
//! must not, know which is up. This is the property that keeps the self-test
//! confirmation from ever leaving a click to be re-decided against a
//! bare-panel state: while it is up, `modal` is true, and every route to the
//! panel is refused here.

/// Which windows are currently on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Windows {
    pub panel: bool,
    /// True while any modal window is up — settings or the self-test
    /// confirmation. One flag, because there is one modal window.
    pub modal: bool,
}

/// Something the user did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Input {
    TrayLeftClick,
    MenuPanel,
    MenuSettings,
    MenuExit,
    /// The "Settings" button on the panel itself.
    PanelSettingsButton,
    /// The "Self-test" button on the panel: it asks to *confirm*, not to run.
    /// Running is what a confirmed modal leads to, handled by the caller.
    PanelSelfTestButton,
    /// The panel's own close box, or Esc.
    PanelClose,
    /// The modal's close box, Esc, Cancel, or No — any dismissal.
    ModalClose,
    /// The modal's affirmative action: OK in settings, Yes in the confirmation.
    /// Validation and the side effect (apply settings, start the test) are the
    /// caller's; this is only the transition that closes the modal afterwards.
    ModalCommit,
}

/// What the caller should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    OpenPanel,
    ClosePanel,
    OpenSettings,
    /// Put up the self-test confirmation modal.
    OpenConfirmSelfTest,
    CloseModal,
    /// A modal is already up; bring it forward instead of acting.
    FocusModal,
    Quit,
    Nothing,
}

/// The rules, in one place.
///
/// Three invariants matter and are asserted by the tests below:
///
/// 1. No input ever produces `OpenPanel` or `ClosePanel` while a modal is up.
///    The modal owns input, so the panel behind it must not appear, disappear
///    or change. This holds for the settings dialog and the self-test
///    confirmation alike, because the table cannot tell them apart.
/// 2. `CloseModal` never implies anything about the panel. Dismissing the
///    modal leaves the panel exactly as it was found.
/// 3. Exit works from every state, modal or not.
pub(crate) fn decide(w: Windows, input: Input) -> Effect {
    // Exit is deliberately checked first and unconditionally: it must work
    // even while a modal is up. The tray icon is not a window, so disabling
    // the panel does not disable the menu.
    if input == Input::MenuExit {
        return Effect::Quit;
    }

    // While a modal is up it is the only thing that can be interacted with.
    // Every route to the panel is refused, and the modal is surfaced so the
    // refusal is visible rather than silent.
    if w.modal {
        return match input {
            Input::ModalClose | Input::ModalCommit => Effect::CloseModal,
            Input::TrayLeftClick
            | Input::MenuPanel
            | Input::MenuSettings
            | Input::PanelSettingsButton
            | Input::PanelSelfTestButton => Effect::FocusModal,
            // The panel is disabled, so it cannot raise this itself; a flag
            // left over from before the modal opened is ignored.
            Input::PanelClose => Effect::Nothing,
            Input::MenuExit => Effect::Quit,
        };
    }

    match input {
        Input::TrayLeftClick => {
            if w.panel {
                Effect::ClosePanel
            } else {
                Effect::OpenPanel
            }
        }
        // "Panel" means show the status window. It is not a toggle: a user
        // picking it from a menu wants the panel, and hiding an already-open
        // one would read as the click having failed.
        Input::MenuPanel => {
            if w.panel {
                Effect::Nothing
            } else {
                Effect::OpenPanel
            }
        }
        // Settings does not need a panel and does not open one. Requiring a
        // panel was what forced the two windows to share a frame.
        Input::MenuSettings | Input::PanelSettingsButton => Effect::OpenSettings,
        // The self-test button only reaches here from the panel, so a panel is
        // always present; the confirmation opens over it.
        Input::PanelSelfTestButton => Effect::OpenConfirmSelfTest,
        Input::PanelClose => {
            if w.panel {
                Effect::ClosePanel
            } else {
                Effect::Nothing
            }
        }
        Input::ModalClose | Input::ModalCommit => Effect::Nothing,
        Input::MenuExit => Effect::Quit,
    }
}

/// Applies an effect to the model, so tests can run sequences.
///
/// Test-only by construction: in the real program the effect is carried out
/// against live `HWND`s and the new state is read back from whether those
/// handles exist. Keeping a second copy of that transition in the shipping
/// binary would be a model that could silently drift from what it models.
#[cfg(test)]
pub(crate) fn apply(w: Windows, e: Effect) -> Windows {
    match e {
        Effect::OpenPanel => Windows { panel: true, ..w },
        Effect::ClosePanel => Windows { panel: false, ..w },
        Effect::OpenSettings | Effect::OpenConfirmSelfTest => Windows { modal: true, ..w },
        Effect::CloseModal => Windows { modal: false, ..w },
        Effect::FocusModal | Effect::Quit | Effect::Nothing => w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_INPUTS: [Input; 9] = [
        Input::TrayLeftClick,
        Input::MenuPanel,
        Input::MenuSettings,
        Input::MenuExit,
        Input::PanelSettingsButton,
        Input::PanelSelfTestButton,
        Input::PanelClose,
        Input::ModalClose,
        Input::ModalCommit,
    ];

    /// Every (panel, modal) combination.
    const ALL_STATES: [Windows; 4] = [
        Windows {
            panel: false,
            modal: false,
        },
        Windows {
            panel: true,
            modal: false,
        },
        Windows {
            panel: false,
            modal: true,
        },
        Windows {
            panel: true,
            modal: true,
        },
    ];

    /// The reported bug, stated directly: dismissing a modal must never bring a
    /// panel into existence.
    #[test]
    fn closing_a_modal_never_opens_the_panel() {
        for w in ALL_STATES {
            for input in [Input::ModalClose, Input::ModalCommit] {
                let effect = decide(w, input);
                assert_ne!(
                    effect,
                    Effect::OpenPanel,
                    "closing a modal from {w:?} must not open the panel"
                );
                let after = apply(w, effect);
                assert_eq!(
                    after.panel, w.panel,
                    "closing a modal from {w:?} changed the panel"
                );
            }
        }
    }

    /// A panel minimised to tray before settings was opened stays gone after
    /// settings closes.
    #[test]
    fn panel_hidden_before_settings_stays_hidden_after() {
        let w = Windows {
            panel: false,
            modal: false,
        };
        let w = apply(w, decide(w, Input::MenuSettings));
        assert!(w.modal && !w.panel, "settings opened without a panel");

        let w = apply(w, decide(w, Input::ModalClose));
        assert!(!w.modal, "settings should be gone");
        assert!(!w.panel, "the panel must not have appeared");
    }

    /// An open panel survives the whole settings round trip untouched.
    #[test]
    fn open_panel_survives_settings_round_trip() {
        let mut w = Windows {
            panel: true,
            modal: false,
        };
        w = apply(w, decide(w, Input::PanelSettingsButton));
        assert_eq!(
            w,
            Windows {
                panel: true,
                modal: true
            }
        );
        w = apply(w, decide(w, Input::ModalCommit));
        assert_eq!(
            w,
            Windows {
                panel: true,
                modal: false
            }
        );
    }

    /// The self-test confirmation is modal exactly as settings is: it opens
    /// over the panel, freezes it, and returns it untouched. This is the
    /// reported bug's regression guard — a tray click during the confirmation
    /// is refused rather than closing the panel.
    #[test]
    fn self_test_confirmation_is_modal_over_the_panel() {
        let mut w = Windows {
            panel: true,
            modal: false,
        };
        w = apply(w, decide(w, Input::PanelSelfTestButton));
        assert_eq!(
            w,
            Windows {
                panel: true,
                modal: true
            },
            "confirming self-test must open a modal over the live panel"
        );
        // A tray click banked while the box was up is refused, not acted on.
        assert_eq!(decide(w, Input::TrayLeftClick), Effect::FocusModal);
        assert_eq!(decide(w, Input::PanelClose), Effect::Nothing);
        // Dismissing (No) returns the panel exactly as found.
        w = apply(w, decide(w, Input::ModalClose));
        assert_eq!(
            w,
            Windows {
                panel: true,
                modal: false
            }
        );
    }

    /// Nothing whatsoever opens, closes or toggles the panel while a modal is
    /// up. Checked exhaustively rather than by example, because the old bug was
    /// exactly a path nobody thought to try.
    #[test]
    fn panel_is_frozen_while_a_modal_is_up() {
        for w in ALL_STATES.into_iter().filter(|w| w.modal) {
            for input in ALL_INPUTS {
                let effect = decide(w, input);
                assert!(
                    !matches!(effect, Effect::OpenPanel | Effect::ClosePanel),
                    "{input:?} from {w:?} moved the panel while a modal was up"
                );
                let after = apply(w, effect);
                if effect != Effect::Quit {
                    assert_eq!(
                        after.panel, w.panel,
                        "{input:?} from {w:?} changed panel visibility"
                    );
                }
            }
        }
    }

    /// No input opens a second modal while one is up: it is surfaced instead.
    #[test]
    fn a_modal_up_is_surfaced_not_reopened() {
        for w in ALL_STATES.into_iter().filter(|w| w.modal) {
            for input in [
                Input::MenuSettings,
                Input::PanelSettingsButton,
                Input::PanelSelfTestButton,
                Input::TrayLeftClick,
                Input::MenuPanel,
            ] {
                assert_eq!(
                    decide(w, input),
                    Effect::FocusModal,
                    "{input:?} from {w:?} should surface the existing modal"
                );
            }
        }
    }

    /// Exit is the one thing that must always work, modal or not.
    #[test]
    fn exit_works_from_every_state() {
        for w in ALL_STATES {
            assert_eq!(decide(w, Input::MenuExit), Effect::Quit, "stuck in {w:?}");
        }
    }

    /// Settings opens with or without a panel, and never drags one in.
    #[test]
    fn settings_does_not_require_or_create_a_panel() {
        for panel in [false, true] {
            let w = Windows {
                panel,
                modal: false,
            };
            let effect = decide(w, Input::MenuSettings);
            assert_eq!(effect, Effect::OpenSettings);
            assert_eq!(apply(w, effect).panel, panel, "panel state was disturbed");
        }
    }

    #[test]
    fn tray_click_toggles_only_the_panel() {
        let w = Windows::default();
        let w = apply(w, decide(w, Input::TrayLeftClick));
        assert_eq!(
            w,
            Windows {
                panel: true,
                modal: false
            }
        );
        let w = apply(w, decide(w, Input::TrayLeftClick));
        assert_eq!(w, Windows::default());
    }

    /// The model must never wedge: from any state, some sequence reaches
    /// "everything closed" without going through Quit.
    #[test]
    fn every_state_can_be_returned_to_rest() {
        for start in ALL_STATES {
            let mut w = start;
            // Modal first — it owns input, so nothing else moves until it is
            // dismissed. Then the panel.
            for input in [Input::ModalClose, Input::PanelClose] {
                w = apply(w, decide(w, input));
            }
            assert_eq!(
                w,
                Windows::default(),
                "could not return to rest from {start:?}"
            );
        }
    }
}
