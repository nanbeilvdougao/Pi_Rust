//! Clipboard integration for the TUI.
//!
//! `Ctrl+V` in the TUI tries (in order):
//! 1. An image on the clipboard → return an `Attachment` ready to ship to
//!    a multimodal provider. We encode RGBA pixels into PNG bytes inline
//!    using a tiny zlib-less PNG writer so we don't pull `image`.
//! 2. Plain text → insert into the input buffer at the cursor.
//! 3. Nothing → return `Pasted::Empty`.
//!
//! When the workspace is built without the `clipboard` feature (e.g. CI
//! containers without a display server) we still expose the API but it
//! always returns `Pasted::Empty` so callers do not need to feature-gate
//! their key bindings.

use pi_core::{Attachment, AttachmentData, AttachmentKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    Empty,
    Text(String),
    Image(Attachment),
}

#[cfg(feature = "clipboard")]
pub fn read_clipboard() -> Pasted {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return Pasted::Empty;
    };
    if let Ok(image) = clipboard.get_image() {
        let (width, height, rgba) = scale_to_max(
            image.width as u32,
            image.height as u32,
            image.bytes.into_owned(),
            MAX_DIM,
        );
        if let Some(png) = encode_png(width, height, &rgba) {
            return Pasted::Image(Attachment {
                mime_type: "image/png".to_string(),
                kind: AttachmentKind::Image,
                data: AttachmentData::Base64 {
                    data: pi_core::base64_encode(&png),
                },
            });
        }
    }
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            return Pasted::Text(text);
        }
    }
    Pasted::Empty
}

/// Max long side for clipboard images before we downscale. Matches TS pi's
/// 1568 px choice (the largest Anthropic recommends).
const MAX_DIM: u32 = 1568;

/// Downscale RGBA via nearest-neighbor if either dimension exceeds `max`.
/// arboard returns clipboard images as already-decoded RGBA; EXIF orientation
/// is baked in upstream so we don't need a separate rotation pass.
pub fn scale_to_max(width: u32, height: u32, rgba: Vec<u8>, max: u32) -> (u32, u32, Vec<u8>) {
    if width <= max && height <= max {
        return (width, height, rgba);
    }
    let scale = (max as f32 / width.max(height) as f32).min(1.0);
    let new_w = ((width as f32 * scale) as u32).max(1);
    let new_h = ((height as f32 * scale) as u32).max(1);
    let mut out: Vec<u8> = Vec::with_capacity((new_w * new_h * 4) as usize);
    let xratio = width as f32 / new_w as f32;
    let yratio = height as f32 / new_h as f32;
    for y in 0..new_h {
        let src_y = (y as f32 * yratio) as u32;
        for x in 0..new_w {
            let src_x = (x as f32 * xratio) as u32;
            let idx = ((src_y * width + src_x) * 4) as usize;
            if idx + 4 <= rgba.len() {
                out.extend_from_slice(&rgba[idx..idx + 4]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    (new_w, new_h, out)
}

#[cfg(not(feature = "clipboard"))]
pub fn read_clipboard() -> Pasted {
    Pasted::Empty
}

/// Encode an RGBA byte buffer as a minimal PNG (no compression — uses
/// stored-only deflate blocks). The output is bigger than zlib-compressed
/// PNGs but stays inside our `unsafe_code = forbid` and zero-deps stance,
/// and downstream providers re-encode anyway.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(rgba.len() + 1024);
    // PNG signature
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    // IHDR
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(6); // RGBA
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut out, b"IHDR", &ihdr);

    // IDAT: zlib container around stored deflate blocks of the filtered scanlines.
    // Filter 0 prepended to each row.
    let stride = (width as usize) * 4;
    let mut raw: Vec<u8> = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0); // filter type none
        raw.extend_from_slice(&rgba[row * stride..(row + 1) * stride]);
    }
    let zlib = wrap_zlib_stored(&raw);
    write_chunk(&mut out, b"IDAT", &zlib);
    write_chunk(&mut out, b"IEND", &[]);
    Some(out)
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

fn wrap_zlib_stored(input: &[u8]) -> Vec<u8> {
    // zlib header: CMF=0x78, FLG=0x01 (default compression level placeholder).
    let mut out = Vec::with_capacity(input.len() + 16);
    out.push(0x78);
    out.push(0x01);
    // Split input into stored deflate blocks of <=65535 bytes.
    let mut i = 0;
    while i < input.len() {
        let chunk_len = (input.len() - i).min(65_535);
        let bfinal = if i + chunk_len == input.len() {
            1u8
        } else {
            0u8
        };
        out.push(bfinal); // BTYPE=00, BFINAL bit
        let len = chunk_len as u16;
        let nlen = !len;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&input[i..i + chunk_len]);
        i += chunk_len;
    }
    let adler = adler32(input);
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

fn adler32(input: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in input {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

struct Crc32 {
    table: [u32; 256],
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        Self {
            table,
            value: 0xFFFF_FFFF,
        }
    }
    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let idx = ((self.value ^ byte as u32) & 0xff) as usize;
            self.value = self.table[idx] ^ (self.value >> 8);
        }
    }
    fn finalize(self) -> u32 {
        self.value ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_encoder_round_trip_signature() {
        let pixels = vec![255u8; 2 * 2 * 4];
        let png = encode_png(2, 2, &pixels).expect("encode");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // IEND chunk's type bytes appear near the end
        let iend_marker = b"IEND";
        let found = png.windows(4).any(|w| w == iend_marker);
        assert!(found, "PNG missing IEND chunk");
    }

    #[test]
    fn png_encoder_rejects_size_mismatch() {
        let pixels = vec![0u8; 7];
        assert!(encode_png(2, 2, &pixels).is_none());
    }

    #[test]
    fn adler32_matches_known_value() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
    }

    #[test]
    fn scale_to_max_passes_small_images_through() {
        let rgba = vec![1u8; 4 * 4 * 4];
        let (w, h, out) = scale_to_max(4, 4, rgba.clone(), 1024);
        assert_eq!((w, h), (4, 4));
        assert_eq!(out.len(), rgba.len());
    }

    #[test]
    fn scale_to_max_downsamples_to_long_side() {
        let w = 4000;
        let h = 2000;
        let rgba = vec![255u8; (w * h * 4) as usize];
        let (nw, nh, out) = scale_to_max(w, h, rgba, 1568);
        assert!(nw.max(nh) <= 1568);
        assert_eq!(out.len() as u32, nw * nh * 4);
    }
}
