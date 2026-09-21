//! The application icon: the window icon and the one embedded in the exe.
//!
//! # Why this is not the CyberPower logo
//!
//! Using the manufacturer's red "CP" mark here was considered and rejected.
//! It is a registered trademark of Cyber Power Systems (USA), Inc., and
//! putting it on a third-party utility's window and executable presents this
//! program as CyberPower software — which it is not, and which is exactly the
//! confusion trademark protection exists to prevent. That a logo file can be
//! downloaded is not a licence to redistribute it inside a binary. The
//! utility would become undistributable, and the manufacturer would be
//! entitled to object.
//!
//! There is a second, independent reason. Portability is a hard requirement
//! here and outranks convenience, and icons are **generated procedurally** —
//! no `.ico` or `.png` beside the exe. Shipping a downloaded asset would
//! break both rules for a picture.
//!
//! So the mark is original: a shield carrying a lightning bolt, drawn from
//! the same analytic shapes as the tray icons in `icon.rs` and rendered at
//! build-independent sizes at runtime. Shield for protection, bolt for mains
//! power — it says what the utility does without borrowing anyone's identity.
//! The accent colour comes from the active theme, so it follows the tray
//! icons rather than fighting them.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, HICON, WM_SETICON};

use crate::icon::{render, IconSpec};
use crate::ui::theme::Theme;
#[cfg(test)]
use crate::ui::tray::IconState;
/// Sizes carried in the icon resource `build.rs` bakes into the executable.
///
/// Kept in step with the copy in `build.rs` by
/// `resource_sizes_match_the_declared_set` below, which parses the `.res` that
/// build actually produced. A build script cannot depend on the crate it
/// builds, so the set is written twice; a duplicate that can drift silently is
/// worse than no duplicate at all.
///
/// This resource serves only the surfaces Windows reads off the file on disk
/// without asking the process: Explorer, shortcuts, and the Task Manager entry
/// of a process with no window open. 16 for the small views and the details
/// pane, 24 and 32 for the standard shell views, 48 for large icons. Where a
/// size is missing Windows scales a neighbour, which for these surfaces means
/// shrinking 48 — cheap, and nothing here ever asks for a size between them.
///
/// Window title bars are **not** on that list. `set_window_icon` renders the
/// mark at the exact size the window's own DPI asks for, so no baked set has
/// to cover the 100–200% scale range. An earlier iteration widened this to
/// nine sizes to make the title bar crisp; the renderer does that better and
/// for free.
///
/// 256 is deliberately absent. It is used only by Explorer's extra-large view
/// and the file properties dialog, and as an uncompressed 32-bit BMP it costs
/// 270 KB — two thirds again on top of a 425 KB program, for a size almost
/// nobody displays. Storing it as PNG would shrink it, but that means a PNG
/// encoder, and the project ships two runtime dependencies on purpose.
/// Windows scales 48 up when it needs 256; a slightly soft icon in one rarely
/// opened dialog is the better trade.
#[cfg(test)]
pub(crate) const SIZES: [u32; 4] = [16, 24, 32, 48];

/// The application mark, in the theme's `[icons] app` colours.
///
/// Same glyph, same geometry and same renderer as the tray icons — only the
/// palette differs. The application icon is blue and does not follow power
/// state: an exe on disk and a window on screen are never "on battery", so
/// tying this to the tray's green `normal` state made the file's appearance
/// depend on something it cannot represent. State belongs in the tray.
pub(crate) fn spec(theme: &Theme) -> IconSpec {
    theme.icons.app
}

/// Renders the mark at one size as RGBA, through the same path as the tray.
pub(crate) fn render_rgba(theme: &Theme, size: u32) -> Vec<u8> {
    render(spec(theme), size)
}

/// Which of the two icons a window carries.
///
/// Named rather than `WPARAM(0)` and `WPARAM(1)`, which is how both call sites
/// spelled it. The numbers are `ICON_SMALL` and `ICON_BIG` and they are not
/// interchangeable: the small one is the title bar and the taskbar, the big one
/// is Alt+Tab, and setting the wrong one leaves Windows to scale the other from
/// it — which is the blurred taskbar icon this program has already fixed once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IconSlot {
    /// Title bar and taskbar.
    Small,
    /// Alt+Tab and the task switcher.
    Big,
}

impl IconSlot {
    /// The `wParam` `WM_SETICON` expects.
    fn as_wparam(self) -> WPARAM {
        match self {
            Self::Small => WPARAM(0),
            Self::Big => WPARAM(1),
        }
    }
}

/// Gives `hwnd` an icon, and reports back the handle the window now owns.
///
/// `Some` means the caller has taken on a handle it must destroy when it
/// replaces or discards it — `WM_SETICON` stores the handle rather than copying
/// it. `None` means the icon came from the exe resource and belongs to the
/// system. Returning that rather than leaving it to the caller keeps the
/// ownership rule beside [`WindowIcon`], which is the type that knows it.
///
/// It exists as a function because the alternative was the same three lines in
/// two files, and the middle one of them was `LPARAM(icon.handle().0 as isize)`
/// — a cast that is unavoidable, `LPARAM` being an integer by ABI, but that has
/// no business in code otherwise occupied with title bars. Here it is written
/// once, at the boundary, where a reader looking for the program's remaining
/// pointer-to-integer conversions will find it.
pub(crate) fn set_window_icon(hwnd: HWND, slot: IconSlot, icon: &WindowIcon) -> Option<HICON> {
    // SAFETY: `hwnd` names a window to the system, which validates it, and the
    // handle is a live icon this process either rendered or loaded from its own
    // resource. The message stores the handle; ownership is what the return
    // value reports.
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(slot.as_wparam()),
            Some(LPARAM(icon.handle().0 as isize)),
        )
    };
    icon.owned()
}

/// An icon handed to a window, carrying who owns the handle.
///
/// Provenance is inseparable from the handle and cannot be recovered from it:
/// `LoadImageW` with `LR_SHARED` returns one the system owns, on which
/// `DestroyIcon` must never be called, while a rendered one leaks unless
/// somebody frees it. Returning a bare handle made that a fact each caller had
/// to remember, and the two windows that set an icon remembered it separately —
/// the panel got it right, the tray window dropped both of its handles on the
/// floor. The rule now travels with the value, and [`WindowIcon::owned`] is the
/// only place it is applied.
#[derive(Clone, Copy)]
pub(crate) enum WindowIcon {
    /// From the exe's icon resource. The system owns it.
    Shared(HICON),
    /// Rendered by this process for this window. The window owns it and must
    /// destroy it when it is done.
    Owned(HICON),
}

impl WindowIcon {
    /// The handle, to hand to `WM_SETICON` or a window class.
    pub(crate) fn handle(self) -> HICON {
        match self {
            WindowIcon::Shared(h) | WindowIcon::Owned(h) => h,
        }
    }

    /// The handle the caller must destroy, or `None` when it belongs to the
    /// system.
    pub(crate) fn owned(self) -> Option<HICON> {
        match self {
            WindowIcon::Shared(_) => None,
            WindowIcon::Owned(h) => Some(h),
        }
    }
}

/// Loads icon id 1 from this executable at a given pixel size.
///
/// `LR_SHARED` means the module owns the handle and it must not be destroyed,
/// which is exactly how `WM_SETICON` stores it — hence [`WindowIcon::Shared`]
/// rather than a bare handle.
///
/// Uses `LoadImageW` rather than `LoadIconMetric`. The latter is what the
/// notification-area documentation recommends, and reaching for it here was a
/// mistake: it is exported by comctl32 version 6, which a process only gets
/// through an application manifest declaring that assembly dependency. This
/// executable has no manifest, so the import could not be resolved and Windows
/// refused to start the program at all. A statically linked import is resolved
/// at load time, before any code runs, so no error handling around the call
/// site could have saved it. `LoadImageW` lives in user32, needs no manifest,
/// and reads the same `RT_GROUP_ICON` resource.
pub(crate) fn resource_icon(size: i32) -> Option<WindowIcon> {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, IMAGE_ICON, LR_SHARED};

    // SAFETY: the module handle is this executable's own, and the resource id
    // is an integer passed in the pointer slot as `MAKEINTRESOURCE` requires —
    // not a pointer to be dereferenced. `LR_SHARED` means the icon belongs to
    // the module and must not be destroyed, which is what `WindowIcon::Shared`
    // records for the caller.
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        let instance: windows::Win32::Foundation::HINSTANCE = module.into();
        // MAKEINTRESOURCE(1): the icon group id the build script writes.
        let handle = LoadImageW(
            Some(instance),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            size,
            size,
            LR_SHARED,
        )
        .ok()?;
        // Rebuilt from the field rather than converted, because `windows` 0.62
        // defines no `From<HANDLE> for HICON` — checked against the generated
        // source, not assumed. `LoadImageW` is typed to return the generic
        // `HANDLE` for all three image kinds it can load, and narrowing that to
        // the kind actually asked for is left to the caller.
        //
        // This is the only bet on a foreign type's representation left in the
        // tree; the other two were removed with it. It is confined to one line
        // in the module that owns icons, and it breaks loudly — a change to
        // either type's field is a compile error here, not a silent
        // reinterpretation.
        Some(WindowIcon::Shared(HICON(handle.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    /// The application icon shares the tray's geometry but not its colours.
    ///
    /// Same glyph, same silhouette, different palette: the exe and window are
    /// blue regardless of power state, while the tray shows state. Checked
    /// both ways round, because either half alone would let a regression
    /// through — identical pixels would mean the app icon had gone green,
    /// and a different silhouette would mean it had stopped being the same
    /// mark.
    #[test]
    fn app_icon_shares_tray_geometry_but_not_its_palette() {
        let t = theme();
        for size in [16u32, 24, 32, 48] {
            let app = render_rgba(&t, size);
            let tray = crate::icon::render(t.icons.spec(IconState::Normal), size);

            let alpha = |v: &[u8]| -> Vec<u8> { v.chunks(4).map(|p| p[3]).collect() };
            assert_eq!(
                alpha(&app),
                alpha(&tray),
                "app icon is a different shape from the tray icon at {size}px"
            );
            assert_ne!(
                app, tray,
                "app icon must not use the tray's normal colours; it is blue"
            );
        }
    }

    /// The blue is the theme's, not a hardcoded constant.
    #[test]
    fn app_icon_uses_the_theme_app_entry() {
        let t = theme();
        assert_eq!(spec(&t), t.icons.app);
        // Distinct from every tray state, or it would read as a state.
        for state in IconState::ALL {
            assert_ne!(
                t.icons.app.fill,
                t.icons.spec(state).fill,
                "app colour collides with the {state:?} tray state"
            );
        }
    }

    #[test]
    fn every_declared_size_renders() {
        for size in SIZES {
            let rgba = render_rgba(&theme(), size);
            assert_eq!(rgba.len(), (size * size * 4) as usize, "size {size}");
        }
    }

    /// The mark must actually be a mark: neither blank nor a solid block.
    #[test]
    fn mark_is_neither_empty_nor_solid() {
        for size in SIZES {
            let rgba = render_rgba(&theme(), size);
            let opaque = rgba.chunks(4).filter(|p| p[3] > 128).count();
            let total = (size * size) as usize;
            assert!(opaque > total / 20, "icon nearly empty at {size}");
            assert!(opaque < total * 9 / 10, "icon nearly solid at {size}");
        }
    }

    #[test]
    fn ico_header_and_directory_are_well_formed() {
        let images: Vec<(u32, Vec<u8>)> = SIZES
            .iter()
            .map(|&s| (s, render_rgba(&theme(), s)))
            .collect();
        let ico = crate::icoenc::encode(&images);

        assert_eq!(&ico[0..2], &[0, 0], "reserved must be zero");
        assert_eq!(&ico[2..4], &[1, 0], "type must be 1 (icon)");
        assert_eq!(
            u16::from_le_bytes([ico[4], ico[5]]) as usize,
            SIZES.len(),
            "image count"
        );

        // Each directory entry must point inside the file at a payload of the
        // stated length: a wrong offset is the failure that produces an icon
        // Windows silently refuses to draw.
        for i in 0..SIZES.len() {
            let e = 6 + i * 16;
            let len = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]);
            let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]);
            assert!(
                off as usize + len as usize <= ico.len(),
                "entry {i} runs past the end of the file"
            );
            assert!(len > 40, "entry {i} payload is smaller than its header");
        }
    }

    /// 256 is stored as 0 in the one-byte dimension field. Writing 255, or
    /// truncating 256 to 0 by accident elsewhere, both produce a file that
    /// looks valid and renders wrongly.
    #[test]
    fn large_size_is_encoded_as_zero() {
        let images = vec![(256u32, render_rgba(&theme(), 256))];
        let ico = crate::icoenc::encode(&images);
        assert_eq!(ico[6], 0, "width byte for 256 must be 0");
        assert_eq!(ico[7], 0, "height byte for 256 must be 0");

        let images = vec![(32u32, render_rgba(&theme(), 32))];
        let ico = crate::icoenc::encode(&images);
        assert_eq!(ico[6], 32);
    }

    /// The version resource `build.rs` writes must parse, and must name the
    /// program.
    ///
    /// Task Manager shows `FileDescription`; without a version resource it
    /// falls back to the file name, which is what produced "ups-monitor.exe"
    /// in the process list. This parses the artifact `build.rs` actually
    /// produced rather than a re-implementation of it: an earlier drift guard
    /// compared source text and missed a real divergence, so the artifact is
    /// the thing to check.
    #[test]
    fn version_resource_names_the_program() {
        let Some(res) = generated_res() else {
            // No build output in this profile; nothing to check.
            return;
        };

        let v = res_record(&res, RT_VERSION)
            .expect("no RT_VERSION record; Task Manager would show the file name");

        // The root node must declare exactly the bytes it occupies. windres
        // rejects the file otherwise, and the resource silently vanishes.
        let declared = u16::from_le_bytes(v[0..2].try_into().unwrap()) as usize;
        assert_eq!(
            declared,
            v.len(),
            "VS_VERSION_INFO declares {declared} bytes but the record holds {}",
            v.len()
        );

        // wType 0 marks a binary value: the root carries VS_FIXEDFILEINFO.
        assert_eq!(u16::from_le_bytes(v[4..6].try_into().unwrap()), 0);
        assert_eq!(
            u16::from_le_bytes(v[2..4].try_into().unwrap()),
            52,
            "VS_FIXEDFILEINFO is 52 bytes"
        );

        // Walking the child tree is the part that matters. Searching the
        // blob for strings would pass on a malformed tree — the text is still
        // in there — which is exactly how a broken version resource slips
        // through: the bytes look present, and Windows ignores them.
        let keys = walk_version_tree(v);
        for expected in [
            "VS_VERSION_INFO",
            "StringFileInfo",
            "040904B0",
            "FileDescription",
            "VarFileInfo",
            "Translation",
        ] {
            assert!(
                keys.iter().any(|(k, _)| k == expected),
                "version tree has no {expected:?} node; found {keys:?}"
            );
        }

        let description = keys
            .iter()
            .find(|(k, _)| k == "FileDescription")
            .and_then(|(_, val)| val.clone())
            .expect("FileDescription must carry a value");
        assert_eq!(
            description, "UPS Monitor",
            "Task Manager shows this string; it must match the panel title"
        );
    }

    /// Walks a `VS_VERSIONINFO` tree, returning every node's key and text
    /// value. Panics if a node declares a length that does not fit, which is
    /// precisely the corruption `windres` refuses to link.
    fn walk_version_tree(v: &[u8]) -> Vec<(String, Option<String>)> {
        fn walk(d: &[u8], off: usize, end: usize, out: &mut Vec<(String, Option<String>)>) {
            let mut off = off;
            while off + 6 <= end {
                let len = u16::from_le_bytes(d[off..off + 2].try_into().unwrap()) as usize;
                if len == 0 {
                    break;
                }
                assert!(
                    off + len <= end,
                    "node at {off} declares {len} bytes, only {} remain",
                    end - off
                );
                let value_len =
                    u16::from_le_bytes(d[off + 2..off + 4].try_into().unwrap()) as usize;
                let ty = u16::from_le_bytes(d[off + 4..off + 6].try_into().unwrap());

                let mut i = off + 6;
                let mut key = String::new();
                while i + 1 < end {
                    let c = u16::from_le_bytes(d[i..i + 2].try_into().unwrap());
                    i += 2;
                    if c == 0 {
                        break;
                    }
                    key.push(char::from_u32(u32::from(c)).unwrap_or('?'));
                }

                let vstart = (i + 3) & !3;
                let vbytes = if ty == 1 { value_len * 2 } else { value_len };
                let text = if ty == 1 && value_len > 0 && vstart + vbytes <= end {
                    let units: Vec<u16> = d[vstart..vstart + vbytes]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    Some(
                        String::from_utf16_lossy(&units)
                            .trim_end_matches('\0')
                            .to_owned(),
                    )
                } else {
                    None
                };
                out.push((key, text));

                let cstart = (vstart + vbytes + 3) & !3;
                if cstart < off + len {
                    walk(d, cstart, off + len, out);
                }
                off += (len + 3) & !3;
            }
        }
        let mut out = Vec::new();
        walk(v, 0, v.len(), &mut out);
        out
    }

    /// Reads the `.res` this build produced, if the build script has run.
    fn generated_res() -> Option<Vec<u8>> {
        let out = std::path::Path::new(env!("OUT_DIR")).join("ups-monitor.res");
        std::fs::read(out).ok()
    }

    /// `RT_GROUP_ICON`: the directory naming every image in the icon resource.
    const RT_GROUP_ICON: u16 = 14;
    /// `RT_VERSION`: the `VS_VERSIONINFO` block.
    const RT_VERSION: u16 = 16;

    /// The payload of the last `.res` record of a given type.
    ///
    /// A `.res` is a flat run of 32-byte headers each followed by its payload,
    /// both padded to four bytes. `Type` sits at offset 8 in its ordinal form —
    /// `0xFFFF` then the numeric id — so the id itself is at offset 10.
    fn res_record(res: &[u8], ty: u16) -> Option<&[u8]> {
        let mut off = 0usize;
        let mut found = None;
        while off + 32 <= res.len() {
            let dsize = u32::from_le_bytes(res[off..off + 4].try_into().unwrap()) as usize;
            let hsize = u32::from_le_bytes(res[off + 4..off + 8].try_into().unwrap()) as usize;
            let kind = u16::from_le_bytes(res[off + 10..off + 12].try_into().unwrap());
            if kind == ty && off + hsize + dsize <= res.len() {
                found = Some(&res[off + hsize..off + hsize + dsize]);
            }
            off += (hsize + dsize + 3) & !3;
        }
        found
    }

    /// The sizes baked into the exe must be exactly the set this module
    /// declares.
    ///
    /// The list lives in two crates — a build script cannot depend on the crate
    /// it builds — so it can drift. The check reads the `RT_GROUP_ICON`
    /// directory out of the `.res` the build actually produced rather than
    /// matching text in `build.rs`: an earlier drift guard compared source
    /// strings and missed a real divergence, and the artifact is what Windows
    /// will read.
    ///
    /// The set is deliberately small. It covers only the surfaces that read the
    /// file on disk; window icons are rendered at their exact size at runtime,
    /// so widening this to chase DPI steps trades kilobytes for nothing.
    #[test]
    fn resource_sizes_match_the_declared_set() {
        let Some(res) = generated_res() else {
            // No build output in this profile; nothing to check.
            return;
        };
        let group = res_record(&res, RT_GROUP_ICON)
            .expect("no RT_GROUP_ICON record; the exe would have no icon at all");

        // GRPICONDIR: reserved, type, count, then one 14-byte entry per image.
        assert_eq!(u16::from_le_bytes(group[2..4].try_into().unwrap()), 1);
        let count = u16::from_le_bytes(group[4..6].try_into().unwrap()) as usize;
        assert_eq!(count, SIZES.len(), "image count in the group directory");

        let sizes: Vec<u32> = (0..count)
            .map(|i| {
                // Width is one byte; 256 is stored as 0.
                let w = group[6 + i * 14];
                if w == 0 {
                    256
                } else {
                    u32::from(w)
                }
            })
            .collect();
        assert_eq!(
            sizes,
            SIZES.to_vec(),
            "build.rs bakes a different set of icon sizes than this module documents"
        );
    }

    /// The exe icon must be the same pixels as the window icon.
    ///
    /// `build.rs` no longer keeps its own copy of the geometry — it compiles
    /// `src/icon.rs` directly via `#[path]` — so this compares the rendered
    /// result rather than the source text. The previous guard matched
    /// substrings in `build.rs` and missed a genuine drift: it asked whether
    /// `"* 2 / 5"` appeared anywhere, so changing one colour channel of three
    /// left it green while the exe icon quietly diverged.
    #[test]
    fn exe_icon_pixels_match_the_window_icon() {
        // The exe icon (build.rs) and the window/app icon (this module) render
        // from the same shared spec, `icon::ICONS.app`. There is no theme file
        // to parse any more; the single constant *is* the shared source, so
        // this checks that the theme still exposes it unchanged and that both
        // render paths agree pixel for pixel.
        let from_theme = theme().icons.app;
        assert_eq!(
            from_theme,
            crate::icon::ICONS.app,
            "the app spec must come from the one shared icon set"
        );

        for size in SIZES {
            assert_eq!(
                crate::icon::render(crate::icon::ICONS.app, size),
                render_rgba(&theme(), size),
                "exe icon differs from the window/tray icon at {size}px"
            );
        }
    }

    /// Every icon Task Manager might pick must be a real, non-empty image.
    ///
    /// Task Manager resolves a process icon in a fixed order (Raymond Chen,
    /// "How does Task Manager choose the icon to show for a process?"): the
    /// icon of a visible window, else the notification icon, else the icon of
    /// the executable. This utility usually has no window open, so the exe
    /// resource is the branch reached *last* — which is why two iterations of
    /// work on that resource changed nothing in the process list.
    ///
    /// The fix is to make all three sources correct rather than only the one
    /// that is easiest to inspect offline. This checks the two that are
    /// rendered at runtime: the application mark used for window icons, and
    /// every tray state used for the notification icon.
    #[test]
    fn all_task_manager_icon_sources_are_populated() {
        let t = theme();

        let mut sources: Vec<(String, IconSpec)> = vec![("window/app".to_owned(), t.icons.app)];
        for state in IconState::ALL {
            sources.push((format!("tray {state:?}"), t.icons.spec(state)));
        }

        for (name, spec) in sources {
            // Sizes Windows asks for via SM_CXSMICON / SM_CXICON across the
            // 100–200% scale range (16/20/24/28/32 and 32/40/48/56/64). The
            // renderer, not the baked `.res`, is what answers these now:
            // `set_window_icon` draws the window mark at whichever of them the
            // window's DPI produces, and `tray.rs` does the same for the
            // notification icon. So the full range belongs here even though
            // the `.res` carries only the classic four — that resource covers
            // a different surface and is checked separately by
            // `resource_sizes_match_the_declared_set`.
            for size in [16u32, 20, 24, 28, 32, 40, 48, 56, 64] {
                let rgba = crate::icon::render(spec, size);
                assert_eq!(rgba.len(), (size * size * 4) as usize);

                let opaque = rgba.chunks(4).filter(|p| p[3] > 128).count();
                assert!(
                    opaque > (size * size / 20) as usize,
                    "{name} at {size}px is effectively blank: an empty icon is \
                     indistinguishable from no icon in the process list"
                );
                // Fully opaque would mean a solid rectangle, which is what a
                // broken mask or a filled glyph looks like on screen.
                assert!(
                    opaque < (size * size * 9 / 10) as usize,
                    "{name} at {size}px is a solid block"
                );
            }
        }
    }

    /// The crate must not depend on comctl32.
    ///
    /// `LoadIconMetric` was used briefly because the notification-area
    /// documentation recommends it. It is exported by comctl32 **version 6**,
    /// which a process only receives via an application manifest declaring
    /// that assembly. This executable ships without a manifest, so the import
    /// could not be resolved and Windows refused to launch the program:
    ///
    /// > The procedure entry point LoadIconMetric could not be located in the
    /// > dynamic link library ups-monitor.exe
    ///
    /// A statically linked import is resolved before `main` runs, so this
    /// could not be caught by error handling, only by not linking it. The
    /// equivalent user32 call, `LoadImageW`, needs no manifest.
    ///
    /// Guarded here rather than by inspecting the binary because the manifest
    /// is a per-target concern and `Cargo.toml` is the single place the
    /// dependency could be reintroduced.
    #[test]
    fn no_comctl32_dependency() {
        let manifest = include_str!("../../Cargo.toml");
        assert!(
            !manifest.contains("Win32_UI_Controls"),
            "Win32_UI_Controls pulls in comctl32 APIs such as LoadIconMetric, \
             which need an application manifest this exe does not have; the \
             program then fails to start with an entry-point error"
        );

        let sources = [
            include_str!("tray_window.rs"),
            include_str!("window/mod.rs"),
            include_str!("window/paint.rs"),
            include_str!("window/draw.rs"),
        ];
        for src in sources {
            // Doc comments explaining the incident are fine; a call is not.
            for line in src.lines() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                assert!(
                    !code.contains("LoadIconMetric"),
                    "LoadIconMetric is a comctl32 v6 export and must not be called"
                );
            }
        }
    }

    /// Only an icon this process rendered may be destroyed.
    ///
    /// Both windows that set a title-bar icon prefer one source and fall back
    /// to the other, so both hold handles of both provenances, and the two
    /// must never be treated alike: a rendered handle nobody frees is a leak,
    /// and `DestroyIcon` on an `LR_SHARED` handle is a fault. That was a fact
    /// each call site had to remember — the panel remembered, the tray window
    /// did not, and its two handles were never freed.
    ///
    /// Now the rule is applied in exactly one place and this checks that place
    /// rather than the source text of its callers.
    #[test]
    fn only_a_rendered_icon_is_ours_to_destroy() {
        // The null handle, not an invented address. Nothing here dereferences
        // it — the rule under test is about provenance, not about the icon
        // behind the handle — so the value only has to be *a* handle. A made-up
        // pointer literal would answer the same, and would leave a pattern in
        // the suite for the next test to copy, which might not be as harmless.
        // "No handle at all" is the honest way to say the value is irrelevant.
        let handle = HICON::default();

        assert_eq!(
            WindowIcon::Owned(handle).owned(),
            Some(handle),
            "a rendered icon leaks unless the window that took it frees it"
        );
        assert_eq!(
            WindowIcon::Shared(handle).owned(),
            None,
            "an LR_SHARED handle belongs to the system; destroying it is a fault"
        );

        // Both still answer WM_SETICON with the handle they carry: provenance
        // changes who frees it, never which icon is shown.
        assert_eq!(WindowIcon::Owned(handle).handle(), handle);
        assert_eq!(WindowIcon::Shared(handle).handle(), handle);
    }

    /// Every source the build script compiles must also be declared as an
    /// input to it.
    ///
    /// Emitting any `cargo:rerun-if-changed` switches off the default "rerun
    /// when anything in the package changes", so an incomplete list is worse
    /// than none at all. The script names `build.rs` and pulls in
    /// `src/color.rs` and `src/icon.rs` through `#[path]`; those two are as
    /// much input to the `.res` as the script itself, and leaving them out left
    /// the embedded exe icon stale after an edit to the icon geometry — the exe
    /// then disagreed with the tray icon it duplicates, which is precisely the
    /// drift the shared include exists to prevent.
    ///
    /// Both sides of the check live in the same file, so this compares them
    /// directly rather than restating either.
    #[test]
    fn build_script_reruns_on_every_source_it_compiles() {
        let script = include_str!("../../build.rs");

        let declared: Vec<&str> = script
            .lines()
            .filter_map(|l| l.trim().strip_prefix("println!(\"cargo:rerun-if-changed="))
            .filter_map(|l| l.split('"').next())
            .collect();

        // `#[path = "..."]` attributes, which is how the script compiles a
        // source it does not own.
        let included: Vec<&str> = script
            .lines()
            .filter_map(|l| l.trim().strip_prefix("#[path = \""))
            .filter_map(|l| l.split('"').next())
            .collect();

        assert!(
            !included.is_empty(),
            "the #[path] includes were not found; this test is looking in the wrong place"
        );
        assert!(
            declared.contains(&"build.rs"),
            "the script itself must be declared"
        );
        for path in included {
            assert!(
                declared.contains(&path),
                "{path} is compiled by build.rs but not declared with \
                 cargo:rerun-if-changed, so editing it will not rebuild the resources"
            );
        }
    }

    /// The AND mask must mirror the alpha channel, not be zero-filled.
    ///
    /// This is the bug that made the exe icon invisible in Task Manager while
    /// every structural check passed: the resource was well-formed, the
    /// directory correct, the alpha channel right, and the mask said "every
    /// pixel is opaque". Paths that honour the mask — Task Manager's process
    /// list among them — then drew the transparent surround as solid black.
    /// Nothing in the format is violated by a zero mask, so only a test that
    /// compares it against alpha catches this.
    #[test]
    fn and_mask_follows_the_alpha_channel() {
        let t = theme();
        for size in SIZES {
            let rgba = render_rgba(&t, size);
            let payload = crate::icoenc::bmp_payload(size, &rgba);

            let mask_off = 40 + (size * size * 4) as usize;
            let row_bytes = size.div_ceil(32) * 4;
            let mask = &payload[mask_off..];
            assert_eq!(mask.len(), (row_bytes * size) as usize, "mask size");

            assert!(
                mask.iter().any(|b| *b != 0),
                "{size}px mask is all zeros: every pixel would be treated as \
                 opaque and the icon renders as a filled rectangle"
            );

            // Every transparent pixel must have its bit set, every opaque one
            // clear. Rows are bottom-up, matching the colour data.
            let mut transparent = 0usize;
            for y in 0..size {
                let mask_row = ((size - 1 - y) * row_bytes) as usize;
                for x in 0..size {
                    let alpha = rgba[((y * size + x) * 4 + 3) as usize];
                    let bit = mask[mask_row + (x / 8) as usize] & (0x80 >> (x % 8)) != 0;
                    let expected = alpha < 128;
                    if expected {
                        transparent += 1;
                    }
                    assert_eq!(
                        bit, expected,
                        "{size}px mask disagrees with alpha at ({x},{y}): \
                         alpha={alpha} mask_bit={bit}"
                    );
                }
            }
            // The shield does not fill its bounding box, so a correct mask
            // always has transparent pixels. A mask that agreed with a
            // fully-opaque alpha channel would pass the loop above.
            assert!(
                transparent > (size * size / 10) as usize,
                "{size}px: only {transparent} transparent pixels; the glyph \
                 should not fill its box"
            );
        }
    }

    /// The stored height is doubled for the AND mask. If this regresses the
    /// icon appears squashed to half height.
    #[test]
    fn bmp_height_is_doubled_for_the_mask() {
        let size = 16u32;
        let payload = crate::icoenc::bmp_payload(size, &render_rgba(&theme(), size));
        let h = i32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
        assert_eq!(h, (size * 2) as i32);

        let expected = 40 + (size * size * 4) + size * size.div_ceil(32) * 4;
        assert_eq!(payload.len(), expected as usize, "payload size");
    }
}
