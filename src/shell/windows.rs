//! Ownership of the windows: opening, closing, redrawing, and the content each
//! one is built from.
//!
//! Every window this program shows is created and destroyed here, and every
//! measurement of what goes in one is made here too — a window sized from one
//! measurement and filled from another is the failure this grouping exists to
//! prevent.

use super::{Modal, ModalKind, Ui};
use crate::ui::layout::PanelContent;
use crate::ui::row::{Gap, Row};
use crate::ui::row::{HOT_CONFIRM_NO, HOT_CONFIRM_YES};
use crate::ui::window::{ModalWindow, PanelWindow};
use crate::ui::winstate::Windows;
use crate::{lang, strings, ui};
/// The font choices a measurement at `dpi` is made under.
///
/// The family comes from the locale the rows are being built in and the size
/// from the theme they are being built with — the same two the window will be
/// told to draw with. Named once rather than spelled out at each of the three
/// call sites, so the three cannot come to differ.
fn script_for(
    theme: &ui::theme::Theme,
    family: Option<&'static str>,
    dpi: ui::Dpi,
) -> ui::text::Script {
    ui::text::Script {
        family,
        dpi,
        points: theme.font_points,
    }
}

impl Ui {
    /// Current window model, as the decision table sees it.
    pub(super) fn windows(&self) -> Windows {
        Windows {
            panel: self.panel.is_some(),
            modal: self.modal.is_some(),
        }
    }

    /// True when the modal currently up is the settings dialog. Used where the
    /// settings-specific redraw path must not run against a confirmation.
    fn is_settings_open(&self) -> bool {
        matches!(
            self.modal.as_ref().map(|m| m.kind),
            Some(ModalKind::Settings)
        )
    }

    /// Destroys whichever modal is up. The panel is untouched — it stays open
    /// if it was open, and stays absent if it was not. Nothing here opens a
    /// window, which is what the old shared-window design got wrong: closing
    /// the modal had to put *something* back in the frame, so it resurrected a
    /// panel the user had closed or never opened.
    ///
    /// Cleanup that is specific to the settings dialog — remembering its
    /// position, clearing the draft — is keyed on the modal's kind, so
    /// dismissing the transient confirmation neither saves a position it never
    /// owned nor disturbs a draft it never had.
    pub(super) fn close_modal(&mut self) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        if modal.kind == ModalKind::Settings {
            self.settings_pos = modal.window.position();
            self.app.draft = None;
        }
        // `modal` (and its window) is dropped here, which re-enables the owner
        // panel and destroys the window.
        drop(modal);
        if self.panel.is_none() {
            self.stop_timer();
        }
    }

    /// Creates the settings dialog, modal over the panel when one is open.
    pub(super) fn open_settings(&mut self) {
        if self.modal.is_some() {
            return;
        }
        self.app.draft = Some(ui::settings::SettingsDraft::from_config(&self.app.config));

        let theme = self.app.theme;
        // 96 is a starting size, not a guess that has to be right: no window
        // exists yet, and `Window::create` re-measures against the real
        // monitor DPI before this one is ever shown.
        let content = self.build_settings_content(&theme, ui::Dpi::BASELINE);
        let owner = self.panel.as_ref().map(PanelWindow::hwnd);
        // "UPS Monitor — Settings" in the active language: both halves come
        // from the locale, so the caption changes with the panel below it.
        let title = format!(
            "{} \u{2014} {}",
            self.app.locale.t(strings::Key::PanelTitle),
            self.app.locale.t(strings::Key::SettingsTitle),
        );
        let window = ModalWindow::open(
            content,
            &theme,
            self.app.locale.font(),
            owner,
            self.settings_pos,
            &title,
        );
        match window {
            Some(window) => {
                self.modal = Some(Modal {
                    window,
                    kind: ModalKind::Settings,
                });
                self.start_timer();
            }
            None => {
                // Creation failed: do not leave a draft behind claiming a
                // dialog is up when none is.
                self.app.draft = None;
            }
        }
    }

    /// Puts up the self-test confirmation: a question and Yes/No, modal over
    /// the panel exactly as settings is.
    ///
    /// This is reached only from the panel's Self-test button, so a panel is
    /// always present to own it. Unlike settings it carries no draft and keeps
    /// no position — it is transient and always centres over the panel.
    pub(super) fn open_confirm_self_test(&mut self) {
        if self.modal.is_some() {
            return;
        }
        let theme = self.app.theme;
        // See the note in `open_settings`: 96 is a starting size only, ahead
        // of `Window::create` re-measuring at the real monitor DPI.
        let content = self.build_confirm_content(&theme, ui::Dpi::BASELINE);
        let owner = self.panel.as_ref().map(PanelWindow::hwnd);
        let title = format!(
            "{} \u{2014} {}",
            self.app.locale.t(strings::Key::PanelTitle),
            self.app.locale.t(strings::Key::SelftestConfirmTitle),
        );
        // Centred over the panel: pass no saved position.
        let window =
            ModalWindow::open(content, &theme, self.app.locale.font(), owner, None, &title);
        if let Some(window) = window {
            self.modal = Some(Modal {
                window,
                kind: ModalKind::ConfirmSelfTest,
            });
            self.start_timer();
        }
    }

    /// Rebuilds the settings rows and resizes the dialog to match. Toggling
    /// the notifications master switch adds or removes four rows, so the
    /// frame has to follow or the buttons fall outside the client area.
    pub(super) fn redraw_settings(&mut self) {
        if !self.is_settings_open() {
            return;
        }
        let theme = self.app.theme;
        if let Some(modal) = &self.modal {
            let window = &modal.window;
            let content = self.build_settings_content(&theme, window.dpi());
            // Move the new palette into the window before repainting; the
            // content diff cannot see a colour-only change.
            window.apply_style(&theme, self.app.locale.font());
            // Size and pixels together: this same path serves language
            // changes, which do alter the row widths. A theme change leaves
            // the size unchanged (both themes share every metric), so the
            // resize half is then a no-op.
            window.set_content(content);
        }
    }

    pub(super) fn open_panel(&mut self) {
        let theme = self.app.theme;
        // See the note in `open_settings`: 96 is a starting size only, ahead
        // of `Window::create` re-measuring at the real monitor DPI.
        let content = self.build_content(&theme, ui::Dpi::BASELINE);
        let title = self.app.locale.t(strings::Key::PanelTitle).to_owned();
        self.panel = PanelWindow::open(
            content,
            &theme,
            self.app.locale.font(),
            self.panel_pos,
            &title,
        );
        if self.panel.is_some() {
            self.start_timer();
        }
    }

    pub(super) fn close_panel(&mut self) {
        // The refresh timer belongs to whichever windows are still open, so
        // it is only killed once both are gone.
        if self.modal.is_none() {
            self.stop_timer();
        }
        if let Some(panel) = &self.panel {
            // Remember where it was before the handle goes away, and push it
            // into the config too: `panel_pos` alone only survives until the
            // process exits, and shutdown() is what writes the INI.
            let pos = panel.position();
            self.panel_pos = pos;
            if let Some((x, y)) = pos {
                self.app.config.panel_x = x;
                self.app.config.panel_y = y;
            }
        }
        // The settings dialog is a separate window with its own draft, so
        // closing the panel must not touch it.
        // Dropping destroys the window. Nothing is left to render, so the
        // loop goes back to blocking in GetMessage at zero cost.
        self.panel = None;
    }

    /// Wraps rows in a sized `PanelContent`.
    ///
    /// Both windows size themselves the same way, and they must: this was
    /// written out twice, so a change to how width or height is derived had
    /// to be made in both places or the two windows would disagree about
    /// their own geometry.
    fn build_content(&self, theme: &ui::theme::Theme, dpi: ui::Dpi) -> PanelContent {
        let data = self.app.panel_data();
        PanelContent::measure(
            ui::panel::build(self.app.locale, theme, &data),
            ui::panel::width_samples(self.app.locale, theme),
            theme,
            theme.metrics.panel_width,
            script_for(theme, self.app.locale.font(), dpi),
        )
    }

    fn build_settings_content(&self, theme: &ui::theme::Theme, dpi: ui::Dpi) -> PanelContent {
        let rows = match &self.app.draft {
            Some(draft) => draft.rows(self.app.locale, theme, self.app.themes, self.app.languages),
            None => Vec::new(),
        };
        PanelContent::measure(
            rows,
            // No samples: the settings dialog has no value that changes while
            // it is open, so its own rows already carry every string it can
            // draw.
            Vec::new(),
            theme,
            theme.metrics.settings_width,
            script_for(theme, self.app.locale.font(), dpi),
        )
    }

    /// Rows for the self-test confirmation modal.
    ///
    /// Built to the same rhythm as the settings dialog so the two modals read
    /// as one family: a `Header` and a `Space(4)` at the top, the body, then a
    /// `Row::Buttons` pair. Reusing `Row::Buttons` is deliberate and load-
    /// bearing — the Yes/No buttons are laid out, bordered, and hover-lit by the
    /// exact code that draws OK/Cancel, so they match in position and in the
    /// background-on-hover feedback without a second implementation to keep in
    /// step. The half-spacing spacer above the buttons mirrors the gap settings
    /// leaves so both button rows sit the same distance under their last line.
    ///
    /// The question is a `Wrapped` row: it is a full sentence that runs to two
    /// or three lines, and wrapping is what keeps a longer translation growing
    /// the window downward rather than being clipped at the right edge.
    fn build_confirm_content(&self, theme: &ui::theme::Theme, dpi: ui::Dpi) -> PanelContent {
        let rows = confirm_rows(self.app.locale, theme);
        PanelContent::measure(
            rows,
            // No samples: the confirmation is one fixed question and two fixed
            // captions, so its own rows already carry every string it can
            // draw.
            Vec::new(),
            theme,
            theme.metrics.settings_width,
            script_for(theme, self.app.locale.font(), dpi),
        )
    }

    /// Rebuilds and repaints the panel. Does nothing when the panel is not
    /// open: building content costs a full row layout and measure pass, and
    /// laying out a window that does not exist is pure waste.
    pub(super) fn redraw_panel(&mut self) {
        if self.panel.is_none() {
            return;
        }
        let theme = self.app.theme;
        if let Some(panel) = &self.panel {
            let content = self.build_content(&theme, panel.dpi());
            // Move the new palette into the window first: painting reads the
            // window's stored theme, and `update` only carries rows and size.
            panel.apply_style(&theme, self.app.locale.font());
            // The frame has to follow the content, not just the painting
            // inside it. Both dimensions are measured now — the width from
            // the strings of the current language, the height from the rows
            // present — and a panel that repainted at a new width without
            // moving its frame simply drew into the old one: values clipped
            // at the right edge, or a band of dead background beside them.
            panel.set_content(content);
        }
    }

    pub(super) fn sync_tray_labels(&mut self) {
        if let Some(tray) = &mut self.tray {
            tray.set_menu_labels(
                self.app.locale.t(strings::Key::MenuPanel),
                self.app.locale.t(strings::Key::MenuSettings),
                self.app.locale.t(strings::Key::MenuExit),
            );
        }
    }

    /// Pushes the current reading to the tray icon and tooltip.
    ///
    /// Tray only: the panel is repainted by the caller, not here. This ran a
    /// panel repaint of its own once, which coupled the two and made the
    /// commit path draw the panel twice — once here, once explicitly after.
    /// Callers that need the panel to follow a new reading call `redraw_panel`
    /// themselves, so the responsibility sits in one place.
    ///
    /// Called only from the device half of `tick`, so it needs no guard of
    /// its own: reaching it already means something device-side may have
    /// changed. It briefly took a `force` flag and consumed a `reading_dirty`
    /// flag, both of which existed to undo the fact that this ran on every
    /// mouse move — a problem now fixed one level up, where it belonged.
    pub(super) fn refresh_icon(&mut self) {
        let Some(tray) = &mut self.tray else {
            return;
        };

        let tooltip = self.app.tooltip();
        if tooltip != self.last_tooltip {
            tray.set_tooltip(&tooltip);
            self.last_tooltip = tooltip;
        }

        let state = self.app.icon_state();
        if self.last_icon != Some(state) {
            if let Some((rgba, size)) = self.app.icons.get(state) {
                // Copied because set_icon takes &mut tray while the buffer is
                // still borrowed from the cache.
                let buf = rgba.to_vec();
                tray.set_icon(&buf, size);
                self.last_icon = Some(state);
            }
        }
    }
}

/// The self-test confirmation dialog's rows.
///
/// A free function rather than a method: it needs a locale and a theme and
/// nothing else from the application, and keeping it out of `impl` is what
/// lets the button-row invariant in `ui::focus` be checked against every
/// builder in the crate rather than against the two that happened to be
/// reachable.
pub(crate) fn confirm_rows(locale: lang::Locale, theme: &ui::theme::Theme) -> Vec<Row> {
    vec![
        Row::Header(locale.t(strings::Key::SelftestConfirmTitle).to_owned()),
        Row::Space(Gap::Half),
        Row::Wrapped {
            text: locale.t(strings::Key::SelftestConfirmBody).to_owned(),
            color: theme.colors.text_primary,
        },
        // The same half-spacing settings leaves below its last line before the
        // button row, so the buttons sit at the same distance under the text.
        Row::Space(Gap::Half),
        Row::Buttons {
            left: locale.t(strings::Key::SelftestConfirmYes).to_owned(),
            left_id: HOT_CONFIRM_YES,
            right: locale.t(strings::Key::SelftestConfirmNo).to_owned(),
            right_id: HOT_CONFIRM_NO,
        },
    ]
}

#[cfg(test)]
mod sized_dpi_tests {
    use crate::ui::layout::PanelContent;
    use crate::ui::row::Row;
    /// A representative slice of the panel: enough rows, of enough kinds,
    /// that a height bug shows up as more than a rounding blip.
    fn rows() -> Vec<Row> {
        (0..20)
            .map(|i| Row::Pair {
                label: format!("Label {i}"),
                value: format!("Value {i}"),
                color: crate::color::Color::TRANSPARENT,
            })
            .collect()
    }

    /// `PanelContent::measure` must measure height at the window's real DPI,
    /// not always at 96 — the regression this guards against left every window
    /// too short for its own content on any DPI but 96, because the height was
    /// derived from the raw, unscaled theme regardless of what `dpi` said. The
    /// width took `dpi` correctly even then, which is what made the bug easy to
    /// miss: the window came out the right *width* and the wrong *height*, and
    /// only revealed itself once the row count was large enough for the missing
    /// scale to clip something visibly — exactly the shape of the report this
    /// test is named after.
    #[test]
    fn height_scales_with_dpi_like_width_does() {
        let theme = crate::ui::theme::Theme::default_theme();

        let at_96 = PanelContent::measure(
            rows(),
            Vec::new(),
            &theme,
            theme.metrics.panel_width,
            super::script_for(&theme, None, crate::ui::Dpi::BASELINE),
        );
        let at_150 = PanelContent::measure(
            rows(),
            Vec::new(),
            &theme,
            theme.metrics.panel_width,
            super::script_for(&theme, None, crate::ui::Dpi::new(144)),
        );

        assert!(
            at_150.height > at_96.height,
            "height must grow with DPI: {} at 96 DPI, {} at 150%",
            at_96.height,
            at_150.height
        );
        // Not a strict 1.5x — text wrapping and rounding mean the real
        // ratio drifts a little from the theoretical one — but a height
        // that failed to scale at all (the actual bug) sits at 1.0x, and a
        // fixed one should land close to the 1.5x DPI factor.
        let ratio = at_150.height as f32 / at_96.height as f32;
        assert!(
            (1.4..1.6).contains(&ratio),
            "height ratio {ratio:.2} is not close to the 1.5x DPI factor \
             (96 -> 144 DPI); a ratio near 1.0 means height stopped \
             tracking DPI again"
        );
    }
}
