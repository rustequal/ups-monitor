//! Windows icon encoding: RGBA images to `.ico`, and to the `BITMAPINFOHEADER`
//! payload the `.res` container stores per image.
//!
//! Shared by the build script and the crate. `build.rs` cannot depend on the
//! crate it builds, so it pulls this file in with `#[path]` — the same
//! arrangement `src/icon.rs` and `src/color.rs` already use, and for the same
//! reason: the alternative is two hand-kept copies of a byte layout, which is
//! how the exe icon came to be a different drawing from the tray icon it
//! duplicates. There was a copy here too, kept `#[cfg(test)]` so the format
//! stayed under test, with a drift guard between the two. Sharing the code
//! removes the failure mode instead of testing for it.
//!
//! Written by hand rather than pulled from a crate: the format is a header, a
//! directory and a run of payloads, and the whole encoder is shorter than the
//! dependency's changelog. The project ships two runtime dependencies on
//! purpose.
//!
//! Each image is stored as a 32-bit BMP with an AND mask, which every version
//! of Windows accepts. PNG payloads are legal since Vista and smaller at 256
//! px, but need a PNG encoder — another dependency, for one image.

/// Encodes RGBA images as a Windows `.ico` file.
///
/// `images` pairs each square edge length with its RGBA pixels, row-major from
/// the top left.
pub(crate) fn encode(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    // ICONDIR: reserved, type 1 (icon), count.
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let payloads: Vec<Vec<u8>> = images.iter().map(|(s, d)| bmp_payload(*s, d)).collect();

    // Directory entries follow the header; image data follows the directory.
    let mut offset = 6 + 16 * images.len() as u32;
    for ((size, _), payload) in images.iter().zip(&payloads) {
        // 256 is stored as 0: the field is one byte.
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // palette size, 0 for true colour
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += payload.len() as u32;
    }

    for payload in payloads {
        out.extend_from_slice(&payload);
    }
    out
}

/// One image as a `BITMAPINFOHEADER` followed by bottom-up BGRA rows and an AND
/// mask.
///
/// The height in the header is doubled: the format expects colour and mask
/// stacked, and Windows reads the real height as half. Getting this wrong
/// produces an icon squashed to half height, which is the classic symptom.
pub(crate) fn bmp_payload(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // header size
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&0u32.to_le_bytes()); // image size, may be 0
    out.extend_from_slice(&0i32.to_le_bytes()); // x ppm
    out.extend_from_slice(&0i32.to_le_bytes()); // y ppm
    out.extend_from_slice(&0u32.to_le_bytes()); // palette used
    out.extend_from_slice(&0u32.to_le_bytes()); // palette important

    // Colour rows, bottom-up, BGRA.
    //
    // The buffer is walked as rows and pixels rather than addressed by
    // `(y * size + x) * 4`: `rev` is what "bottom-up" means here, and the
    // channel swap is a permutation of the four bytes a chunk already is.
    // Written as offsets, the row order and the channel order were two
    // conventions expressed in one arithmetic expression, which is why the
    // doubled-height mistake this function's doc warns about is so easy to
    // make in its neighbourhood.
    let stride = (size * 4) as usize;
    for row in rgba.chunks_exact(stride).rev() {
        for pixel in row.chunks_exact(4) {
            // Blue, green, red, alpha: the colour triple reversed, the alpha
            // left where it is.
            out.extend(pixel.iter().take(3).rev().chain(pixel.iter().skip(3)));
        }
    }

    // AND mask, one bit per pixel, rows padded to 4 bytes, bottom-up like the
    // colour rows above.
    //
    // A set bit means "leave the background alone", i.e. transparent. This was
    // once filled with zeros on the assumption that 32-bit icons are drawn from
    // their alpha channel and the mask is vestigial. That holds for the desktop
    // and the title bar, but not everywhere: Task Manager's process list draws
    // through a legacy path that still honours the mask, and an all-zero mask
    // declares every pixel opaque — including the fully transparent ones around
    // the shield. The result is a solid rectangle of black on black, which
    // reads as "no icon at all", which is exactly what was reported.
    let row_bytes = size.div_ceil(32) * 4;
    for source in rgba.chunks_exact(stride).rev() {
        let mut row = vec![0u8; row_bytes as usize];
        for (x, pixel) in source.chunks_exact(4).enumerate() {
            // Threshold at the midpoint: the mask is 1-bit, so antialiased
            // edges have to fall to one side. Biasing toward opaque keeps the
            // outline solid rather than fraying it.
            let transparent = pixel.last().is_some_and(|&alpha| alpha < 128);
            if let (true, Some(byte)) = (transparent, row.get_mut(x / 8)) {
                *byte |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&row);
    }
    out
}
