//! Procedural tray icon rendering.
//!
//! Depends only on `IconSpec`, never on the theme as a whole, so the renderer
//! stays independent of how themes are loaded. The shape is one drawing — a
//! shield with a bolt overlay — so it is not parameterised; only the palette in
//! `IconSpec` varies. Nothing is read from disk here: shipping .ico files would
//! break the single-file layout.

use crate::color::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IconSpec {
    pub(crate) fill: Color,
    pub(crate) stroke: Color,
}

/// The complete tray icon appearance: one spec per state and the application
/// mark. The body shape (a shield with a bolt overlay) is the same everywhere;
/// only the palette differs per state, so it is drawn unconditionally by the
/// renderer rather than selected by a field.
///
/// This lives in `icon.rs` rather than in `theme.rs` because it is the single
/// source shared by two consumers: the theme system at run time and the build
/// script that draws the exe icon (`build.rs` includes this module by
/// `#[path]`). Keeping the values here means the exe, the window title bars,
/// Task Manager and all four tray states are drawn from one definition and can
/// never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IconSet {
    /// The application mark: exe, window title bars, Task Manager.
    ///
    /// Separate from the four tray states and deliberately not one of them.
    /// A file on disk and a window on screen are never "on battery", so the
    /// application icon does not change with power state; it is the utility's
    /// identity. Only the tray reports state.
    pub(crate) app: IconSpec,
    pub(crate) normal: IconSpec,
    pub(crate) on_battery: IconSpec,
    pub(crate) critical: IconSpec,
    pub(crate) disconnected: IconSpec,
}

/// The one shipped icon set, shared by every theme.
///
/// Icons belong to the base theme and are the same for the dark and the light
/// theme alike: the tray and the exe live outside the panel window, so the
/// colour of the window background has no bearing on them. The shield-with-bolt
/// shape is one drawing everywhere; only the palette differs per tray state,
/// and the application mark is blue regardless of power state.
pub(crate) const ICONS: IconSet = IconSet {
    // Blue is the utility's brand colour and does not depend on power state:
    // a file on disk and a window on screen are never "on battery".
    app: IconSpec {
        fill: Color::from_rgb(0x0A, 0x84, 0xFF),
        stroke: Color::from_rgb(0x4D, 0xA6, 0xFF),
    },
    // Each state is two-tone: a light stroke on the outline, a darker interior
    // of the same hue on the shield.
    normal: IconSpec {
        fill: Color::from_rgb(0x34, 0xC7, 0x59),
        stroke: Color::from_rgb(0x5B, 0xE0, 0x7E),
    },
    on_battery: IconSpec {
        fill: Color::from_rgb(0xFF, 0x9F, 0x0A),
        stroke: Color::from_rgb(0xFF, 0xC6, 0x5C),
    },
    critical: IconSpec {
        fill: Color::from_rgb(0xFF, 0x3B, 0x30),
        stroke: Color::from_rgb(0xFF, 0x7B, 0x73),
    },
    disconnected: IconSpec {
        fill: Color::from_rgb(0x6E, 0x6E, 0x6E),
        stroke: Color::from_rgb(0x9E, 0x9E, 0x9E),
    },
};

/// Smallest icon this renders. Below it the shield loses the bolt entirely,
/// and a mark that is a coloured blob is worse than a smaller one.
pub(crate) const MIN_ICON: u32 = 8;

/// Largest icon this renders.
///
/// 256 is the largest size a Windows icon resource carries, so nothing above
/// it can reach a window, a tray or a `.ico` group. The number is here rather
/// than beside either size table because there are two of those — the resource
/// sizes in `build.rs` and the same list in `ui::appicon` — and this file is
/// the one both of them call into.
///
/// It is a domain, not a guess at what callers will pass. `render` allocates
/// `(size * 3)^2 * 4` bytes and indexes into that buffer; at 256 the product
/// is 2.4 MB and every intermediate fits a `u32` with three orders of
/// magnitude to spare, so the arithmetic below cannot wrap whatever the caller
/// asks for. Without a stated ceiling the release profile would wrap silently
/// and the index that follows would abort the process.
pub(crate) const MAX_ICON: u32 = 256;

/// Renders one icon to an RGBA buffer of `size` x `size`.
///
/// Supersampled 3x and box-filtered down: the glyphs are drawn from analytic
/// shapes, so aliasing at 16px would otherwise be severe.
///
/// `size` is clamped to [`MIN_ICON`]..=[`MAX_ICON`] rather than asserted
/// against: this is a drawing routine, and a caller asking for an
/// unrepresentable size should get the nearest representable mark instead of
/// killing a process whose job is to keep running.
#[must_use]
pub(crate) fn render(spec: IconSpec, size: u32) -> Vec<u8> {
    const SS: u32 = 3;
    let size = size.clamp(MIN_ICON, MAX_ICON);
    let hi = size * SS;
    let mut hires = vec![0u8; (hi * hi * 4) as usize];

    // The buffer is walked rather than addressed. Each coordinate is paired
    // with the bytes it names — rows with rows, pixels with pixels — so the
    // offset `(y * hi + x) * 4` is not written down anywhere and cannot be off
    // by one. `chunks_exact_mut` also states the pixel's width once: what the
    // sample is copied into is four bytes because the chunk is, not because a
    // reader kept three `+ 1`s in order.
    let stride = (hi * 4) as usize;
    for (y, row) in (0..hi).zip(hires.chunks_exact_mut(stride)) {
        for (x, pixel) in (0..hi).zip(row.chunks_exact_mut(4)) {
            let u = cell_centre(x, hi);
            let v = cell_centre(y, hi);
            if let Some(c) = sample(u, v, spec) {
                pixel.copy_from_slice(&[c.r, c.g, c.b, c.a]);
            }
        }
    }

    downsample(&hires, hi, size, SS)
}

/// The centre of sample cell `i` of `n`, in normalised coordinates.
///
/// The `+ 0.5` is the whole point. Sampling at `i / n` reads the cell's
/// top-left *corner*, which puts the grid half a cell off in both axes and,
/// worse, makes it asymmetric: the first sample sits exactly on 0 while the
/// last sits on `(n - 1) / n`, never on 1. The shape is then measured against
/// a ruler that is short at one end, so a boundary that should fall equally on
/// both sides of the canvas falls differently on each.
///
/// That was visible. The shield spans 0.06..0.94, which at 16 px is
/// 0.96..15.04 — very nearly the pixel boundaries 1 and 15, so both edge
/// columns should be all but empty. With corner sampling the left column came
/// out at zero coverage and the right at a third, giving a full-height
/// one-third-alpha ghost column down the right side and nothing down the left.
/// On a light taskbar that reads as a shadow, and it is the artefact this
/// helper exists to prevent.
fn cell_centre(i: u32, n: u32) -> f32 {
    (i as f32 + 0.5) / n as f32
}

/// Returns the colour at normalised coordinates, or None for transparent.
fn sample(u: f32, v: f32, spec: IconSpec) -> Option<Color> {
    // The bolt modifier draws on top of the body.
    if let Some(c) = sample_bolt(u, v, spec) {
        return Some(c);
    }
    sample_shield(u, v, spec)
}

fn sample_shield(u: f32, v: f32, spec: IconSpec) -> Option<Color> {
    // Outline width, in glyph-relative units.
    //
    // 0.09 was too heavy for the shield: the body is only 0.34 half-width and
    // tapers to a point, so a 0.09 rim left the dimmed interior at under a
    // third of the opaque pixels. The icon then read as a flat light shape
    // with a thin dark core rather than the two-tone mark it is meant to be.
    // The value is a sixteenth, so the outline is exactly one pixel at 16 px —
    // the size the notification area asks for at 100% scaling, and the size
    // every other alignment below is chosen for. A stroke that lands on 0.88 px
    // instead is antialiased across two columns however correct the sampling
    // is, which is a soft outline by construction. `two_tone_balance_is_visible`
    // pins the resulting ratio.
    const STROKE_W: f32 = 0.0625;
    // Rounded top, tapering to a point at the bottom.
    //
    // The half-width and the vertical span are deliberately close to the edges
    // of the canvas. They used to be 0.34 and 0.12..0.94, which drew the shield
    // across only about 68% of the width and left a wide empty margin down each
    // side. Windows scales the whole bitmap into the tray slot, so that margin
    // is not free space around the icon — it is wasted resolution, and the mark
    // reads as small next to neighbours that fill their slot.
    //
    // Every bound is a multiple of a sixteenth, and that is not cosmetic. At
    // 16 px these land on whole pixel boundaries — left edge at 1, right edge
    // at 15, top at 1 — so the straight parts of the outline are drawn by full
    // pixels rather than by two half-covered ones. The previous values
    // (0.44 / 0.05 / 0.97) put those same edges at 0.96, 0.8 and 15.04:
    // within a fifth of a pixel of the grid, close enough to look intended and
    // far enough to be antialiased into a soft band on every straight edge.
    // 32, 48 and 64 are multiples of 16 and inherit the alignment; 20 and 24
    // do not, and there the edges are antialiased — but *symmetrically*, which
    // is what the eye reads as a smooth edge rather than as a skew.
    //
    // `glyph_fills_the_canvas` pins the size: at tray sizes a couple of wasted
    // pixels per side is a visible fraction of a 16 px icon.
    const HALF_W: f32 = 0.4375;
    const TOP: f32 = 0.0625;
    const BOTTOM: f32 = 0.9375;
    const SHOULDER: f32 = 0.50;

    let cx = 0.5;
    let half_w = if v < SHOULDER {
        HALF_W
    } else {
        HALF_W * (1.0 - (v - SHOULDER) / (BOTTOM - SHOULDER)).max(0.0)
    };
    if !(TOP..=BOTTOM).contains(&v) {
        return None;
    }
    let d = (u - cx).abs();
    if d > half_w {
        return None;
    }
    Some(if d > half_w - STROKE_W || v < TOP + STROKE_W {
        spec.stroke
    } else {
        dim(spec.fill)
    })
}

fn sample_bolt(u: f32, v: f32, spec: IconSpec) -> Option<Color> {
    // The bolt sits inside the body, where alpha is already opaque. Drawing it
    // in colour alone would leave the silhouette unchanged and make the states
    // indistinguishable in monochrome, so it is ringed by a transparent gap
    // that cuts the outline.
    //
    // Lightning bolt: two mirrored wedges meeting at the centre.
    let wedge = |half: f32| -> bool {
        let upper = v > 0.30 && v < 0.56 && (u - (0.62 - (v - 0.30) * 0.9)).abs() < half;
        let lower = (0.56..0.82).contains(&v) && (u - (0.52 - (v - 0.56) * 0.9)).abs() < half;
        upper || lower
    };
    if wedge(0.10) {
        Some(spec.stroke)
    } else if wedge(0.17) {
        Some(Color::TRANSPARENT)
    } else {
        None
    }
}

fn dim(c: Color) -> Color {
    Color::from_rgba(
        (u16::from(c.r) * 2 / 5) as u8,
        (u16::from(c.g) * 2 / 5) as u8,
        (u16::from(c.b) * 2 / 5) as u8,
        c.a,
    )
}

/// Box filter from the supersampled buffer down to the target size.
fn downsample(src: &[u8], src_size: u32, dst_size: u32, factor: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dst_size * dst_size * 4) as usize];
    let per_box = factor * factor;
    let src_stride = (src_size * 4) as usize;
    let dst_stride = (dst_size * 4) as usize;
    let factor = factor as usize;

    // Destination rows and the source rows that feed them are paired by the
    // walk, and so are the pixels within them. What used to be four
    // multiplications reconstructing an offset — one of which had to know the
    // supersampling factor, one the source stride and one the destination's —
    // is now `skip` and `take` over the very buffers those offsets pointed
    // into, and none of the three strides appears twice.
    let mut source_rows = src.chunks_exact(src_stride);
    for out_row in out.chunks_exact_mut(dst_stride) {
        // The `factor` source rows this destination row averages, consumed
        // from the walk so the next destination row continues where this one
        // stopped rather than computing where to resume.
        let block: Vec<&[u8]> = source_rows.by_ref().take(factor).collect();
        for (x, out_pixel) in out_row.chunks_exact_mut(4).enumerate() {
            let (mut red, mut green, mut blue, mut alpha_sum) = (0u32, 0u32, 0u32, 0u32);
            for row in &block {
                for pixel in row.chunks_exact(4).skip(x * factor).take(factor) {
                    // A chunk of four bytes is a pixel, and `try_from` is how
                    // that is said to the compiler rather than to the reader:
                    // the channels are named once and the `Err` arm is the
                    // conversion's other half, not a guard — `chunks_exact`
                    // yields nothing else.
                    let Ok([r, g, b, a]) = <[u8; 4]>::try_from(pixel) else {
                        continue;
                    };
                    let alpha = u32::from(a);
                    // Weight colour by coverage so edges do not darken.
                    red += u32::from(r) * alpha;
                    green += u32::from(g) * alpha;
                    blue += u32::from(b) * alpha;
                    alpha_sum += alpha;
                }
            }
            // Zero total alpha means every sample in this box was fully
            // transparent, so the destination pixel stays as initialised —
            // transparent black. Skipping the whole pixel up front expresses
            // that as one decision and leaves the four divisions below
            // unconditional, rather than guarding each one against a zero
            // divisor separately.
            if alpha_sum == 0 {
                continue;
            }
            out_pixel.copy_from_slice(&[
                (red / alpha_sum) as u8,
                (green / alpha_sum) as u8,
                (blue / alpha_sum) as u8,
                (alpha_sum / per_box) as u8,
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tray::IconState;

    fn spec() -> IconSpec {
        IconSpec {
            fill: Color::from_rgb(0x34, 0xC7, 0x59),
            stroke: Color::from_rgb(0x34, 0xC7, 0x59),
        }
    }

    #[test]
    fn buffer_is_correctly_sized() {
        for size in [16, 20, 24, 32, 64] {
            let buf = render(spec(), size);
            assert_eq!(buf.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn glyph_is_neither_empty_nor_solid() {
        let buf = render(spec(), 32);
        let opaque = buf.chunks(4).filter(|px| px[3] > 128).count();
        assert!(opaque > 50, "shield nearly empty");
        assert!(opaque < 32 * 32, "shield fully opaque");
    }

    /// The bolt must change the silhouette, not just the hue: a plain shield
    /// and a shield-with-bolt that differed only in colour would be unreadable
    /// in monochrome high-contrast mode and to colour-blind users. The bolt
    /// sits inside an already-opaque body, so this confirms its transparent
    /// gap actually cuts the outline — the rendered mark is not a solid shield.
    #[test]
    fn bolt_cuts_the_silhouette() {
        // Alpha of the full mark (shield plus bolt).
        let with_bolt: Vec<u8> = render(spec(), 32).chunks(4).map(|px| px[3]).collect();
        // Alpha of the shield alone, sampled without the bolt overlay.
        let bare: Vec<u8> = (0..32 * 32)
            .map(|i| {
                let (col, row) = (i % 32, i / 32);
                let (cx, cy) = (cell_centre(col, 32), cell_centre(row, 32));
                match sample_shield(cx, cy, spec()) {
                    Some(colour) => colour.a,
                    None => 0,
                }
            })
            .collect();
        let differing = bare
            .iter()
            .zip(&with_bolt)
            .filter(|(a, b)| (i16::from(**a) - i16::from(**b)).abs() > 40)
            .count();
        assert!(
            differing > 30,
            "the bolt changes only colour ({differing} px differ)"
        );
    }

    /// The sample grid must be symmetric about the centre of the canvas.
    ///
    /// This is the property corner sampling breaks, and it breaks it in one
    /// line: `i / n` puts the first sample on 0 and the last on `(n - 1) / n`,
    /// so the two ends of the ruler are not mirror images and a shape centred
    /// on 0.5 is not measured the same way on its left and right. Stated as an
    /// identity rather than as a rendering, because it is one.
    #[test]
    fn the_sample_grid_is_symmetric() {
        for n in [8u32, 16, 48, 60, 96, 144] {
            for i in 0..n {
                let a = cell_centre(i, n);
                let b = cell_centre(n - 1 - i, n);
                assert!(
                    (a + b - 1.0).abs() < 1e-6,
                    "n={n} i={i}: samples {a} and {b} are not mirrored about 0.5"
                );
            }
        }
    }

    /// The mark must sit the same distance from both side edges.
    ///
    /// The shield is mirror-symmetric and the bolt never reaches the outermost
    /// columns, so the first and last columns of the bitmap must always carry
    /// the same coverage. They did not: at 16 px — the size the notification
    /// area asks for at 100% scaling, and the only size where the shield's
    /// edges land this close to the pixel grid — the left column came out
    /// empty and the right at a third alpha down its whole height. A
    /// one-third-alpha column on one side only is seen as a shadow, and it is
    /// what a real machine showed.
    #[test]
    fn the_mark_is_inset_equally_from_both_sides() {
        let theme = crate::ui::theme::Theme::default();
        for size in [16u32, 20, 24, 28, 32, 40, 48, 64] {
            let rgba = render(theme.icons.normal, size);
            let column = |x: u32| -> u32 {
                (0..size)
                    .map(|y| u32::from(rgba[((y * size + x) * 4 + 3) as usize]))
                    .sum()
            };
            assert_eq!(
                column(0),
                column(size - 1),
                "{size}px: the outer columns carry different coverage, so the \
                 mark is offset within its bitmap"
            );
        }
    }

    /// The glyph must fill its bitmap, not float in the middle of it.
    ///
    /// Windows scales the whole bitmap into the tray slot, so empty margins
    /// are not padding around the icon — they are wasted resolution, and the
    /// mark reads as small beside neighbours that use their full slot. The
    /// shield was originally drawn at half-width 0.34 spanning v 0.12..0.94,
    /// which covered about 68% of the width and looked noticeably smaller
    /// than every other tray icon.
    ///
    /// Checked at the sizes the shell actually asks for, including the 20 and
    /// 24 px variants used at 125% and 150% display scaling.
    #[test]
    fn glyph_fills_the_canvas() {
        let theme = crate::ui::theme::Theme::default();

        for size in [16u32, 20, 24, 32, 48] {
            let rgba = render(theme.icons.normal, size);

            let (mut min_x, mut max_x) = (size, 0u32);
            let (mut min_y, mut max_y) = (size, 0u32);
            for y in 0..size {
                for x in 0..size {
                    if rgba[((y * size + x) * 4 + 3) as usize] > 40 {
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                        min_y = min_y.min(y);
                        max_y = max_y.max(y);
                    }
                }
            }
            assert!(max_x >= min_x, "{size}px: nothing drawn");

            let w = max_x + 1 - min_x;
            let h = max_y + 1 - min_y;
            assert!(
                w * 100 / size >= 85,
                "{size}px: glyph spans only {}% of the width",
                w * 100 / size
            );
            assert!(
                h * 100 / size >= 85,
                "{size}px: glyph spans only {}% of the height",
                h * 100 / size
            );

            // ...but it must not be clipped, or the outline is cut off at the
            // bitmap edge and the shield loses its shape.
            let edge_pixels = (0..size)
                .filter(|&i| {
                    let top = rgba[((i) * 4 + 3) as usize] > 128;
                    let bottom = rgba[(((size - 1) * size + i) * 4 + 3) as usize] > 128;
                    let left = rgba[((i * size) * 4 + 3) as usize] > 128;
                    let right = rgba[((i * size + size - 1) * 4 + 3) as usize] > 128;
                    top || bottom || left || right
                })
                .count();
            assert!(
                edge_pixels <= 2,
                "{size}px: {edge_pixels} pixels touch the bitmap edge; the \
                 glyph is clipped rather than merely large"
            );
        }
    }

    /// Every state must be the *same* drawing, differing only in colour.
    ///
    /// This inverts an earlier rule, and the inversion is deliberate. States
    /// were originally required to differ by more than hue, for readability
    /// in monochrome and to colour-blind eyes, and an earlier iteration
    /// implemented that by giving each state its own shape overlay — which
    /// meant the icon the user sees almost all the time was a different shape
    /// from the window and exe icon. The project owner withdrew that
    /// requirement in favour of one consistent mark. The overlay is now the
    /// single bolt, drawn on every state, and `bolt_cuts_the_silhouette`
    /// guards that it stays visible.
    #[test]
    fn shipped_states_share_one_silhouette() {
        let theme = crate::ui::theme::Theme::default();
        let size = 16u32;
        let reference: Vec<u8> = render(theme.icons.spec(IconState::Normal), size)
            .chunks(4)
            .map(|p| p[3])
            .collect();

        for state in IconState::ALL {
            let alpha: Vec<u8> = render(theme.icons.spec(state), size)
                .chunks(4)
                .map(|p| p[3])
                .collect();
            assert_eq!(
                alpha, reference,
                "{state:?} must have the same silhouette as Normal; only the palette may differ"
            );
        }
    }

    /// ...but the colours must actually differ, or the states become
    /// genuinely indistinguishable rather than merely same-shaped.
    /// Every state must actually show both tones, in a visible proportion.
    ///
    /// The palette declares a light stroke and a darker fill, but whether the
    /// dark tone is *visible* depends on geometry: the shield is narrow and
    /// tapers, so an outline that is slightly too thick swallows the
    /// interior. At `STROKE_W = 0.09` the fill was under a third of the
    /// opaque pixels and the icon read as flat. This pins the balance for
    /// every state, not just the default green.
    #[test]
    fn two_tone_balance_is_visible() {
        let theme = crate::ui::theme::Theme::default();

        for state in IconState::ALL {
            let spec = theme.icons.spec(state);
            let interior = dim(spec.fill);
            let rgba = render(spec, 32);

            let opaque = rgba.chunks(4).filter(|p| p[3] > 128).count();
            // Counted with a tolerance: antialiasing blends the two tones, so
            // an exact match would undercount both.
            let near = |p: &[u8], c: Color| {
                (i16::from(p[0]) - i16::from(c.r)).abs() < 24
                    && (i16::from(p[1]) - i16::from(c.g)).abs() < 24
                    && (i16::from(p[2]) - i16::from(c.b)).abs() < 24
            };
            let light = rgba
                .chunks(4)
                .filter(|p| p[3] > 128 && near(p, spec.stroke))
                .count();
            let dark = rgba
                .chunks(4)
                .filter(|p| p[3] > 128 && near(p, interior))
                .count();

            assert!(
                dark * 100 / opaque >= 30,
                "{state:?}: interior is only {}% of the mark; the icon reads flat",
                dark * 100 / opaque
            );
            assert!(
                light * 100 / opaque >= 30,
                "{state:?}: outline is only {}% of the mark",
                light * 100 / opaque
            );

            // The two tones must be far enough apart to be seen as different.
            let lum = |c: Color| {
                (u32::from(c.r) * 299 + u32::from(c.g) * 587 + u32::from(c.b) * 114) / 1000
            };
            let contrast = lum(spec.stroke).abs_diff(lum(interior));
            assert!(
                contrast > 60,
                "{state:?}: tones differ by only {contrast} in luminance"
            );
        }
    }

    #[test]
    fn shipped_states_have_distinct_colours() {
        let theme = crate::ui::theme::Theme::default();
        let specs: Vec<(IconState, IconSpec)> = IconState::ALL
            .iter()
            .map(|&s| (s, theme.icons.spec(s)))
            .collect();

        for (i, (sa, a)) in specs.iter().enumerate() {
            for (sb, b) in specs.iter().skip(i + 1) {
                assert!(
                    a.fill != b.fill || a.stroke != b.stroke,
                    "{sa:?} and {sb:?} share a palette; with one silhouette \
                     colour is the only cue left"
                );
            }
        }
    }

    /// A spec whose two tones are told apart by value, so an assertion can say
    /// *which* one a point came out as.
    ///
    /// The fixture above deliberately gives stroke and fill the same colour —
    /// it is about coverage, and the tests below are about geometry, which is
    /// the opposite need: with one colour every point inside the mark answers
    /// the same and the outline could be anywhere.
    fn two_tone() -> IconSpec {
        IconSpec {
            fill: Color::from_rgb(0xC8, 0x64, 0x0A),
            stroke: Color::from_rgb(0x10, 0x20, 0x30),
        }
    }

    /// The interior is the fill at two fifths, alpha untouched.
    ///
    /// Pinned by value because every other assertion about the mark's two
    /// tones is stated in terms of this one: a `dim` that darkened by some
    /// other fraction — or that dimmed the alpha along with the channels,
    /// which would make the interior translucent rather than dark — would
    /// still leave the icon two-toned and every proportion test green.
    #[test]
    fn dimming_scales_the_channels_and_leaves_the_alpha() {
        assert_eq!(
            dim(Color::from_rgba(255, 100, 5, 200)),
            Color::from_rgba(102, 40, 2, 200)
        );
        assert_eq!(dim(Color::TRANSPARENT), Color::TRANSPARENT);
    }

    /// Where the shield begins and ends, asked point by point.
    ///
    /// The rendered-mark tests above look at the bitmap as a whole — the share
    /// of opaque pixels, the balance of the two tones, the columns at the two
    /// edges. All of them hold for a great many shapes that are not this one,
    /// which is why the geometry could be mutated without any of them
    /// noticing. These ask the function the questions its constants answer:
    /// the interior is dim, the rim is the stroke, and the three bounds are
    /// where they are said to be.
    #[test]
    fn the_shield_is_bounded_where_its_constants_say() {
        let spec = two_tone();
        let interior = dim(spec.fill);

        // Above the top edge and below the point: outside the mark entirely.
        assert_eq!(sample_shield(0.5, 0.03, spec), None);
        assert_eq!(sample_shield(0.5, 0.99, spec), None);

        // Across the widest part: interior at the centre, stroke inside the
        // rim, nothing past the edge. The edge itself is inclusive — `d >
        // half_w` excludes — and 0.9375 is exactly half a width from the
        // centre, so it is the last column the shield owns.
        assert_eq!(sample_shield(0.5, 0.30, spec), Some(interior));
        assert_eq!(sample_shield(0.90, 0.30, spec), Some(spec.stroke));
        assert_eq!(sample_shield(0.9375, 0.30, spec), Some(spec.stroke));
        assert_eq!(sample_shield(0.95, 0.30, spec), None);

        // The top band is stroke for the width of the outline, so the rounded
        // head reads as outline rather than as a dim cap.
        assert_eq!(sample_shield(0.5, 0.10, spec), Some(spec.stroke));

        // Below the shoulder the body tapers: a column that is interior at the
        // widest part is outside the shield near the point.
        assert_eq!(sample_shield(0.60, 0.30, spec), Some(interior));
        assert_eq!(sample_shield(0.60, 0.90, spec), None);

        // The point itself is a single stroke-coloured sample, not a gap.
        assert_eq!(sample_shield(0.5, 0.9375, spec), Some(spec.stroke));
    }

    /// The bolt is a stroke wedge inside a transparent gap inside nothing.
    ///
    /// Three answers, and the middle one is the whole reason the function has
    /// two wedges: the gap is what cuts the silhouette, and without it the bolt
    /// would be a colour change on an already-opaque body, which
    /// `bolt_cuts_the_silhouette` counts by pixels differing — a measure a
    /// wider or a narrower gap satisfies equally. This says where they are.
    ///
    /// Each wedge is asked at two heights, and at each height at both of its
    /// own edges rather than only at its centre. A centre sample alone pins
    /// almost nothing: the wedge is a fifth of the canvas wide, so a line
    /// displaced by less than its half-width still answers stroke there.
    /// Sampling the edges bounds the position to a hundredth, and using two
    /// heights bounds the slope as well — a wedge that leans differently
    /// passes through the same point at one height and misses at the other.
    #[test]
    fn the_bolt_is_a_wedge_inside_a_transparent_gap() {
        let spec = two_tone();
        let stroke = Some(spec.stroke);
        let gap = Some(Color::TRANSPARENT);

        // The upper wedge runs from 0.62 at v = 0.30, leaning left as it
        // descends. At v = 0.35 its centre is 0.575: stroke on the line, the
        // transparent ring outside it, nothing outside that.
        assert_eq!(sample_bolt(0.575, 0.35, spec), stroke);
        assert_eq!(sample_bolt(0.705, 0.35, spec), gap);
        assert_eq!(sample_bolt(0.755, 0.35, spec), None);
        // Both edges at 0.35, then both edges again at 0.50 where the centre
        // has moved to 0.44.
        assert_eq!(sample_bolt(0.485, 0.35, spec), stroke);
        assert_eq!(sample_bolt(0.665, 0.35, spec), stroke);
        assert_eq!(sample_bolt(0.350, 0.50, spec), stroke);
        assert_eq!(sample_bolt(0.530, 0.50, spec), stroke);

        // The lower wedge restarts at 0.52 rather than continuing from where
        // the upper one left off, and that jump is the kink that makes the
        // shape a bolt instead of a bar. Its centre is 0.484 at v = 0.60 and
        // 0.304 at v = 0.80.
        assert_eq!(sample_bolt(0.394, 0.60, spec), stroke);
        assert_eq!(sample_bolt(0.574, 0.60, spec), stroke);
        assert_eq!(sample_bolt(0.214, 0.80, spec), stroke);
        assert_eq!(sample_bolt(0.394, 0.80, spec), stroke);

        // The two wedges hand over at 0.56 and only one of them is live at a
        // time — checked from both sides, because a handover moved either way
        // leaves both matching over a band and fills the transparent ring in.
        // A bolt with a bulge, and one no coverage count would notice.
        //
        // Just above 0.56 the upper wedge's line would be at 0.368 and just
        // below it the lower wedge's would be at 0.538; each falls in the
        // other's ring, so each answers gap rather than stroke.
        assert_eq!(sample_bolt(0.368, 0.58, spec), gap);
        assert_eq!(sample_bolt(0.538, 0.54, spec), gap);

        // Outside the bolt's own vertical span, top and bottom. The top bound
        // is exclusive, so the first row of the wedge is the one below it.
        assert_eq!(sample_bolt(0.62, 0.30, spec), None);
        assert_eq!(sample_bolt(0.30, 0.85, spec), None);
    }
}
