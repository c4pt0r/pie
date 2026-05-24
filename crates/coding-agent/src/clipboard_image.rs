//! Clipboard integration for the full-screen TUI.
//!
//! Terminal paste events only carry text. For image paste we need to query the system clipboard
//! directly, then convert the platform RGBA buffer into the same `ImageContent` shape used by the
//! existing `--image` flag.

use std::io::Cursor;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct ClipboardImage {
    pub image: pie_ai::ImageContent,
    pub width: usize,
    pub height: usize,
    pub encoded_bytes: usize,
}

#[derive(Clone, Debug)]
pub enum ClipboardPaste {
    Image(ClipboardImage),
    Text(String),
    Empty,
}

pub async fn read_clipboard() -> Result<ClipboardPaste> {
    tokio::task::spawn_blocking(read_clipboard_sync)
        .await
        .context("clipboard task failed")?
}

fn read_clipboard_sync() -> Result<ClipboardPaste> {
    let mut clipboard = arboard::Clipboard::new().context("open clipboard")?;
    if let Ok(image) = clipboard.get_image() {
        return Ok(ClipboardPaste::Image(encode_rgba_clipboard_image(
            image.width,
            image.height,
            image.bytes.into_owned(),
        )?));
    }
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            return Ok(ClipboardPaste::Text(text));
        }
    }
    Ok(ClipboardPaste::Empty)
}

pub(crate) fn encode_rgba_clipboard_image(
    width: usize,
    height: usize,
    rgba_bytes: Vec<u8>,
) -> Result<ClipboardImage> {
    let expected = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(4))
        .context("clipboard image dimensions are too large")?;
    if rgba_bytes.len() != expected {
        bail!(
            "clipboard image has invalid RGBA buffer: expected {expected} bytes, got {}",
            rgba_bytes.len()
        );
    }

    let width_u32 = u32::try_from(width).context("clipboard image width is too large")?;
    let height_u32 = u32::try_from(height).context("clipboard image height is too large")?;
    let rgba = image::RgbaImage::from_raw(width_u32, height_u32, rgba_bytes)
        .context("clipboard image buffer does not match dimensions")?;
    let mut cursor = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .context("encode clipboard image as PNG")?;
    let png = cursor.into_inner();
    let encoded_bytes = png.len();
    let image = crate::images::load_bytes("clipboard image", &png)?;

    Ok(ClipboardImage {
        image,
        width,
        height,
        encoded_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn encodes_rgba_clipboard_image_as_png() {
        let img = encode_rgba_clipboard_image(
            1,
            1,
            vec![255, 0, 0, 255], // one opaque red pixel
        )
        .unwrap();

        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.image.mime_type, "image/png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(img.image.data)
            .unwrap();
        assert!(decoded.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn rejects_invalid_rgba_buffer_size() {
        let err = encode_rgba_clipboard_image(2, 2, vec![0; 3])
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid RGBA buffer"), "{err}");
    }
}
