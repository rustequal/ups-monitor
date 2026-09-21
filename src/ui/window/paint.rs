use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::HFONT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, FillRect, IntersectClipRect, Polygon, SetBkMode, DT_CENTER, DT_LEFT, SRCCOPY,
    TRANSPARENT,
};

use super::draw::{button_fill, draw_focus_ring, fill_bordered, fill_solid};
use super::state::{DropdownState, Hotspot, Painted, PaintedField, PaintedList, WindowState};
use crate::color::Color;
use crate::ui::gdi::{Bitmap, Brush, Canvas, MemDc, Pen, Selection};
use crate::ui::layout::{button_width_for, column_origin, dialog_button_width, text_column_width};
use crate::ui::rect::Rect;
use crate::ui::row::{DropdownItem, HotspotId, Row};
use crate::ui::scrollbar::{
    draw_stepper, ListLayout, ListWindow, OptionRole, ScrollbarGeometry, HOT_SCROLL_DOWN,
    HOT_SCROLL_PAGE_DOWN, HOT_SCROLL_PAGE_UP, HOT_SCROLL_THUMB, HOT_SCROLL_UP,
};
use crate::ui::text::{
    caret_offsets, draw_item, draw_text, draw_wrapped, text_width, GdiMetrics, Script,
};
use crate::ui::theme::ScaledTheme;
// `AdjustWindowRectExForDpi` and `GetSystemMetricsForDpi` both live here, not
// under `WindowsAndMessaging` with their DPI-agnostic counterparts
// (`AdjustWindowRectEx`, `GetSystemMetrics`): the DPI-aware overload for each
// is a distinct import from a distinct module, and grouping them together —
// rather than beside the sibling they replace — is what keeps a future
// caller from reaching for the wrong one out of habit.

/// The rectangle a row's label is drawn into.
///
/// Its own function because four call sites build it identically, and because
/// those four numbers *are* one rectangle — passing them separately made
/// `draw_label` an eight-argument function whose first four arguments were a
/// shape nobody had named. The left column starting at `origin` and running
/// `label_w` wide is the layout contract every labelled row shares.
fn label_rect(origin: i32, y: i32, label_w: i32, line: i32) -> Rect {
    Rect {
        left: origin,
        top: y,
        right: origin + label_w,
        bottom: y + line,
    }
}

/// Draws a row's label in the left column.
///
/// The same thirteen lines appeared in four match arms, which is four places
/// to edit when the label column changes colour or alignment, and four places
/// for one of them to be missed. The column geometry — left edge at `origin`,
/// width `label_w` — is the layout contract every labelled row shares.
///
/// The caller selects the label font into `canvas` first; it is passed
/// straight to [`draw_text`], which measures and draws in that selection.
pub(super) fn draw_label(
    canvas: Canvas<'_>,
    label: &str,
    rect: Rect,
    theme: &ScaledTheme,
    script: Script,
) {
    draw_text(
        canvas,
        label,
        rect,
        theme.colors().text_secondary,
        DT_LEFT,
        theme.colors().background,
        script,
    );
}

/// Everything a row-drawing routine needs that does not change within a frame.
///
/// Thirteen values were computed once at the top of `paint` and then read by
/// twelve match arms below it. Carried separately they could not leave that
/// function: the first arm lifted out would have taken ten parameters, and the
/// second would have taken ten more. Named together they are one thing — the
/// frame the rows are drawn into — and each arm becomes a method on it.
struct Frame<'a> {
    /// The back buffer. Not the window: the frame reaches the screen in one
    /// `BitBlt` after every row is drawn.
    canvas: Canvas<'a>,
    theme: ScaledTheme,
    script: Script,
    width: i32,
    line: i32,
    spacing: i32,
    /// Left edge of the label column, which is `pad` unless a full-width row
    /// widened the window past what the columns needed.
    origin: i32,
    label_w: i32,
    /// The width a full-width row's text flows into, from the same function
    /// that gave the window its height.
    text_col: i32,
    btn_min: i32,
    /// The shared width of the two value-column buttons, off the measurement
    /// that sized the window rather than measured again here: it is a property
    /// of the language, not of the caption showing this frame.
    act_btn: i32,
    /// Which control the keyboard is aimed at, and whether its ring is shown.
    /// Read once: neither changes while a frame is drawn.
    focus: Option<HotspotId>,
    ring_visible: bool,
    /// Which button the pointer is over, for the hover fill.
    hovered_button: Option<HotspotId>,
    font_bold: HFONT,
    /// Whether the caret is in its visible half of the blink. Read once for
    /// the same reason as the focus: the frame is one instant.
    caret_visible: bool,
}

/// One `Row::LabeledButton`, flattened for the method that draws it.
///
/// Its six fields plus `is_button`, which the row answers and the method must
/// not decide for itself. Grouped rather than passed one by one for the reason
/// [`Frame`] itself is grouped: as separate parameters this is a
/// ten-argument function, and `too_many_arguments` is right about
/// that — a call site nobody can check by eye. Raising the lint's threshold
/// would silence the complaint without answering it; naming the group answers
/// it, because these values are one row and were always passed together.
///
/// Every field is `Copy`, so the method opens with one `let &LabeledButtonRow
/// { .. } = row;` and the body below reads exactly as it did inside the match
/// arm.
struct LabeledButtonRow<'a> {
    label: &'a str,
    value: &'a str,
    color: Color,
    button: &'a str,
    id: HotspotId,
    enabled: bool,
    /// [`Row::is_button`] for this row, asked once in `draw_row`.
    is_button: bool,
}

/// One `Row::Dropdown` as the closed control needs it. See
/// [`LabeledButtonRow`] for why the fields travel as a group.
///
/// `highlighted` and `scroll` are absent: they describe the open list, which
/// is painted by [`Frame::paint_dropdown_list`] out of [`DropdownState`] after
/// every row has been drawn.
struct DropdownRow<'a> {
    label: &'a str,
    options: &'a [DropdownItem],
    selected: usize,
    open: bool,
    id: HotspotId,
    /// [`Row::is_button`] for this row, asked once in `draw_row`.
    is_button: bool,
}

/// One `Row::Field`. See [`LabeledButtonRow`] for why the fields travel as a
/// group.
struct FieldRow<'a> {
    label: &'a str,
    value: &'a str,
    caret: usize,
    sel_start: usize,
    sel_end: usize,
    id: HotspotId,
    /// [`Row::is_button`] for this row, asked once in `draw_row`.
    is_button: bool,
}

impl Frame<'_> {
    /// The dialog's OK and Cancel pair, side by side at the bottom.
    fn draw_buttons(
        &self,
        y: i32,
        body: i32,
        left: (&str, HotspotId),
        right: (&str, HotspotId),
        acc: &mut Accumulator<'_>,
    ) {
        let (left, left_id) = left;
        let (right, right_id) = right;
        // The box starts at the cursor: the lead-in that balances the
        // gap above the buttons against the window's bottom margin
        // below them is `Row::lead_in` for this variant, and has
        // already been charged before the match.
        //
        // OK and Cancel share one width, from the same function the
        // window was measured with, so a caption that needs more room
        // gets it in both places at once.
        let bw = dialog_button_width(left, right, self.btn_min, self.spacing, |s| {
            text_width(self.canvas, s, self.script)
        });
        let lr = Rect {
            left: self.origin,
            top: y,
            right: self.origin + bw,
            bottom: y + body,
        };
        let rr = Rect {
            left: self.origin + bw + self.spacing,
            top: y,
            right: self.origin + bw * 2 + self.spacing,
            bottom: y + body,
        };
        // Both buttons are drawn identically — surface fill, primary
        // caption, the same border and radius as every other button in
        // both windows. OK carried a blue accent fill and a background
        // caption before; that made it the one button the shared hover
        // feedback could not treat like the others, so it is gone and
        // OK is now just another button.
        let lfill = button_fill(self.hovered_button, left_id, &self.theme);
        let rfill = button_fill(self.hovered_button, right_id, &self.theme);
        fill_bordered(
            self.canvas,
            &lr,
            lfill,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );
        fill_bordered(
            self.canvas,
            &rr,
            rfill,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );

        draw_text(
            self.canvas,
            left,
            lr,
            self.theme.colors().text_primary,
            DT_CENTER,
            lfill,
            self.script,
        );
        draw_text(
            self.canvas,
            right,
            rr,
            self.theme.colors().text_primary,
            DT_CENTER,
            rfill,
            self.script,
        );
        draw_focus_ring(
            self.canvas,
            &lr,
            self.focus == Some(left_id),
            self.ring_visible,
            &self.theme,
        );
        draw_focus_ring(
            self.canvas,
            &rr,
            self.focus == Some(right_id),
            self.ring_visible,
            &self.theme,
        );
        acc.painted.hotspots.push(Hotspot {
            rect: lr,
            id: left_id,
            is_button: true,
        });
        acc.painted.hotspots.push(Hotspot {
            rect: rr,
            id: right_id,
            is_button: true,
        });
    }

    /// A checkbox and its label, indented by `depth` levels.
    fn draw_checkbox(
        &self,
        y: i32,
        label: &str,
        checked: bool,
        depth: u8,
        id: HotspotId,
        acc: &mut Accumulator<'_>,
    ) {
        // Tied to the row height so the box scales with the text
        // rather than staying a fixed size against a smaller line.
        let box_side = (self.line * 3 / 4).max(10);
        let box_top = y + (self.line - box_side) / 2;
        // The whole row steps in with its depth — box, label and hit
        // rectangle together — so a subordinate switch reads as
        // belonging to the one above it.
        let left = self.origin + self.theme.metrics().indent * i32::from(depth);
        let boxr = Rect {
            left,
            top: box_top,
            right: left + box_side,
            bottom: box_top + box_side,
        };
        let fill = if checked {
            self.theme.colors().accent
        } else {
            self.theme.colors().surface
        };
        fill_bordered(
            self.canvas,
            &boxr,
            fill,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            0,
        );
        if checked {
            // A tick drawn as text keeps this to one code path
            // instead of pulling in a path-drawing API.
            draw_text(
                self.canvas,
                "✓",
                boxr,
                self.theme.colors().background,
                DT_CENTER,
                self.theme.colors().accent,
                self.script,
            );
        }
        draw_text(
            self.canvas,
            label,
            Rect {
                left: left + box_side + self.spacing,
                top: y,
                right: self.width - self.origin,
                bottom: y + self.line,
            },
            self.theme.colors().text_primary,
            DT_LEFT,
            self.theme.colors().background,
            self.script,
        );
        // The ring goes round the box alone, not the row: the box is
        // what the control *is*, and a frame stretched to the end of
        // the caption would read as a selected line of text.
        draw_focus_ring(
            self.canvas,
            &boxr,
            self.focus == Some(id),
            self.ring_visible,
            &self.theme,
        );
        // The whole row is clickable, not just the box.
        let hit = Rect {
            left,
            top: y,
            right: self.width - self.origin,
            bottom: y + self.line,
        };
        acc.painted.hotspots.push(Hotspot {
            rect: hit,
            id,
            is_button: false,
        });
    }

    /// The panel's title line, with the Settings button on its right.
    ///
    /// Title aligned with the label column, button against the right margin.
    ///
    /// The two do not share an edge and should not: the title is the first
    /// word of the left column and belongs over it, so it starts at `origin`
    /// like every label below. Pinning it to `pad` instead would leave it
    /// behind whenever the columns are centred inside a wider window, and a
    /// heading hanging left of the column it heads reads as a misalignment
    /// even though nothing is clipped.
    ///
    /// The button keeps the window's right margin, which is `origin` by
    /// symmetry, so it lines up with the right edge of the value column below
    /// rather than with the values themselves.
    fn draw_title_button(
        &self,
        y: i32,
        body: i32,
        title: &str,
        button: &str,
        id: HotspotId,
        acc: &mut Accumulator<'_>,
    ) {
        // The button is the row: it fills the body the row was sized
        // for, rather than being given a height of its own here.
        // Fitted to its own caption, floored at the minimum. Settings
        // stands alone in the corner with nothing to line up with, so
        // it sizes to its text rather than sharing the action width.
        let bw = button_width_for(button, self.btn_min, self.spacing, |s| {
            text_width(self.canvas, s, self.script)
        });
        let brect = Rect {
            left: self.width - self.origin - bw,
            top: y,
            right: self.width - self.origin,
            bottom: y + body,
        };
        let text_top = y + (body - self.line) / 2;

        let _bold = Selection::shared(self.canvas, self.font_bold.into());
        draw_text(
            self.canvas,
            title,
            Rect {
                left: self.origin,
                top: text_top,
                right: brect.left - self.spacing,
                bottom: text_top + self.line,
            },
            self.theme.colors().text_primary,
            DT_LEFT,
            self.theme.colors().background,
            self.script,
        );

        let fill = button_fill(self.hovered_button, id, &self.theme);
        fill_bordered(
            self.canvas,
            &brect,
            fill,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );
        draw_text(
            self.canvas,
            button,
            brect,
            self.theme.colors().text_primary,
            DT_CENTER,
            fill,
            self.script,
        );
        draw_focus_ring(
            self.canvas,
            &brect,
            self.focus == Some(id),
            self.ring_visible,
            &self.theme,
        );
        acc.painted.hotspots.push(Hotspot {
            rect: brect,
            id,
            is_button: true,
        });
    }

    /// Paints an open dropdown list and registers a hotspot per visible option.
    ///
    /// Where the parts land is [`ListLayout`]'s answer, not this function's:
    /// which way the list hangs, how tall it is and which pixels each row owns
    /// are arithmetic over the anchor and two counts, and stating them here as
    /// well would be the same layout written in two places. The window is never
    /// resized to fit, though, and that is this function's business: a dialog
    /// that grew and shrank as a list opened and closed would move every control
    /// under the pointer.
    ///
    /// Returns where it drew: the bounding rectangle and the scrollbar geometry,
    /// or `None` for the latter when the list fits and has none. Returned rather
    /// than written into `state` on the way past, so the record of what is on
    /// screen is assembled in one place by the caller — see [`PaintedList`].
    ///
    /// [`paint`] selects the interface font into the frame's canvas before any
    /// row is drawn, and this draws into that same back buffer: the list lands
    /// over the rows beneath it, within one frame, rather than into a context
    /// of its own that would have to be composited afterwards.
    fn paint_dropdown_list(
        &self,
        hotspots: &mut Vec<Hotspot>,
        list: &DropdownState,
        anchor: Rect,
        window_height: i32,
    ) -> (Rect, Option<ScrollbarGeometry>) {
        let item_h = self.line + self.spacing / 2;
        let total = list.options.len();

        let window = ListWindow::of(list.options, list.scroll);
        let visible = window.shown.len();
        if visible == 0 {
            // A dropdown with no options at all. Nothing is drawn, so the anchor is
            // the honest answer for where it is: an empty rectangle would be a
            // region no click can land in, reported as though it had been painted.
            return (anchor, None);
        }
        let scroll = window.first;
        let layout = ListLayout::of(anchor, window_height, item_h, self.line, total, visible);
        let (frame, bar, bounds) = (layout.frame, layout.bar, layout.bounds());

        // Opaque fill: the list sits over already-painted rows, and anything
        // showing through would read as text overlapping text.
        fill_bordered(
            self.canvas,
            &frame,
            self.theme.colors().surface,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );

        // Registered *before* the options, not after.
        //
        // This catches clicks that land on the list but on no option — a disabled
        // row, the scrollbar, the border — and closes it. It has to go first
        // because the hit test runs in reverse: last pushed is checked first, so
        // a frame pushed after the options would shadow every one of them, and
        // the list would open, scroll, and refuse to be clicked. That is exactly
        // the bug this ordering fixes, and the reversal is easy to forget when
        // reading the pushes in source order.
        hotspots.push(Hotspot {
            rect: frame,
            id: list.id,
            is_button: false,
        });

        for (slot, item) in window.shown.iter().enumerate() {
            // The index into the full list, wanted for the selection and
            // highlight comparisons and for the option's hotspot id. It names
            // an option that is already in hand rather than fetching one, so
            // there is nothing here for it to be wrong about.
            let index = scroll + slot;
            let row = layout.option(slot);

            let (bg, fg) = match OptionRole::of(index, list.selected, list.highlighted) {
                OptionRole::Highlighted => {
                    (self.theme.colors().accent, self.theme.colors().background)
                }
                OptionRole::Selected => {
                    (self.theme.colors().background, self.theme.colors().accent)
                }
                OptionRole::Plain => (
                    self.theme.colors().surface,
                    self.theme.colors().text_primary,
                ),
            };
            if bg != self.theme.colors().surface {
                fill_solid(self.canvas, &row, bg);
            }
            draw_item(
                self.canvas,
                item,
                Rect {
                    left: row.left + self.spacing,
                    right: row.right - self.spacing,
                    ..row
                },
                fg,
                bg,
                self.script,
            );

            hotspots.push(Hotspot {
                rect: row,
                id: list.id.option(index),
                is_button: false,
            });
        }

        // The scrollbar: two stepper buttons, a track, and a thumb that can be
        // dragged. Painted after the options so it sits above them, and its
        // hotspots are pushed last so the reverse hit test finds them first —
        // the thumb overlaps the track, and the track overlaps the frame.
        //
        // The bar it replaces was a bare proportional mark with no hit target at
        // all, which made the wheel the only way to move the list. That is fine
        // until someone has no wheel, and it is not what any other list on this
        // platform does: buttons at the ends, a page jump on the track, and a
        // draggable thumb are close enough to universal to count as the expected
        // behaviour rather than one option among several.
        if let Some(sb) = ScrollbarGeometry::layout(frame, bar, total, visible, scroll) {
            // The track is drawn one step darker than the list surface so the
            // thumb has something to travel against. Without it the thumb reads
            // as a floating mark rather than as a control with a range.
            fill_solid(self.canvas, &sb.bar, self.theme.colors().background);

            for (rect, up, id) in [
                (sb.up, true, HOT_SCROLL_UP),
                (sb.down, false, HOT_SCROLL_DOWN),
            ] {
                fill_solid(self.canvas, &rect, self.theme.colors().surface);
                draw_stepper(self.canvas, &rect, up, self.theme.colors().text_secondary);
                hotspots.push(Hotspot {
                    rect,
                    id,
                    is_button: false,
                });
            }

            // The track halves either side of the thumb, each a page jump. Pushed
            // before the thumb so the thumb — drawn on top of them — is also hit
            // first, which is what makes a click on it start a drag rather than
            // paging the list out from under the pointer.
            hotspots.push(Hotspot {
                rect: Rect {
                    bottom: sb.thumb.top,
                    ..sb.track
                },
                id: HOT_SCROLL_PAGE_UP,
                is_button: false,
            });
            hotspots.push(Hotspot {
                rect: Rect {
                    top: sb.thumb.bottom,
                    ..sb.track
                },
                id: HOT_SCROLL_PAGE_DOWN,
                is_button: false,
            });

            fill_solid(self.canvas, &sb.thumb, self.theme.colors().text_secondary);
            hotspots.push(Hotspot {
                rect: sb.thumb,
                id: HOT_SCROLL_THUMB,
                is_button: false,
            });

            // Reported back so a drag can be resolved without rebuilding the layout
            // from the row list on every mouse move — and, more to the point, so
            // it is resolved against the geometry that was actually painted.
            (bounds, Some(sb))
        } else {
            (bounds, None)
        }
    }

    /// A section heading, in the accent colour and the bold face.
    fn draw_header(&self, y: i32, text: &str) {
        let _bold = Selection::shared(self.canvas, self.font_bold.into());
        draw_text(
            self.canvas,
            text,
            Rect {
                left: self.origin,
                top: y,
                right: self.width - self.origin,
                bottom: y + self.line,
            },
            self.theme.colors().accent,
            DT_LEFT,
            self.theme.colors().background,
            self.script,
        );
    }

    /// A label in the left column and its value in the right one.
    fn draw_pair(&self, y: i32, label: &str, value: &str, color: Color) {
        draw_label(
            self.canvas,
            label,
            label_rect(self.origin, y, self.label_w, self.line),
            &self.theme,
            self.script,
        );
        draw_text(
            self.canvas,
            value,
            Rect {
                left: self.origin + self.label_w,
                top: y,
                right: self.width - self.origin,
                bottom: y + self.line,
            },
            color,
            DT_LEFT,
            self.theme.colors().background,
            self.script,
        );
    }

    /// Free-flowing text across the block `body` was measured for.
    fn draw_wrapped_block(&self, y: i32, body: i32, text: &str, color: Color) {
        // The block is exactly the height `DrawTextW` will consume,
        // because that is what measured it — the same call, on this
        // same device context, with `DT_CALCRECT`, the same flags and
        // the same wrap width. `content_size` sized the window from the
        // same number, so the row below lands where the window was
        // sized for it, and the text is neither clipped short nor
        // floating in reserved space.
        let rect = Rect {
            left: self.origin,
            top: y,
            right: self.origin + self.text_col,
            bottom: y + body,
        };
        draw_wrapped(
            self.canvas,
            text,
            rect,
            color,
            self.theme.colors().background,
        );
    }

    /// One line of full-width coloured text, drawn left-aligned like a label.
    ///
    /// No wrapping and no width-dependent height — see the `Notice` variant's
    /// own note for why the interval warning is this rather than a `Wrapped`.
    fn draw_notice(&self, y: i32, text: &str, color: Color) {
        draw_text(
            self.canvas,
            text,
            Rect {
                left: self.origin,
                top: y,
                right: self.width - self.origin,
                bottom: y + self.line,
            },
            color,
            DT_LEFT,
            self.theme.colors().background,
            self.script,
        );
    }

    /// Label, value, and a button placed immediately after the value.
    fn draw_labeled_button(&self, y: i32, row: &LabeledButtonRow<'_>, acc: &mut Accumulator<'_>) {
        let &LabeledButtonRow {
            label,
            value,
            color,
            button,
            id,
            enabled,
            is_button,
        } = row;
        // The row occupies exactly one text line, like every other
        // metric. It used to advance by `line + spacing`, which made
        // the buzzer sit in a band visibly taller than the rows around
        // it — a gap the reader attributes to a missing value rather
        // than to the control that caused it.
        //
        // The button is still taller than the text so it reads as
        // pressable, but it is centred on the line and overhangs it
        // symmetrically instead of pushing the row apart.
        let bh = self.line + self.spacing / 2;
        // A fixed width, not one fitted to the caption: the buzzer and
        // the self-test are the two buttons in this column, and a fixed
        // width is what keeps them the same size on their two rows
        // instead of each sizing to its own word. Fixed across time as
        // well as across the two rows — it is measured over every
        // caption either button can take in this language, so the
        // buzzer's verb alternating between two words of different
        // length does not resize the control under the pointer.
        let bw = self.act_btn;

        // At the right edge of the value column, which is the right
        // margin of the window. The column was measured to hold this
        // row's value and this button side by side, so this is the
        // position that measurement paid for — and it puts the button
        // on the same vertical edge as Settings in the title row.
        let bx = self.width - self.origin - bw;
        let brect = Rect {
            left: bx,
            top: y + (self.line - bh) / 2,
            right: bx + bw,
            bottom: y + (self.line - bh) / 2 + bh,
        };

        draw_label(
            self.canvas,
            label,
            label_rect(self.origin, y, self.label_w, self.line),
            &self.theme,
            self.script,
        );
        draw_text(
            self.canvas,
            value,
            Rect {
                left: self.origin + self.label_w,
                top: y,
                right: brect.left - self.spacing,
                bottom: y + self.line,
            },
            color,
            DT_LEFT,
            self.theme.colors().background,
            self.script,
        );
        // A disabled button is drawn in the secondary colour and
        // registers no hotspot. The border and fill are the same so
        // the control keeps its shape — only the greyed caption and
        // the dead click tell the user it is inert.
        let caption_colour = if enabled {
            self.theme.colors().text_primary
        } else {
            self.theme.colors().text_secondary
        };
        // A disabled button never registers a hotspot, so it can never
        // be the hovered one; `button_fill` returns its resting surface
        // for it regardless.
        let fill = if enabled {
            button_fill(self.hovered_button, id, &self.theme)
        } else {
            self.theme.colors().surface
        };
        fill_bordered(
            self.canvas,
            &brect,
            fill,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );
        draw_text(
            self.canvas,
            button,
            brect,
            caption_colour,
            DT_CENTER,
            fill,
            self.script,
        );
        if enabled {
            draw_focus_ring(
                self.canvas,
                &brect,
                self.focus == Some(id),
                self.ring_visible,
                &self.theme,
            );
            acc.painted.hotspots.push(Hotspot {
                rect: brect,
                id,
                is_button,
            });
        }
    }

    /// The closed control: label, current option, and the disclosure arrow.
    ///
    /// The list itself is not drawn here. An open one is recorded on `acc` and
    /// painted after every row, so it lands on top of whatever follows instead
    /// of under it.
    fn draw_dropdown(&self, y: i32, row: &DropdownRow<'_>, acc: &mut Accumulator<'_>) {
        let &DropdownRow {
            label,
            options,
            selected,
            open,
            id,
            is_button,
        } = row;
        draw_label(
            self.canvas,
            label,
            label_rect(self.origin, y, self.label_w, self.line),
            &self.theme,
            self.script,
        );
        let vr = Rect {
            left: self.origin + self.label_w,
            top: y,
            right: self.width - self.origin,
            bottom: y + self.line,
        };
        fill_bordered(
            self.canvas,
            &vr,
            self.theme.colors().surface,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );

        // Room for the arrow, so a long option cannot run under it.
        let arrow_w = self.line;
        let mut tr = vr;
        tr.left += self.spacing;
        tr.right -= arrow_w;
        if let Some(item) = options.get(selected) {
            draw_item(
                self.canvas,
                item,
                tr,
                self.theme.colors().accent,
                self.theme.colors().surface,
                self.script,
            );
        }
        draw_arrow(
            self.canvas,
            &vr,
            arrow_w,
            open,
            self.theme.colors().text_secondary,
        );
        draw_focus_ring(
            self.canvas,
            &vr,
            self.focus == Some(id),
            self.ring_visible,
            &self.theme,
        );

        acc.painted.hotspots.push(Hotspot {
            rect: vr,
            id,
            is_button,
        });
        if open {
            acc.overlay = Some((id, vr));
        }
    }

    /// An editable text field: its label, the value box, the selection band
    /// under the glyphs, and the caret.
    fn draw_field(&self, y: i32, row: &FieldRow<'_>, acc: &mut Accumulator<'_>) {
        let &FieldRow {
            label,
            value,
            caret,
            sel_start,
            sel_end,
            id,
            is_button,
        } = row;
        // Whether this field is the one the keyboard is aimed at. The
        // window is the sole authority on that, for every kind of
        // control alike, so the row does not carry the answer.
        let focused = self.focus == Some(id);
        draw_label(
            self.canvas,
            label,
            label_rect(self.origin, y, self.label_w, self.line),
            &self.theme,
            self.script,
        );
        let fr = Rect {
            left: self.origin + self.label_w,
            top: y,
            right: self.width - self.origin,
            bottom: y + self.line,
        };
        // A focused field takes a fill between the resting surface and
        // a hovered button's, so the window signals it is waiting for
        // input; unfocused, it keeps the plain surface. The selection
        // band and caret sit on top of this either way.
        let field_fill = if focused {
            self.theme.colors().field_focus
        } else {
            self.theme.colors().surface
        };
        fill_bordered(
            self.canvas,
            &fr,
            field_fill,
            self.theme.colors().border,
            self.theme.colors().border_soft,
            self.theme.metrics().corner_radius,
        );

        let text_left = fr.left + self.spacing;

        // Where every character of the value ends, measured once.
        //
        // The selection band and the caret are two edges of the same
        // ruler and used to be measured by two different routines —
        // each building a prefix `String` and asking for its width.
        // Two routes to one number is two chances to disagree, and a
        // disagreement here is a highlight that does not line up with
        // the caret inside it. It is also the measurement the *click*
        // that placed the caret already used, so all three now come
        // from one place.
        let ends = caret_offsets(self.canvas, value);
        // Character index to pixel offset. Index 0 is the start of the
        // text and has no entry, because `ends` records where each
        // character *ends*.
        let edge = |i: usize| {
            i.checked_sub(1)
                .and_then(|e| ends.get(e).copied())
                .unwrap_or(0)
        };

        // Selection band, drawn under the glyphs it covers.
        if focused && sel_end > sel_start {
            let x0 = text_left + edge(sel_start);
            let x1 = text_left + edge(sel_end);
            let sel = Rect {
                left: x0,
                top: fr.top + 2,
                right: x1,
                bottom: fr.bottom - 2,
            };
            if let Some(brush) = Brush::solid(self.theme.colors().accent.over(field_fill)) {
                // SAFETY: `self.canvas` is live, `sel` is a live
                // local, and the brush outlives the call.
                unsafe { FillRect(self.canvas.raw(), &sel.into(), brush.raw()) };
            }
        }

        let mut tr = fr;
        tr.left = text_left;
        draw_text(
            self.canvas,
            value,
            tr,
            self.theme.colors().text_primary,
            DT_LEFT,
            field_fill,
            self.script,
        );

        // Caret: a one-pixel vertical line at the caret's character
        // position, shown only in the visible phase of the blink so
        // it flashes like every other text caret. The blink is driven
        // by a timer that runs solely while a field has focus (see
        // WM_SETFOCUS / WM_KILLFOCUS handling), so an idle dialog with
        // nothing focused wakes the loop for nothing.
        if focused && self.caret_visible {
            let cx = text_left + edge(caret);
            if let Some(pen) = Pen::hairline(self.theme.colors().text_primary.over(field_fill)) {
                let _selected = Selection::new(self.canvas, &pen);
                // SAFETY: `self.canvas` is live; the pen is selected
                // by the guard above and the coordinates are integers.
                let _ = unsafe {
                    windows::Win32::Graphics::Gdi::MoveToEx(self.canvas.raw(), cx, fr.top + 3, None)
                };
                // SAFETY: as above.
                let _ = unsafe {
                    windows::Win32::Graphics::Gdi::LineTo(self.canvas.raw(), cx, fr.bottom - 3)
                };
            }
        }

        // Record the value box so the blink timer can invalidate just
        // this rectangle rather than the whole window twice a second.
        if focused {
            acc.painted.caret_rect = Some(fr);
        }

        // And the ruler, for the click that will place the caret. Moved, not
        // measured a second time: this is the same `ends` the band and the
        // caret above were placed with, so a click resolves against the text
        // that is on screen and against the widths it was actually drawn at.
        acc.painted.fields.push(PaintedField {
            id,
            text_left,
            ends,
        });

        draw_focus_ring(self.canvas, &fr, focused, self.ring_visible, &self.theme);

        acc.painted.hotspots.push(Hotspot {
            rect: fr,
            id,
            is_button,
        });
    }

    /// Draws one row between its lead-in and the cursor's next step.
    ///
    /// Dispatch and nothing else: one line per variant, in the order [`Row`]
    /// declares them, so a variant that has lost its drawing is visible by
    /// eye. "What kinds of row are there" and "how is each one drawn" are two
    /// questions, and until the arms were lifted into methods the first could
    /// not be answered without reading every answer to the second.
    ///
    /// Every arm draws and advances nothing: the cursor moves by `Row::height`
    /// in the caller, which is the number the window was sized from, so no arm
    /// can invent a height of its own.
    ///
    /// `body` reaches only the three variants whose height is not one text
    /// line. A method that is not given it cannot draw a row of a height the
    /// measurement did not reserve.
    fn draw_row(&self, row: &Row, y: i32, body: i32, acc: &mut Accumulator<'_>) {
        // Asked once, of the row, and handed to the three methods that record
        // a hotspot for a raised control. Answering it inside those methods
        // would put the classification of a button in three places instead of
        // in the `Row` variant, which is the one place that can be right —
        // see [`Hotspot::is_button`].
        let is_button = row.is_button();
        match row {
            Row::Header(text) => self.draw_header(y, text),
            Row::Pair {
                label,
                value,
                color,
            } => self.draw_pair(y, label, value, *color),
            Row::Wrapped { text, color } => self.draw_wrapped_block(y, body, text, *color),
            Row::Notice { text, color } => self.draw_notice(y, text, *color),
            Row::LabeledButton {
                label,
                value,
                color,
                button,
                id,
                enabled,
            } => self.draw_labeled_button(
                y,
                &LabeledButtonRow {
                    label,
                    value,
                    color: *color,
                    button,
                    id: *id,
                    enabled: *enabled,
                    is_button,
                },
                acc,
            ),
            Row::TitleButton { title, button, id } => {
                self.draw_title_button(y, body, title, button, *id, acc);
            }
            Row::Checkbox {
                label,
                checked,
                depth,
                id,
            } => self.draw_checkbox(y, label, *checked, *depth, *id, acc),
            Row::Dropdown {
                label,
                options,
                selected,
                open,
                id,
                ..
            } => self.draw_dropdown(
                y,
                &DropdownRow {
                    label,
                    options,
                    selected: *selected,
                    open: *open,
                    id: *id,
                    is_button,
                },
                acc,
            ),
            Row::Field {
                label,
                value,
                caret,
                sel_start,
                sel_end,
                id,
            } => self.draw_field(
                y,
                &FieldRow {
                    label,
                    value,
                    caret: *caret,
                    sel_start: *sel_start,
                    sel_end: *sel_end,
                    id: *id,
                    is_button,
                },
                acc,
            ),
            Row::Buttons {
                left,
                left_id,
                right,
                right_id,
            } => self.draw_buttons(y, body, (left, *left_id), (right, *right_id), acc),
            Row::Space(_) => {}
        }
    }
}

/// What a frame accumulates while its rows are drawn.
///
/// The measuring cache belongs here rather than in [`Frame`] because it is the
/// one value in the set that a row *changes*, and the split says which is
/// which: a `&Frame` is what the row was given, a `&mut Accumulator` is what
/// it leaves behind.
///
/// [`Painted`] is carried inside rather than duplicated: what the rows record
/// *is* the frame record, and building a second set of fields here to copy
/// across afterwards is how the copy comes to differ from the original. The
/// two fields beside it are the ones the window has no use for once the pass
/// is over.
struct Accumulator<'a> {
    metrics: GdiMetrics<'a>,
    /// The frame being assembled, handed to the window whole at the end.
    painted: Painted,
    /// Whether a section heading has already been drawn. The lead-in above a
    /// heading is only correct between sections; the first has nothing above
    /// it to be separated from.
    header_seen: bool,
    /// Set by an open dropdown while the rows are drawn, consumed after the
    /// loop: the list must be painted last or the rows below would cover it,
    /// and its hotspots registered last so the reversed hit test finds them
    /// first.
    overlay: Option<(HotspotId, Rect)>,
}

/// Double-buffered paint: build the frame in memory, blit once.
///
/// The drawing itself is [`render`]. What is left here is the pair of writes
/// the window keeps from a pass — where the controls landed, and whether the
/// remembered hover still refers to anything — because a pass that produced no
/// frame must make neither, and that is a property of who may write rather
/// than of the order the statements happen to be in.
///
/// # No window handle
///
/// It takes a `client` rectangle rather than the `HWND` to measure one from,
/// and that is deliberate. This function runs **inside** the `with_state`
/// borrow, so a handle here is a way back to the state it is already holding —
/// a nested `with_state` would fail its `try_borrow_mut`, and the failure is
/// silent by design: no frame is drawn and a line nobody reads goes to the log.
///
/// Reading it today shows nothing does that. But the argument list is what
/// decides whether it stays true: with no handle to reach through, the
/// construction refuses the mistake instead of relying on nobody making it.
/// `hwnd` was used exactly once in this function, for `GetClientRect`, and the
/// caller can do that before it takes the borrow.
pub(super) fn paint(target: Canvas<'_>, client: Rect, dirty: Rect, state: &mut WindowState) {
    let previously_open = state.open_list();
    let painted = render(target, client, dirty, state);
    let opened = painted.as_ref().and_then(Painted::open_list);
    state.hovered = hover_across(previously_open, opened, state.hovered);
    state.painted = painted;
}

/// The hovered option that carries from one frame into the next.
///
/// An option index means nothing on its own — it is an offset into whichever
/// list is open — so it survives exactly as long as that list does. Carried
/// across a change of list it would suppress the first hover event in the new
/// one, and the option under the pointer would stay unlit until the pointer
/// moved again.
///
/// Both sides come from [`Painted::open_list`], so "the list the last frame
/// drew" and "the list this frame drew" are one question asked twice rather
/// than two questions that have to agree. The geometry needs no such rule: the
/// whole frame record is replaced on every pass.
fn hover_across(
    previous: Option<HotspotId>,
    opened: Option<HotspotId>,
    hovered: Option<usize>,
) -> Option<usize> {
    if previous == opened {
        hovered
    } else {
        None
    }
}

/// Draws one frame and reports where its controls landed.
///
/// `None` is a pass that drew nothing — a zero-sized client, or a back buffer
/// the system refused — and it is the return type that says so. This used to
/// be a statement: `state.painted = None` stood above the early returns below,
/// and "a pass that drew no frame records no geometry" held because of where
/// that line was written. Moving it past any one of them left the window
/// hit-testing a picture it was no longer showing, with nothing to say so.
/// Now the mutation does not exist to be caught: there is no line to move, and
/// no path out of here that can carry a frame record it did not build.
///
/// `state` is borrowed *shared*, which is the other half of the same idea.
/// The two writes this used to make are the caller's, so the compiler is what
/// keeps them there rather than a reader noticing.
fn render(target: Canvas<'_>, client: Rect, dirty: Rect, state: &WindowState) -> Option<Painted> {
    // Built once, from this window's own state and by the same function the
    // measurement used, then handed to every drawing call below.
    //
    // `draw_text` may reach `script_font` several calls down, for the non-Latin
    // scripts Segoe UI cannot draw, and that call needs *this* window's DPI and
    // *this* window's theme rather than whichever was last loaded anywhere in
    // the process: per-monitor awareness means the panel and the settings
    // dialog can legitimately be on two different monitors at the same moment,
    // and a repaint that draws at a size the layout was not measured at
    // produces rows that do not fit their own text. It used to arrive through a
    // thread-local this function wrote at exactly this point, which made the
    // correctness of every drawing helper depend on a write nobody in the call
    // chain mentioned.
    let script = super::script_of(&state.base_theme, state.family, state.dpi);

    let width = client.width();
    let height = client.height();
    if width == 0 || height == 0 {
        return None;
    }

    // The double buffer, as three guards rather than five hand-matched calls.
    // The failure path used to have to remember what had already been made —
    // a bitmap that could not be created had to delete the DC on its way out —
    // and every early return below this point would have had to remember the
    // same list. Now leaving the function is the release, which is why the two
    // that remain can be a bare `?`.
    let mem = MemDc::compatible(target)?;
    // Everything below draws here, not into `target`: the back buffer reaches
    // the window in one `BitBlt` at the end.
    let mem_dc = mem.canvas();

    // A `WM_PAINT` can arrive with an empty update region — an invalidation
    // that was already satisfied, or a rectangle that fell outside the client
    // area. Nothing such a pass draws can land anywhere, so the back buffer is
    // not allocated for it and the blit at the end is not made: a full-window
    // bitmap and a zero-sized `BitBlt` for a frame that has no pixels in it.
    //
    // The row loop still runs, and must: it is what builds the frame record,
    // and it measures text against `mem_dc`, so the memory DC is created either
    // way. Only the surface is conditional.
    let painting = dirty.right > dirty.left && dirty.bottom > dirty.top;
    let bitmap = if painting {
        Some(Bitmap::compatible(target, width, height)?)
    } else {
        None
    };
    let _bitmap_selected = bitmap.as_ref().map(|bitmap| Selection::new(mem_dc, bitmap));

    // Copied, not borrowed. `Theme` is `Copy` now that its geometry is integral
    // and its colours are grouped, so the copy costs nothing and the frame the
    // loop below is handed carries a theme rather than a borrow of the window.
    // Cloning the whole
    // `Theme` — which owns a `String` and an `IconSet` — heap-allocated on
    // *every* repaint, once a second while the panel is open, for data that is
    // pure `Copy`. This is the one thing `layout::Palette` was really for,
    // achieved without a second type that had to be kept in step by hand.
    let theme = state.theme;

    // Every drawing call below is clipped to the region Windows asked to have
    // repainted. Without this the double buffer had no clip at all: the whole
    // client was filled, every row drawn in full, and only the final blit was
    // bounded — by the window DC, which `BeginPaint` clips for us. So a
    // one-line scroll still redrew both columns, every border and every string
    // in the window, and the partial invalidation in `set_list_scroll` saved
    // nothing but the blit.
    //
    // The row loop still runs in full, and deliberately so: it is what builds
    // the frame record, and skipping rows outside the region would leave the
    // window unable to hit-test the parts it did not repaint. What the clip
    // removes is the drawing — GDI rejects output outside the region before it
    // rasterises anything, which is where the cost of a repaint actually is.
    // SAFETY: `mem_dc` proves the back buffer is live; the edges are integers.
    unsafe {
        IntersectClipRect(
            mem_dc.raw(),
            dirty.left,
            dirty.top,
            dirty.right,
            dirty.bottom,
        )
    };
    if let Some(bg) = Brush::solid(theme.colors().background) {
        // SAFETY: `mem_dc` is live, `client` is a live local, and the brush
        // outlives the call.
        unsafe { FillRect(mem_dc.raw(), &client.into(), bg.raw()) };
    }

    // SAFETY: `mem_dc` is live; the mode is a by-value constant.
    unsafe { SetBkMode(mem_dc.raw(), TRANSPARENT) };
    // The font belongs to a thread-local that outlives every window on the
    // thread, so the guard borrows the state it was read from: the point of the
    // borrow is that the selection cannot outlive what it selected, and here
    // that is trivially true rather than accidentally true.
    let _font_selected = Selection::shared(mem_dc, state.font.into());

    // Measured against the context being drawn into, so a wrapped row's block
    // is exactly the height the draw will fill.
    let metrics = GdiMetrics {
        canvas: mem_dc,
        font: state.font,
        bold: state.font_bold,
        script,
    };

    let pad = theme.metrics().padding;
    let spacing = theme.metrics().spacing;
    let line = theme.metrics().row_height;
    let btn_min = theme.metrics().button_min_width;

    // The columns this window was sized from, carried on the content rather
    // than measured again here. The scan behind them is by far the most
    // expensive thing a repaint could do — it crosses into GDI once per string,
    // and for the settings dialog that means all twenty-four language names on
    // top of every label and value — so the painter used to keep a cache of its
    // own, keyed on the same text fingerprint as the sizing pass's. Two caches
    // for one answer in two scopes; now there is the answer, and the painter
    // divides the window by the rule it was actually sized under rather than by
    // a second opinion that has to agree.
    let needs = state.content.columns;

    // Both columns are the same width, so the value column starts on the
    // window's centre line whatever the language. `origin` is the left edge
    // of the label column: it equals `pad` unless a full-width row made the
    // window wider than the columns needed, in which case the surplus is
    // split between the two margins instead of collecting on the right.
    let (origin, half) = column_origin(width, needs.half, &theme);
    let label_w = half;
    // The width a full-width row's text flows into, from the same function that
    // gave the window its height. Deriving it here a second time — as
    // `width - origin * 2`, which is what this used to be — is how a wrapped
    // row could be measured against one width and drawn into another.
    let text_col = text_column_width(width, needs, &theme);

    let frame = Frame {
        canvas: mem_dc,
        theme,
        script,
        width,
        line,
        spacing,
        origin,
        label_w,
        text_col,
        btn_min,
        act_btn: needs.action_button,
        focus: state.focus,
        ring_visible: state.focus_visible,
        hovered_button: state.hovered_button,
        font_bold: state.font_bold,
        caret_visible: state.caret_visible,
    };
    let mut acc = Accumulator {
        metrics,
        painted: Painted::default(),
        header_seen: false,
        overlay: None,
    };

    let mut y = pad;
    for row in &state.content.rows {
        // The row's two vertical terms, both from `Row`'s own table: the blank
        // above the content, and the content itself. The cursor advances by
        // their sum, which is exactly `Row::height` — the number the window was
        // sized from. The arms below draw between the two and advance nothing,
        // so no arm can invent a height of its own.
        let lead = row.lead_in(&state.theme, acc.header_seen);
        let body = row.body_height(&state.theme, frame.text_col, &mut acc.metrics);
        y += lead;
        frame.draw_row(row, y, body, &mut acc);
        // Exactly as `measure_with` tracks it: the flag turns after the row
        // that set it, so the first heading charges no lead-in and every
        // heading after it does.
        if matches!(row, Row::Header(_)) {
            acc.header_seen = true;
        }
        y += body;
    }
    // The open list, painted over everything already drawn. Deferred to here
    // rather than done inline so it lands above the rows that follow it, and
    // so its hotspots are the last pushed and therefore the first hit-tested.
    if let Some((id, anchor)) = acc.overlay {
        // The row is copied out before painting because the list is drawn from
        // it while `acc` is borrowed mutably to take its hotspots. Only the
        // five scalars are copied; the options are cloned once per repaint of
        // an open list, which happens on a keypress, not a poll.
        let list = state.content.rows.iter().find_map(|r| match r {
            Row::Dropdown {
                options,
                selected,
                highlighted,
                scroll,
                id: rid,
                ..
            } if *rid == id => Some((options.clone(), *selected, *highlighted, *scroll)),
            _ => None,
        });
        if let Some((options, selected, highlighted, scroll)) = list {
            let list = DropdownState {
                id,
                options: &options,
                selected,
                highlighted,
                scroll,
            };
            // The offset the painter will actually draw at, taken from the
            // same window it takes it from. Stored unclamped, an out-of-range
            // request would never compare equal to what is on screen and every
            // mouse move would repaint again — and clamped a second time here,
            // it would be two computations of one number.
            let scroll = ListWindow::of(&options, scroll).first;
            let (bounds, scrollbar) =
                frame.paint_dropdown_list(&mut acc.painted.hotspots, &list, anchor, height);
            acc.painted.list = Some(PaintedList {
                id,
                bounds,
                scroll,
                scrollbar,
            });
        }
    }

    // Only the dirty rectangle is transferred. The destination is already
    // clipped to it by `BeginPaint`, so blitting the whole client was never
    // *wrong* — it was simply asking the system to move an image it would then
    // throw most of away.
    if painting {
        // SAFETY: both contexts are live — `target` is the window's, `mem_dc`
        // the back buffer — and the bitmap is selected into the latter by a
        // guard that outlives this call.
        let _ = unsafe {
            BitBlt(
                target.raw(),
                dirty.left,
                dirty.top,
                dirty.right - dirty.left,
                dirty.bottom - dirty.top,
                Some(mem_dc.raw()),
                dirty.left,
                dirty.top,
                SRCCOPY,
            )
        };
    }

    // Everything above wrote into `acc`, so there is no window in which half of
    // one frame is recorded beside half of another: the record leaves here
    // whole or not at all.
    //
    // The font, the bitmap and the memory DC are released after it, by the
    // guards above going out of scope — the reverse of the order they were
    // taken, which is what the four calls this replaces spelled out by hand.
    Some(acc.painted)
}

/// The disclosure triangle on a dropdown, pointing down when closed and up
/// when open.
///
/// Drawn as a polygon rather than printed as a character: a glyph comes from
/// whichever font is selected, so its size and baseline shift with the font
/// and with DPI, and it drifts against the box it sits in. Three points do
/// the same job identically everywhere.
pub(super) fn draw_arrow(
    canvas: Canvas<'_>,
    box_rect: &Rect,
    width: i32,
    open: bool,
    color: Color,
) {
    let cx = box_rect.right - width / 2;
    let cy = (box_rect.top + box_rect.bottom) / 2;
    let w = (width / 5).max(2);
    let h = if open { -w } else { w };
    let pts = [
        POINT {
            x: cx - w,
            y: cy - h / 2,
        },
        POINT {
            x: cx + w,
            y: cy - h / 2,
        },
        POINT { x: cx, y: cy + h },
    ];
    let (Some(brush), Some(pen)) = (Brush::solid(color), Pen::hairline(color)) else {
        return;
    };
    // Both guards live to the end of the block, so the two objects are restored
    // and released in the reverse of the order they were selected — which is
    // what the four hand-written calls this replaces were doing, and what an
    // early return between any two of them would have stopped doing.
    let _fill = Selection::new(canvas, &brush);
    let _outline = Selection::new(canvas, &pen);
    // SAFETY: `canvas` proves the context is live, the brush and pen are
    // selected by the guards above, and `pts` is a slice read for its own
    // length.
    let _ = unsafe { Polygon(canvas.raw(), &pts) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hovered option survives exactly the frames that keep one list open.
    ///
    /// This is the rule that used to live as two lines in the middle of the
    /// paint pass, where the only way to check it was to read them. It is
    /// asserted by value in all four combinations, because the two that matter
    /// are opposites of each other: the same list open across a frame keeps the
    /// highlight, and anything else — a different list, one closing, one
    /// opening — drops it, since the index means nothing outside the list it
    /// indexes.
    #[test]
    fn the_hover_survives_only_while_one_list_stays_open() {
        let a = Some(HotspotId::new(7));
        let b = Some(HotspotId::new(8));

        assert_eq!(hover_across(a, a, Some(3)), Some(3));
        assert_eq!(hover_across(a, b, Some(3)), None);
        assert_eq!(hover_across(a, None, Some(3)), None);
        assert_eq!(hover_across(None, a, Some(3)), None);
        // No list either side: nothing changed, so nothing is cleared. A frame
        // that drew none is this case, and it leaves the value alone.
        assert_eq!(hover_across(None, None, Some(3)), Some(3));
        assert_eq!(hover_across(a, a, None), None);
    }

    /// The painter's cursor moves by the row table and by nothing else.
    ///
    /// Both readers of that table must agree, and for years they agreed only
    /// because someone kept two `match` statements in step by hand: the painter
    /// advanced by its own expression per arm, `measure_with` summed
    /// `Row::height`, and a row kind whose height changed in one place would
    /// have gone on being drawn at the other's. The painter now takes both
    /// terms — [`Row::lead_in`] and [`Row::body_height`] — from the same table,
    /// so the only two advances in the row loop are those.
    ///
    /// Checked structurally because [`render`] needs a device context and a
    /// live window: the property is about the shape of the code, and the shape
    /// is exactly what would rot when the next row kind is added.
    ///
    /// **Structural, and temporarily so.** It reads source text rather than
    /// running the code, because [`render`] needs a device context and a
    /// window. A
    /// text search cannot tell a correct advance from one in an arm that is
    /// never reached, so this is weaker than the property it stands for. The
    /// remedy is the one applied to `app::plan_emission`: lift the decision
    /// into a function that takes what it needs and returns what it decided,
    /// then assert on the value. Until that is done here, this is the only
    /// check there is.
    #[test]
    fn the_painter_advances_only_by_the_row_table() {
        let body = crate::testsupport::fn_body(include_str!("paint.rs"), "fn render(");
        let advances: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("y +="))
            .collect();
        assert_eq!(
            advances,
            ["y += lead;", "y += body;"],
            "a paint arm that advances the cursor itself is a second height \
             table, and the window is sized from the first"
        );
    }

    /// A dropdown's frame is hit-tested *after* its options, so a click on an
    /// option reaches the option.
    ///
    /// The painter pushes the frame first and the options after, because the
    /// hit test walks the list in reverse. Getting that backwards produced a
    /// list that opened, scrolled and highlighted correctly but could not be
    /// clicked — every option click was swallowed by the frame on top of it,
    /// which merely closed the list. No logic test could see it: the fault
    /// was entirely in the order two `push` calls appear in.
    #[test]
    fn an_option_outranks_the_frame_it_sits_in() {
        // Reproduces the painter's push order without a device context: frame
        // first, then one hotspot per option.
        let frame = Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let mut hotspots = vec![Hotspot {
            rect: frame,
            id: HotspotId::new(42),
            is_button: false,
        }];
        for i in 0..3usize {
            hotspots.push(Hotspot {
                rect: Rect {
                    left: 0,
                    top: 10 * i as i32,
                    right: 100,
                    bottom: 10 * (i as i32 + 1),
                },
                id: HotspotId::new(42).option(i),
                is_button: false,
            });
        }

        // The click path: reverse order, first match wins.
        let hit = |x: i32, y: i32| {
            hotspots
                .iter()
                .rev()
                .find(|h| {
                    x >= h.rect.left && x < h.rect.right && y >= h.rect.top && y < h.rect.bottom
                })
                .map(|h| h.id)
        };

        for i in 0..3usize {
            let id = hit(50, 10 * i as i32 + 5).expect("a hotspot covers this point");
            assert_eq!(
                id.split_option(),
                Some((HotspotId::new(42), i)),
                "click on option {i} resolved to {id:?}, not the option"
            );
        }
        // Below the options but inside the frame: closes the list.
        assert_eq!(hit(50, 95), Some(HotspotId::new(42)));
    }
}
