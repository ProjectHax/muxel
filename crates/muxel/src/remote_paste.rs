//! Pasting a clipboard image or local files into a pane that runs on another host.
//!
//! Locally an image paste is a Ctrl+V the agent answers by reading the clipboard
//! itself, and a file paste is the file's path. Over ssh neither works: the agent
//! reads the *host's* clipboard, which doesn't have the image, and the path names a
//! file on this machine. So for a remote pane each file is copied to the host first
//! (`integrations::upload_paste`) and the path *there* is pasted instead — which an
//! agent such as Claude Code takes as an attached image or a file to read.
//!
//! This module turns a [`LocalPaste`] into the files to copy; the app does the
//! copying and the pasting.

use gpui::{Image, ImageFormat};
use muxel_terminal::LocalPaste;
use std::path::Path;

/// Largest file muxel copies to a host for a paste. Big enough for any screenshot
/// or document an agent would read; small enough that an accidental paste of a
/// video doesn't tie the connection up for minutes.
pub const MAX_PASTE_BYTES: u64 = 25 * 1024 * 1024;

/// One file to copy to the host: its name there and its contents.
#[derive(Debug, PartialEq)]
pub struct PasteFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The files a paste copies to the host, plus a line for each one it can't (a
/// folder, a file too large or unreadable). `unique` prefixes every name, so two
/// pastes never overwrite each other there.
pub fn paste_files(paste: LocalPaste, unique: &str) -> (Vec<PasteFile>, Vec<String>) {
    match paste {
        LocalPaste::Image(image) => {
            let (bytes, ext) = agent_image(image);
            let name = muxel_core::remote_ops::paste_file_name(unique, &format!("image.{ext}"));
            (vec![PasteFile { name, bytes }], Vec::new())
        }
        LocalPaste::Paths(paths) => {
            let mut files = Vec::new();
            let mut skipped = Vec::new();
            for (n, path) in paths.iter().enumerate() {
                let original = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Several files from one paste share a stamp; keep them apart.
                let unique = if paths.len() > 1 {
                    format!("{unique}-{}", n + 1)
                } else {
                    unique.to_string()
                };
                match read_paste_file(path) {
                    Ok(bytes) => files.push(PasteFile {
                        name: muxel_core::remote_ops::paste_file_name(&unique, &original),
                        bytes,
                    }),
                    Err(why) => skipped.push(format!("{}: {why}", path.display())),
                }
            }
            (files, skipped)
        }
    }
}

/// A clipboard image in a form an agent takes, with its extension: PNG, JPEG, GIF
/// and WebP as they are; anything else re-encoded as PNG. A macOS clipboard often
/// holds only a TIFF (copying from Preview, say), which Claude Code won't attach.
/// An image that can't be re-encoded (an SVG) goes as it is.
fn agent_image(image: Image) -> (Vec<u8>, &'static str) {
    let decodable = match image.format {
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::Webp => {
            return (image.bytes, image.format.extension());
        }
        ImageFormat::Tiff => Some(image::ImageFormat::Tiff),
        ImageFormat::Bmp => Some(image::ImageFormat::Bmp),
        ImageFormat::Ico => Some(image::ImageFormat::Ico),
        ImageFormat::Pnm => Some(image::ImageFormat::Pnm),
        ImageFormat::Svg => None,
    };
    let png = decodable.and_then(|format| {
        let decoded = image::load_from_memory_with_format(&image.bytes, format).ok()?;
        let mut out = std::io::Cursor::new(Vec::new());
        decoded.write_to(&mut out, image::ImageFormat::Png).ok()?;
        Some(out.into_inner())
    });
    match png {
        Some(png) => (png, "png"),
        None => (image.bytes, image.format.extension()),
    }
}

/// A local file's contents for a paste, or why it can't be pasted.
fn read_paste_file(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("folders can't be pasted into a remote pane".to_string());
    }
    if meta.len() > MAX_PASTE_BYTES {
        return Err(format!(
            "larger than {} MB",
            MAX_PASTE_BYTES / (1024 * 1024)
        ));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{MAX_PASTE_BYTES, PasteFile, paste_files};
    use gpui::{Image, ImageFormat};
    use muxel_terminal::LocalPaste;

    fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_png_screenshot_goes_as_it_is() {
        let bytes = png_bytes();
        let (files, skipped) = paste_files(
            LocalPaste::Image(Image::from_bytes(ImageFormat::Png, bytes.clone())),
            "u1",
        );
        assert!(skipped.is_empty());
        assert_eq!(
            files,
            vec![PasteFile {
                name: "u1-image.png".into(),
                bytes
            }]
        );
    }

    /// Claude Code won't attach a TIFF, and a macOS clipboard often holds only one.
    #[test]
    fn a_tiff_becomes_a_png() {
        let img = image::RgbaImage::from_pixel(3, 1, image::Rgba([0, 128, 255, 255]));
        let mut tiff = std::io::Cursor::new(Vec::new());
        img.write_to(&mut tiff, image::ImageFormat::Tiff).unwrap();
        let (files, _) = paste_files(
            LocalPaste::Image(Image::from_bytes(ImageFormat::Tiff, tiff.into_inner())),
            "u1",
        );
        assert_eq!(files[0].name, "u1-image.png");
        let back = image::load_from_memory_with_format(&files[0].bytes, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(back, img);
    }

    #[test]
    fn an_svg_goes_as_it_is() {
        let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec();
        let (files, _) = paste_files(
            LocalPaste::Image(Image::from_bytes(ImageFormat::Svg, svg.clone())),
            "u1",
        );
        assert_eq!(files[0].name, "u1-image.svg");
        assert_eq!(files[0].bytes, svg);
    }

    /// A fresh directory for one test, removed when dropped.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir = crate::test_support::short_temp_path();
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn files_are_copied_and_folders_and_huge_files_are_not() {
        let dir = TempDir::new();
        let shot = dir.0.join("Screen Shot 1.png");
        std::fs::write(&shot, b"png").unwrap();
        let notes = dir.0.join("notes.md");
        std::fs::write(&notes, b"# hi").unwrap();
        let big = dir.0.join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_PASTE_BYTES + 1)
            .unwrap();
        let folder = dir.0.join("sub");
        std::fs::create_dir(&folder).unwrap();

        let (files, skipped) = paste_files(LocalPaste::Paths(vec![shot, folder, notes, big]), "u1");
        assert_eq!(
            files,
            vec![
                PasteFile {
                    name: "u1-1-Screen_Shot_1.png".into(),
                    bytes: b"png".to_vec()
                },
                PasteFile {
                    name: "u1-3-notes.md".into(),
                    bytes: b"# hi".to_vec()
                },
            ]
        );
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped[0].contains("folders"), "{skipped:?}");
        assert!(skipped[1].contains("larger than 25 MB"), "{skipped:?}");
    }

    #[test]
    fn a_single_file_keeps_the_plain_stamp() {
        let dir = TempDir::new();
        let shot = dir.0.join("a.jpg");
        std::fs::write(&shot, b"jpg").unwrap();
        let (files, _) = paste_files(LocalPaste::Paths(vec![shot]), "u1");
        assert_eq!(files[0].name, "u1-a.jpg");
    }
}
