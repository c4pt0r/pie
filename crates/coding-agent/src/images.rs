//! Image attachment loader for the CLI's `--image` flag (issue #16).
//!
//! Reads each path, infers mime type from magic bytes, base64-encodes, and returns a
//! `pie_ai::ImageContent` ready to hand to `AgentHarness::prompt_with_images`.

use std::path::Path;

use anyhow::{Context, Result, bail};

pub const MAX_PER_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_IMAGES_PER_MESSAGE: usize = 10;

/// Detect a supported image's mime type from its leading bytes. Supported: PNG, JPEG, WebP,
/// GIF — matches the providers' general intersection.
pub fn infer_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("image/png");
    }
    if bytes.len() >= 3 && &bytes[..3] == b"\xff\xd8\xff" {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    None
}

/// Load a single image into a pie-ai ImageContent. Enforces the size cap.
pub async fn load_one(path: &Path) -> Result<pie_ai::ImageContent> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read image {}", path.display()))?;
    load_bytes(&path.display().to_string(), &bytes)
}

/// Build an image attachment from already-read bytes. Used by both `--image` paths and
/// clipboard paste, so format and size validation stay identical.
pub fn load_bytes(label: &str, bytes: &[u8]) -> Result<pie_ai::ImageContent> {
    if bytes.len() > MAX_PER_IMAGE_BYTES {
        bail!(
            "image {} exceeds {}MB cap ({} bytes)",
            label,
            MAX_PER_IMAGE_BYTES / 1024 / 1024,
            bytes.len()
        );
    }
    let mime = infer_mime(bytes).ok_or_else(|| {
        anyhow::anyhow!(
            "unsupported image format for {}; expected PNG/JPEG/WebP/GIF",
            label
        )
    })?;
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(pie_ai::ImageContent {
        data,
        mime_type: mime.to_string(),
    })
}

/// Load every path. Errors on the first failure so the user gets a clear, surfaceable error
/// instead of a partial attachment list.
pub async fn load_all(paths: &[std::path::PathBuf]) -> Result<Vec<pie_ai::ImageContent>> {
    if paths.len() > MAX_IMAGES_PER_MESSAGE {
        bail!(
            "{} images exceeds per-message cap of {}",
            paths.len(),
            MAX_IMAGES_PER_MESSAGE
        );
    }
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        out.push(load_one(p).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn infer_png() {
        let bytes = b"\x89PNG\r\n\x1a\n0000more";
        assert_eq!(infer_mime(bytes), Some("image/png"));
    }

    #[test]
    fn infer_jpeg() {
        let bytes = b"\xff\xd8\xff\xe000more";
        assert_eq!(infer_mime(bytes), Some("image/jpeg"));
    }

    #[test]
    fn infer_webp() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(b"WEBPmore");
        assert_eq!(infer_mime(&bytes), Some("image/webp"));
    }

    #[test]
    fn rejects_unknown_format() {
        assert!(infer_mime(b"not an image").is_none());
    }

    #[tokio::test]
    async fn load_one_round_trips_a_png() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("x.png");
        // Minimal valid PNG header bytes — enough for magic-byte detection.
        let bytes =
            b"\x89PNG\r\n\x1a\nthe rest is not a real png but this test only checks load + mime";
        std::fs::write(&p, bytes).unwrap();
        let img = load_one(&p).await.unwrap();
        assert_eq!(img.mime_type, "image/png");
        assert!(!img.data.is_empty());

        let from_bytes = load_bytes("clipboard", bytes).unwrap();
        assert_eq!(from_bytes.mime_type, "image/png");
        assert_eq!(from_bytes.data, img.data);
    }

    #[tokio::test]
    async fn load_one_rejects_unknown_format() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("x.bin");
        std::fs::write(&p, b"hello").unwrap();
        let err = load_one(&p).await.unwrap_err().to_string();
        assert!(err.contains("unsupported image format"), "{err}");
    }
}
