//! Decoding and resizing. Nothing here depends on gpui, so it can run on any thread.

use anyhow::{Result, bail};
use image::{DynamicImage, RgbaImage, imageops::FilterType};

/// Longest side of a stored cover thumbnail, in pixels.
pub const COVER_MAX_W: u32 = 400;
pub const COVER_MAX_H: u32 = 600;

/// Refuse absurd dimensions before allocating (decompression bombs).
const MAX_PIXELS: u64 = 120_000_000;

fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let img = image::load_from_memory(bytes)?;
    if u64::from(img.width()) * u64::from(img.height()) > MAX_PIXELS {
        bail!("image is too large ({}x{})", img.width(), img.height());
    }
    Ok(img)
}

/// Decode a page and scale it down (never up) so it fits inside `max_w` x `max_h`.
///
/// Pre-scaling on the CPU with a good filter looks much sharper than letting the GPU
/// minify a 3000px scan with plain bilinear sampling, and it keeps VRAM use low.
///
/// The returned buffer is already **BGRA**, the channel order gpui expects.
pub fn decode_fit_bgra(bytes: &[u8], max_w: u32, max_h: u32) -> Result<RgbaImage> {
    let img = decode(bytes)?;
    let (w, h) = (img.width(), img.height());
    let scale = (max_w as f32 / w as f32).min(max_h as f32 / h as f32);

    let img = if scale < 1.0 {
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        img.resize_exact(nw, nh, FilterType::CatmullRom)
    } else {
        img
    };

    let mut rgba = img.into_rgba8();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(rgba)
}

/// Encode a JPEG thumbnail that fits inside `max_w` x `max_h`, keeping the aspect ratio.
pub fn make_cover_jpeg(bytes: &[u8], max_w: u32, max_h: u32) -> Result<Vec<u8>> {
    let img = decode(bytes)?;
    let thumb = img.resize(max_w, max_h, FilterType::Lanczos3);
    let rgb = DynamicImage::ImageRgb8(thumb.to_rgb8());

    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 88);
    rgb.write_with_encoder(encoder)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba};
    use std::io::Cursor;

    fn png(w: u32, h: u32, px: Rgba<u8>) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, px);
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn scales_down_and_swaps_to_bgra() {
        let bytes = png(400, 800, Rgba([10, 20, 30, 255]));
        let out = decode_fit_bgra(&bytes, 100, 100).unwrap();
        assert_eq!(out.dimensions(), (50, 100));
        assert_eq!(out.get_pixel(0, 0).0, [30, 20, 10, 255]);
    }

    #[test]
    fn never_upscales() {
        let bytes = png(40, 60, Rgba([1, 2, 3, 255]));
        let out = decode_fit_bgra(&bytes, 4000, 4000).unwrap();
        assert_eq!(out.dimensions(), (40, 60));
    }

    #[test]
    fn cover_is_valid_jpeg() {
        let bytes = png(1000, 1500, Rgba([200, 100, 50, 255]));
        let jpg = make_cover_jpeg(&bytes, COVER_MAX_W, COVER_MAX_H).unwrap();
        assert_eq!(&jpg[..2], &[0xff, 0xd8]);
        let back = image::load_from_memory(&jpg).unwrap();
        assert!(back.width() <= COVER_MAX_W && back.height() <= COVER_MAX_H);
    }
}
