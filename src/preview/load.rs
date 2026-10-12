//! Reading a file for a preview: kind detection, capped reads, decoding, directory listing.

use std::fs;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, bail};
use image::{ImageFormat, ImageReader};

use super::{
    Content, DirEntry, ImageData, MAX_IMAGE_BYTES, MAX_IMAGE_SIDE, MAX_TEXT_BYTES, PreviewKind, SNIFF_BYTES, TextData,
};
use crate::text::human_size;

pub(super) fn load(path: &Path) -> (PreviewKind, Content) {
    let ext_kind = kind_from_extension(path);
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => {
            let message = format!("cannot read {}: {err}", path.display());
            return (ext_kind.unwrap_or(PreviewKind::Text), Content::Error(message));
        }
    };
    if meta.is_dir() {
        let content = read_directory(path).map_or_else(error_content, Content::Directory);
        return (PreviewKind::Directory, content);
    }
    if !meta.is_file() {
        let message = format!("cannot preview {}: not a regular file", path.display());
        return (ext_kind.unwrap_or(PreviewKind::Binary), Content::Error(message));
    }
    match ext_kind {
        Some(PreviewKind::Markdown) => {
            let content = read_capped(path, MAX_TEXT_BYTES).map_or_else(error_content, |(bytes, truncated)| {
                let size = meta.len().max(bytes.len() as u64);
                Content::Markdown(String::from_utf8_lossy(&bytes).into_owned(), truncated.then_some(size))
            });
            (PreviewKind::Markdown, content)
        }
        Some(PreviewKind::Image) => (PreviewKind::Image, load_image(path)),
        _ => sniff(path, meta.len()),
    }
}

pub(super) fn kind_from_extension(path: &Path) -> Option<PreviewKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => Some(PreviewKind::Markdown),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(PreviewKind::Image),
        _ => None,
    }
}

/// Classifies a file without a known extension by its content.
pub(super) fn sniff(path: &Path, size: u64) -> (PreviewKind, Content) {
    let (bytes, truncated) = match read_capped(path, MAX_TEXT_BYTES) {
        Ok(read) => read,
        Err(err) => return (PreviewKind::Text, error_content(err)),
    };
    let size = size.max(bytes.len() as u64);
    if is_supported_image(&bytes) {
        return (PreviewKind::Image, load_image(path));
    }
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return (PreviewKind::Binary, Content::Binary(size));
    }
    match decode_utf8(bytes, truncated) {
        Some(text) => {
            let data = TextData::new(&text, truncated.then_some(size));
            (PreviewKind::Text, Content::Text(data))
        }
        None => (PreviewKind::Binary, Content::Binary(size)),
    }
}

pub(super) fn is_supported_image(bytes: &[u8]) -> bool {
    matches!(
        image::guess_format(bytes),
        Ok(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP | ImageFormat::Bmp)
    )
}

/// Decodes UTF-8, tolerating a sequence cut short by the read limit.
pub(super) fn decode_utf8(bytes: Vec<u8>, truncated: bool) -> Option<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) => {
            let utf8 = err.utf8_error();
            if !truncated || utf8.error_len().is_some() {
                return None;
            }
            let mut bytes = err.into_bytes();
            bytes.truncate(utf8.valid_up_to());
            String::from_utf8(bytes).ok()
        }
    }
}

/// Reads at most `cap` bytes; the flag tells whether the file was longer.
pub(super) fn read_capped(path: &Path, cap: u64) -> anyhow::Result<(Vec<u8>, bool)> {
    let context = || format!("cannot read {}", path.display());
    let file = File::open(path).with_context(context)?;
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes).with_context(context)?;
    let truncated = bytes.len() as u64 > cap;
    if truncated {
        bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    }
    Ok((bytes, truncated))
}

pub(super) fn error_content(err: anyhow::Error) -> Content {
    Content::Error(format!("{err:#}"))
}

pub(super) fn load_image(path: &Path) -> Content {
    decode_image(path).map_or_else(error_content, Content::Image)
}

pub(super) fn decode_image(path: &Path) -> anyhow::Result<ImageData> {
    let (bytes, truncated) = read_capped(path, MAX_IMAGE_BYTES)?;
    if truncated {
        bail!("cannot preview {}: image larger than {}", path.display(), human_size(MAX_IMAGE_BYTES));
    }
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader.format();
    let decoded = reader.decode().with_context(|| format!("cannot decode image {}", path.display()))?;
    let (width, height) = (decoded.width(), decoded.height());
    let decoded = if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        decoded.thumbnail(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE)
    } else {
        decoded
    };
    Ok(ImageData { pixels: decoded.into_rgba8(), width, height, format, png: None })
}

pub(super) fn read_directory(path: &Path) -> anyhow::Result<Vec<DirEntry>> {
    let reader = fs::read_dir(path).with_context(|| format!("cannot list {}", path.display()))?;
    let mut entries: Vec<DirEntry> = reader
        .filter_map(Result::ok)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            // Follow symlinks so that links to directories sort as directories.
            let meta = fs::metadata(entry.path()).or_else(|_| entry.metadata()).ok();
            let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
            DirEntry {
                hidden: name.starts_with('.'),
                is_dir,
                size: meta.as_ref().map_or(0, fs::Metadata::len),
                executable: !is_dir && meta.as_ref().is_some_and(is_executable),
                link_target: is_link
                    .then(|| fs::read_link(entry.path()).ok())
                    .flatten()
                    .map(|target| target.display().to_string()),
                name,
            }
        })
        .collect();
    entries.sort_by_cached_key(|e| (e.hidden, !e.is_dir, e.name.to_lowercase(), e.name.clone()));
    Ok(entries)
}

pub(super) fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}
