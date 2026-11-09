//! Server appearance: the icon file (`server-icon.png`) Minecraft shows in the
//! multiplayer list. The MOTD and resource pack URL live in
//! `server.properties` and are handled by [`crate::properties`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// File name vanilla and Bukkit-compatible servers load the icon from.
pub const SERVER_ICON_FILE: &str = "server-icon.png";
/// Edge length of a server icon; other sizes are scaled to fit (aspect kept).
pub const ICON_SIZE: u32 = 64;

/// Path of the server's icon file inside its working directory.
pub fn server_icon_path(working_dir: &Path) -> PathBuf {
    working_dir.join(SERVER_ICON_FILE)
}

/// Reads the raw icon file bytes, or `None` when the server has no icon.
pub fn read_server_icon(working_dir: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(server_icon_path(working_dir)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Decodes an image (PNG, JPEG, …), scales it onto a [`ICON_SIZE`]² canvas
/// preserving aspect ratio, and saves it as the server's icon. Existing icons
/// are overwritten; the working directory is created when missing.
pub fn write_server_icon(bytes: &[u8], working_dir: &Path) -> io::Result<()> {
    let image = image::load_from_memory(bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("invalid image: {err}")))?
        .to_rgba8();
    let icon = fit_square(&image, ICON_SIZE);
    fs::create_dir_all(working_dir)?;
    icon.save(server_icon_path(working_dir))
        .map_err(|err| io::Error::other(format!("could not save icon: {err}")))
}

/// Removes the server's icon; succeeds when there is none.
pub fn clear_server_icon(working_dir: &Path) -> io::Result<()> {
    match fs::remove_file(server_icon_path(working_dir)) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Scales `img` to fit inside a `size`×`size` canvas, centered on transparent pixels.
pub fn fit_square(img: &image::RgbaImage, size: u32) -> image::RgbaImage {
    let scale = size as f32 / img.width().max(img.height()) as f32;
    let w = ((img.width() as f32 * scale).round() as u32).max(1);
    let h = ((img.height() as f32 * scale).round() as u32).max(1);
    let scaled = image::imageops::resize(img, w, h, image::imageops::FilterType::Lanczos3);
    let mut canvas = image::RgbaImage::new(size, size);
    image::imageops::overlay(
        &mut canvas,
        &scaled,
        ((size - w) / 2) as i64,
        ((size - h) / 2) as i64,
    );
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::new_id;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spoon-icon-test-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, image::Rgba([10, 200, 30, 255]))
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        cursor.into_inner()
    }

    #[test]
    fn imports_and_resizes_to_square_icon() {
        let dir = temp_dir();
        // Wide image: must be letterboxed onto a 64x64 canvas.
        write_server_icon(&png_bytes(128, 64), &dir).unwrap();
        let saved = fs::read(server_icon_path(&dir)).unwrap();
        let decoded = image::load_from_memory(&saved).unwrap();
        assert_eq!(decoded.width(), ICON_SIZE);
        assert_eq!(decoded.height(), ICON_SIZE);
        assert_eq!(read_server_icon(&dir).unwrap(), Some(saved));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_image_bytes() {
        let dir = temp_dir();
        let err = write_server_icon(b"not an image", &dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!server_icon_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_icon_is_idempotent() {
        let dir = temp_dir();
        clear_server_icon(&dir).unwrap();
        write_server_icon(&png_bytes(32, 32), &dir).unwrap();
        clear_server_icon(&dir).unwrap();
        assert_eq!(read_server_icon(&dir).unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fit_square_preserves_aspect_ratio() {
        let wide = image::RgbaImage::new(200, 50);
        let fitted = fit_square(&wide, 64);
        assert_eq!((fitted.width(), fitted.height()), (64, 64));
    }
}
