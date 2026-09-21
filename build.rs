//! Generates the application icon and embeds it in the executable.
//!
//! The icon is *generated*, not vendored. Nothing is downloaded, and no image
//! file is committed to the repository: the same analytic shapes that draw the
//! tray icons at runtime draw this one at build time. That keeps every icon
//! procedural, which is a requirement rather than a preference, and keeps the
//! tree free of binary assets whose provenance would have to be justified.
//!
//! The `.res` is written directly rather than produced by a resource
//! compiler. An earlier version shelled out to `windres`, which exists only
//! in the mingw toolchain: on the normal MSVC build every user saw
//! `no resource compiler found; exe will have no icon`, because MSVC's
//! equivalent is `rc.exe` from the Windows SDK, which is not on `PATH` unless
//! a developer command prompt put it there. Hunting for an SDK across
//! registry keys and versioned directories is both fragile and forbidden
//! here: this utility reads no registry at all.
//!
//! The `.res` container is a documented sequence of aligned records, and this
//! build script already writes the `.ico` payload by hand, so emitting the
//! wrapper costs a few dozen lines and removes the external dependency
//! entirely. `link.exe` takes a `.res` on the command line; the mingw linker
//! takes the same file through the same `-l` mechanism after `windres` would
//! have converted it, so a COFF object is still produced for that path.

use std::path::{Path, PathBuf};

// Colour and geometry are *shared* with `src/`, not duplicated: a build
// script cannot depend on the crate it builds, but it can compile the same
// files, and the `#[path]` includes at the bottom of this script do exactly
// that. An earlier version kept a hand-written twin here — see the note above
// those includes for how that drifted and why the include replaced it.

// The sizes baked into the `.res`, and the only place a *static* icon is
// unavoidable: Windows reads this resource straight off the file on disk for
// Explorer, shortcuts, the "Open with" list and the Task Manager entry of a
// process with no window open. None of those ask the running process
// anything, so no renderer can serve them.
//
// Window title bars are deliberately not on that list. `set_window_icon`
// renders the mark at the exact pixel size `GetSystemMetricsForDpi` asks for,
// which gives an exact bitmap at every DPI step for no bytes at all. An
// earlier iteration had that priority the other way round and answered the
// resulting soft title bar by baking every size the 100–200% range can
// request — 16/20/24/28/32/40/48/56/64, about 41 KB of uncompressed 32-bit
// BMP — solving in the binary what the renderer already solved at runtime.
//
// What remains is the classic set. Where the shell wants a size that is not
// here it takes the nearest baked one and scales; downscaling 48 to 40 costs
// little, unlike upscaling a smaller image, and these surfaces only ever ask
// for icon-view sizes in the first place.
const SIZES: [u32; 4] = [16, 24, 32, 48];

/// Reports a build-time problem the way the toolchain shows it.
///
/// `cargo:warning=` is the only channel a build script has that reaches the
/// person running the build. The three write failures below used to return
/// silently: the build succeeded, the exe came out with no icon and no version
/// resource, and nothing anywhere said so. The mechanism was already known and
/// used for a missing `windres` a few lines further down — this simply applies
/// it to the failures that are at least as consequential.
fn warn(message: &str) {
    println!("cargo:warning={message}");
}

fn main() {
    // Every source this script compiles must be listed, not just the script
    // itself. `src/color.rs` and `src/icon.rs` are pulled in by the `#[path]`
    // includes at the bottom, so they are as much input to the `.res` as
    // `build.rs` is — but cargo only knows what it is told. Once any
    // `rerun-if-changed` is emitted the default "rerun on any change in the
    // package" is switched off, so naming only `build.rs` was worse than
    // naming nothing: editing the icon geometry left the embedded exe icon
    // stale until an unrelated edit to this file happened to rebuild it, and
    // the exe icon then silently disagreed with the tray icon it is supposed
    // to duplicate — the exact drift the shared include was introduced to
    // remove. Pinned by `build_script_reruns_on_every_source_it_compiles`.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/color.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=src/icoenc.rs");

    // The resource files are generated on every target, and only *linked* on
    // Windows. Generating unconditionally costs a few milliseconds and makes
    // the artifacts available to `cargo test` on the host, where the version
    // resource is parsed and checked; skipping generation left that test
    // silently reading nothing and passing regardless.
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into()));
    let ico_path = out.join("ups-monitor.ico");
    let rc_path = out.join("ups-monitor.rc");
    let res_path = out.join("ups-monitor.res");

    let images: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, render(s))).collect();
    if let Err(e) = std::fs::write(&ico_path, encode(&images)) {
        warn(&format!(
            "could not write {}: {e}; the exe will have no icon",
            ico_path.display()
        ));
        return;
    }

    // IDI_APP_ICON = 1: Windows shows the lowest-numbered icon resource for
    // the file, so this must be first to appear in Explorer.
    //
    // The version block is written out too, because rc.exe compiles this file
    // and nothing else; an .rc that mentioned only the icon is how the
    // version resource went missing from the mingw build in an earlier
    // iteration.
    let [a, b, c, d] = version_quad();
    let ver = env!("CARGO_PKG_VERSION");
    let rc = format!(
        "1 ICON \"{ico}\"\n\
         \n\
         1 VERSIONINFO\n\
         FILEVERSION {a},{b},{c},{e}\n\
         PRODUCTVERSION {a},{b},{c},{e}\n\
         FILEOS 0x4\n\
         FILETYPE 0x1\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904B0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"FileDescription\", \"UPS Monitor\"\n\
         \x20     VALUE \"ProductName\", \"UPS Monitor\"\n\
         \x20     VALUE \"FileVersion\", \"{ver}\"\n\
         \x20     VALUE \"ProductVersion\", \"{ver}\"\n\
         \x20     VALUE \"InternalName\", \"ups-monitor\"\n\
         \x20     VALUE \"OriginalFilename\", \"ups-monitor.exe\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
        ico = ico_path.display().to_string().replace('\\', "/"),
        a = a,
        b = b,
        c = c,
        e = d,
    );
    if let Err(e) = std::fs::write(&rc_path, rc) {
        warn(&format!(
            "could not write {}: {e}; the exe will have no icon and no version resource",
            rc_path.display()
        ));
        return;
    }

    if let Err(e) = std::fs::write(&res_path, build_res(&images)) {
        warn(&format!(
            "could not write {}: {e}; the exe will have no icon and no version resource",
            res_path.display()
        ));
        return;
    }

    if target == "windows" {
        emit_resource(&rc_path, &res_path, &out, &env);
    }
}

/// Emits the icon resource and tells cargo to link it.
///
/// MSVC takes the `.res` straight from the linker command line. The GNU
/// linker does not understand `.res`, so for that target the resource is
/// handed to `windres` when it exists; failing that, the exe simply carries
/// no icon, which must stay non-fatal because `cargo check` on a machine
/// without any Windows toolchain is a normal thing to do.
fn emit_resource(rc: &Path, res: &Path, out: &Path, env: &str) {
    if env == "msvc" {
        // Prefer the Windows SDK resource compiler when the build is running
        // in an environment that has it (a Developer Command Prompt, or any
        // shell where rc.exe is on PATH). A `.res` produced by rc.exe is by
        // definition what link.exe expects; the hand-written one below is a
        // fallback, and the difference between them is the one thing that has
        // never been testable from the Linux side of this project.
        let compiled = out.join("ups-monitor-rc.res");
        let rc_ok = std::process::Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&compiled)
            .arg(rc)
            .status();
        if matches!(rc_ok, Ok(s) if s.success()) {
            println!("cargo:rustc-link-arg-bins={}", compiled.display());
            return;
        }

        // link.exe also accepts a .res directly, which keeps the build
        // working without the SDK on PATH. Both paths produce a working icon,
        // so neither is announced: a warning on a successful build is noise,
        // and these two existed only to tell the iterations apart while the
        // Task Manager icon was being chased.
        println!("cargo:rustc-link-arg-bins={}", res.display());
        return;
    }

    // The `.res` is converted, not the `.rc`. Feeding windres the `.rc`
    // compiled only what that file mentions — the icon — so the version
    // resource written into the `.res` never reached the exe, and Task
    // Manager kept showing the file name. One authored resource file, both
    // linkers.
    let obj = out.join("ups-monitor-rc.o");
    let _ = rc;
    for tool in ["x86_64-w64-mingw32-windres", "windres"] {
        let status = std::process::Command::new(tool)
            .arg("--input-format=res")
            .arg("--output-format=coff")
            .arg(res)
            .arg(&obj)
            .status();
        if matches!(status, Ok(s) if s.success()) {
            println!("cargo:rustc-link-arg-bins={}", obj.display());
            return;
        }
    }
    // The only outcome worth reporting: the exe genuinely ends up without an
    // icon. Non-fatal, because `cargo check` without any Windows toolchain is
    // a normal thing to do.
    warn("windres not found; exe will have no icon (MSVC builds are unaffected)");
}

/// Wraps the icon images in a Windows `.res` container.
///
/// The format is a run of records, each a header followed by its payload,
/// with both padded to a 4-byte boundary. Two record types are needed: one
/// `RT_ICON` (type 3) per image, and one `RT_GROUP_ICON` (type 14) directory
/// naming them. Windows shows the lowest-numbered group icon for a file, so
/// the group gets id 1.
fn build_res(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    const RT_VERSION: u16 = 16;

    let mut out = Vec::new();
    // A null record first: the documented marker for the start of a .res.
    push_record(&mut out, 0, 0, &[]);

    // Encoded once, then used twice: the record below carries the bytes and
    // the directory after it carries their length. Encoding a second time to
    // measure what was already written makes the directory and the data two
    // independent results that merely happen to agree — an encoder that is not
    // bit-for-bit reproducible would desynchronise them silently. `icoenc`
    // already builds its payloads this way; this follows it.
    let payloads: Vec<Vec<u8>> = images
        .iter()
        .map(|(size, rgba)| bmp_payload(*size, rgba))
        .collect();

    // The count the directory declares is a `u16`, and so are the resource ids
    // derived from it. Checked once, here, rather than narrowed at each of the
    // four places it is written: a build script may fail the build, and failing
    // it is the right answer — a silently truncated count writes a resource
    // that Explorer reads as a different icon set than the one built.
    let count = u16::try_from(images.len()).expect("an icon group holds at most 65535 images");

    // Icons are numbered from 1 and referenced by the group below.
    for (id, payload) in (1u16..).zip(&payloads) {
        push_record(&mut out, RT_ICON, id, payload);
    }

    // GRPICONDIR: same shape as the .ico directory, except each entry ends
    // with a 2-byte resource id instead of a 4-byte file offset.
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&count.to_le_bytes());
    for (id, ((size, _), payload)) in (1u16..).zip(images.iter().zip(&payloads)) {
        let payload_len = payload.len() as u32;
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        group.push(dim);
        group.push(dim);
        group.push(0);
        group.push(0);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&payload_len.to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_record(&mut out, RT_GROUP_ICON, 1, &group);

    // Without this, Task Manager and Explorer fall back to the file name and
    // show "ups-monitor.exe". `FileDescription` is the field both display as
    // the friendly name, so it carries "UPS Monitor" — the same string as the
    // panel title, because two names for one program is one too many.
    push_record(&mut out, RT_VERSION, 1, &build_version_info());
    out
}

/// A `VS_VERSIONINFO` resource.
///
/// The layout is a tree of length-prefixed, 4-byte-aligned nodes, each with a
/// total length, a value length, a type flag, a UTF-16 key, padding, then the
/// value and any children. It is fiddly but entirely mechanical, and writing
/// it here keeps the no-external-tools property that the `.res` work bought.
/// The crate version as the four numbers a Windows version resource wants.
///
/// Written once because it was written twice: the `.rc` text and the binary
/// `VS_FIXEDFILEINFO` each parsed `CARGO_PKG_VERSION` themselves, with the
/// same four lines and the same padding rule, and the two are meant to state
/// the same version. An array rather than a `Vec` so the four are named by
/// destructuring — a fixed shape cannot be subscripted past its end, and the
/// twelve subscripts this replaces were the only reason either site had to
/// know that `take(4)` had run.
///
/// Missing or unparseable components become zero: a build script is not the
/// place to refuse a build over a version string, and `1.1` means `1.1.0.0`.
fn version_quad() -> [u16; 4] {
    let mut quad = [0u16; 4];
    for (slot, part) in quad.iter_mut().zip(env!("CARGO_PKG_VERSION").split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    quad
}

fn build_version_info() -> Vec<u8> {
    let [a, b, c, d] = version_quad();

    // VS_FIXEDFILEINFO
    let mut ffi = Vec::new();
    ffi.extend_from_slice(&0xFEEF_04BDu32.to_le_bytes()); // signature
    ffi.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // struct version

    // File version and product version, each a pair of packed DWORDs. They
    // carry the same four numbers here because this project ships one version
    // of one product; the four lines are not a duplication to fold away, they
    // are two distinct fields of the structure that happen to agree.
    let high = ((u32::from(a) << 16) | u32::from(b)).to_le_bytes();
    let low = ((u32::from(c) << 16) | u32::from(d)).to_le_bytes();
    ffi.extend_from_slice(&high);
    ffi.extend_from_slice(&low);
    ffi.extend_from_slice(&high);
    ffi.extend_from_slice(&low);
    ffi.extend_from_slice(&0x3Fu32.to_le_bytes()); // file flags mask
    ffi.extend_from_slice(&0u32.to_le_bytes()); // file flags
    ffi.extend_from_slice(&0x0004_0004u32.to_le_bytes()); // VOS_NT_WINDOWS32
    ffi.extend_from_slice(&1u32.to_le_bytes()); // VFT_APP
    ffi.extend_from_slice(&0u32.to_le_bytes()); // subtype
    ffi.extend_from_slice(&0u32.to_le_bytes()); // date high
    ffi.extend_from_slice(&0u32.to_le_bytes()); // date low

    let version = env!("CARGO_PKG_VERSION");
    let strings: [(&str, &str); 6] = [
        // The name Task Manager, Explorer and the Details tab display.
        ("FileDescription", "UPS Monitor"),
        ("ProductName", "UPS Monitor"),
        ("FileVersion", version),
        ("ProductVersion", version),
        ("InternalName", "ups-monitor"),
        ("OriginalFilename", "ups-monitor.exe"),
    ];

    let mut st = Vec::new();
    for (k, val) in strings {
        st.extend(node(k, NodeValue::Text(val), &[]));
    }
    // 040904B0: US English, Unicode.
    let string_table = node("040904B0", NodeValue::None, &st);
    let string_file_info = node("StringFileInfo", NodeValue::None, &string_table);

    // VarFileInfo/Translation must agree with the table name above.
    let mut xlat = Vec::new();
    xlat.extend_from_slice(&0x0409u16.to_le_bytes());
    xlat.extend_from_slice(&0x04B0u16.to_le_bytes());
    let translation = node("Translation", NodeValue::Binary(&xlat), &[]);
    let var_file_info = node("VarFileInfo", NodeValue::None, &translation);

    let mut children = string_file_info;
    children.extend(var_file_info);
    node("VS_VERSION_INFO", NodeValue::Binary(&ffi), &children)
}

/// What a version-info node carries: nothing, a string, or raw bytes.
///
/// `Copy`, because that is what it is — three variants, two of which hold a
/// shared reference and one nothing at all. Saying so is not a concession to a
/// lint but a fact about the type, and it is what lets [`node`] match on the
/// value twice without the second match reading as a use-after-move to anyone
/// scanning the function.
#[derive(Clone, Copy)]
enum NodeValue<'a> {
    None,
    Text(&'a str),
    Binary(&'a [u8]),
}

/// One `.res` record: 32-byte header, payload, padded to 4 bytes.
///
/// `type` and `name` are written as ordinals — the `0xFFFF` prefix followed
/// by the numeric id — which is what Windows looks up for icons and version
/// info.
fn push_record(out: &mut Vec<u8>, ty: u16, id: u16, payload: &[u8]) {
    // DataSize + HeaderSize + Type + Name + DataVersion + MemoryFlags +
    // LanguageId + Version + Characteristics, with Type and Name in their
    // 4-byte ordinal form. That is 32 bytes, and the field must state the
    // true size: the loader uses it to find the payload, so a wrong value
    // silently points at the middle of the header.
    let header_size: u32 = 4 + 4 + 4 + 4 + 4 + 2 + 2 + 4 + 4;
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&header_size.to_le_bytes());
    if ty == 0 && id == 0 {
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    } else {
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // data version
    out.extend_from_slice(&0x0030u16.to_le_bytes()); // memory flags
    out.extend_from_slice(&0x0409u16.to_le_bytes()); // language: en-US
    out.extend_from_slice(&0u32.to_le_bytes()); // version
    out.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    out.extend_from_slice(payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// One version-info node.
///
/// Layout, in order: `wLength`, `wValueLength`, `wType`, a NUL-terminated
/// UTF-16 key, padding to a 4-byte boundary *measured from the start of the
/// node*, the value, padding again, then any child nodes. `wLength` covers
/// the whole node including children but excluding the trailing padding that
/// separates it from its sibling.
///
/// Two details caused real breakage here and are worth stating. `wValueLength`
/// counts UTF-16 *characters* for text and *bytes* for binary — mixing them up
/// yields a resource Windows ignores without complaint. And the padding must
/// be computed against the node's own start, not against the length of the
/// buffer being built: doing the latter shifted every child by two bytes, so
/// `windres` read a key of "\u{1}StringFileInfo" and a `wLength` of zero, and
/// refused the file with "version length 584 greater than resource length
/// 582".
fn node(key: &str, value: NodeValue, children: &[u8]) -> Vec<u8> {
    // The node starts with its own `wLength`, so every offset inside it is two
    // bytes further along than the buffer being built below.
    const LENGTH_FIELD: usize = 2;

    let (value_len, is_text) = match value {
        NodeValue::None => (0u16, true),
        NodeValue::Text(t) => (t.encode_utf16().count() as u16 + 1, true),
        NodeValue::Binary(b) => (b.len() as u16, false),
    };

    // `wLength` counts from the start of the node, and the node starts with
    // `wLength` itself. Everything after it is built into `body` and the field
    // is written when its value is known, rather than reserved as a
    // placeholder and patched back over afterwards: a back-patch is a second
    // place that has to agree about where the field is and how wide it is, and
    // this is a function whose doc comment already records two byte-offset
    // mistakes.
    // The padding rule is stated against the node's start throughout — see the
    // doc comment: aligning against the buffer's own length shifted every
    // child by two bytes and produced a resource `windres` refused.
    let pad = |body: &mut Vec<u8>| {
        while (LENGTH_FIELD + body.len()) % 4 != 0 {
            body.push(0);
        }
    };

    let mut body = Vec::new();
    body.extend_from_slice(&value_len.to_le_bytes());
    body.extend_from_slice(&u16::from(is_text).to_le_bytes());

    for u in key.encode_utf16().chain(std::iter::once(0)) {
        body.extend_from_slice(&u.to_le_bytes());
    }
    pad(&mut body);

    match value {
        NodeValue::None => {}
        NodeValue::Text(t) => {
            for u in t.encode_utf16().chain(std::iter::once(0)) {
                body.extend_from_slice(&u.to_le_bytes());
            }
        }
        NodeValue::Binary(b) => body.extend_from_slice(b),
    }
    pad(&mut body);

    body.extend_from_slice(children);

    let len = (LENGTH_FIELD + body.len()) as u16;
    let mut out = Vec::with_capacity(LENGTH_FIELD + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.append(&mut body);
    out
}

// ---------------------------------------------------------------------------
// Icon rendering.
//
// The renderer is *included*, not copied. `src/icon.rs` and `src/color.rs`
// depend on nothing but each other, so the build script compiles the very
// same source the program uses. An earlier version kept a hand-written twin
// of the geometry here and guarded it with a test that compared source text;
// that guard missed a real drift (it checked whether a substring appeared
// anywhere, so changing one colour channel of three slipped through), and the
// exe icon ended up a different drawing from the tray icon it duplicates.
// Sharing the code removes the failure mode instead of testing for it.
// ---------------------------------------------------------------------------

// `#[path]` rather than `include!`: the files carry `//!` module docs, which
// are only legal at the top of a real module.
//
// `pub`, and that is the whole of what used to be `#[allow(dead_code)]`.
//
// The build script uses only part of each module's API — it renders an icon,
// so it never calls `Color::to_colorref` or reads `IconState` — and the rest
// is there for the program. `dead_code` measures reachability from the crate
// root, so a private module made five items look unused on every build. The
// suppression that answered it was a lint switch, and a lint switch is
// permanent: it would go on hiding the day one of these modules is genuinely
// not used here at all.
//
// Declaring the modules public states the reachability instead of overriding
// the conclusion drawn from it, and it costs nothing that was being paid for.
// `dead_code` inside these modules is still checked in full by the program's
// own build, which is where an unused item in `src/color.rs` is actually a
// finding; the build script was never going to be the place that noticed.
#[path = "src/color.rs"]
pub mod color;
#[path = "src/icon.rs"]
pub mod icon;
// Not `pub`: the script uses every item this one declares, so `dead_code` has
// nothing to report and the private form keeps saying so.
#[path = "src/icoenc.rs"]
mod icoenc;

use icoenc::{bmp_payload, encode};

use icon::{render as render_icon, ICONS};

fn render(size: u32) -> Vec<u8> {
    // The exe icon shares its spec with the window and Task Manager marks: the
    // `app` entry of the one icon set defined in `icon.rs`, which the program
    // also uses at run time. Sharing the definition — rather than parsing a
    // theme file that no longer exists — keeps the exe, the window and the tray
    // one drawing.
    render_icon(ICONS.app, size)
}
