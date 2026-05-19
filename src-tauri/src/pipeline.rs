use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageReader, RgbaImage};

pub const PHOTO_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp"];

pub const ANCHORS: &[&str] = &[
    "top-left", "top", "top-right",
    "left", "center", "right",
    "bottom-left", "bottom", "bottom-right",
];

pub fn is_photo(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|e| PHOTO_EXTS.iter().any(|p| p.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Walks `root` and all subdirectories, collecting photos.
/// Skips dot-folders (`.git`, `.DS_Store`, etc.) and does not follow symlinks
/// so we can't infinite-loop on a self-referential link.
pub fn list_photos(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.filter_map(Result::ok) {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                let is_hidden = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with('.'))
                    .unwrap_or(false);
                if !is_hidden {
                    stack.push(path);
                }
            } else if ft.is_file() && is_photo(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub fn open_photo(path: &Path) -> Result<DynamicImage, String> {
    // ImageReader handles format guessing; orientation is read from the decoder
    // before we hand it off, then re-applied so portrait phone photos land right-side up.
    let reader = ImageReader::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("guess format {}: {e}", path.display()))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| format!("decoder {}: {e}", path.display()))?;
    let orientation = image::ImageDecoder::orientation(&mut decoder)
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("decode {}: {e}", path.display()))?;
    img.apply_orientation(orientation);
    Ok(img)
}

pub fn load_watermark(path: &Path, target_width_px: u32) -> Result<RgbaImage, String> {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if ext == "svg" {
        rasterize_svg(path, target_width_px)
    } else {
        let img = image::open(path).map_err(|e| format!("open watermark: {e}"))?;
        let (w, h) = img.dimensions();
        let ratio = target_width_px as f32 / w as f32;
        let new_h = ((h as f32) * ratio).round().max(1.0) as u32;
        let resized = img.resize_exact(target_width_px, new_h, FilterType::Lanczos3);
        Ok(resized.to_rgba8())
    }
}

fn rasterize_svg(path: &Path, target_width_px: u32) -> Result<RgbaImage, String> {
    let data = fs::read(path).map_err(|e| format!("read svg: {e}"))?;
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&data, &opt)
        .map_err(|e| format!("parse svg: {e}"))?;

    let svg_size = tree.size();
    let scale = target_width_px as f32 / svg_size.width();
    let out_w = target_width_px.max(1);
    let out_h = ((svg_size.height() * scale).round() as u32).max(1);

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h)
        .ok_or_else(|| "tiny_skia: failed to allocate pixmap".to_string())?;
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny_skia pixmaps are premultiplied RGBA; image's overlay expects straight RGBA.
    let mut data = pixmap.take();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            px[0] = ((px[0] as u32 * 255) / a).min(255) as u8;
            px[1] = ((px[1] as u32 * 255) / a).min(255) as u8;
            px[2] = ((px[2] as u32 * 255) / a).min(255) as u8;
        }
    }
    RgbaImage::from_raw(out_w, out_h, data)
        .ok_or_else(|| "RgbaImage::from_raw failed".to_string())
}

pub fn apply_opacity(img: &mut RgbaImage, opacity: f32) {
    if opacity >= 1.0 {
        return;
    }
    let clamped = opacity.clamp(0.0, 1.0);
    for px in img.pixels_mut() {
        px[3] = ((px[3] as f32) * clamped).round() as u8;
    }
}

pub fn compute_position(
    photo: (u32, u32),
    wm: (u32, u32),
    anchor: &str,
    margin: i64,
) -> (i64, i64) {
    let (pw, ph) = (photo.0 as i64, photo.1 as i64);
    let (ww, wh) = (wm.0 as i64, wm.1 as i64);

    let x = if anchor.contains("left") {
        margin
    } else if anchor.contains("right") {
        pw - ww - margin
    } else {
        (pw - ww) / 2
    };
    let y = if anchor.starts_with("top") {
        margin
    } else if anchor.starts_with("bottom") {
        ph - wh - margin
    } else {
        (ph - wh) / 2
    };
    (x, y)
}

pub fn compose(
    photo: &DynamicImage,
    watermark: &RgbaImage,
    anchor: &str,
    margin: i64,
) -> RgbaImage {
    let mut base = photo.to_rgba8();
    let (x, y) = compute_position(photo.dimensions(), watermark.dimensions(), anchor, margin);
    image::imageops::overlay(&mut base, watermark, x, y);
    base
}

pub fn save_composed(
    composed: &RgbaImage,
    original_path: &Path,
    out_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create output dir: {e}"))?;
    }
    let ext = original_path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    match ext.as_str() {
        "jpg" | "jpeg" => {
            // JPEG has no alpha; flatten to RGB before saving.
            let rgb = DynamicImage::ImageRgba8(composed.clone()).to_rgb8();
            rgb.save(out_path).map_err(|e| format!("save jpeg: {e}"))
        }
        _ => composed.save(out_path).map_err(|e| format!("save image: {e}")),
    }
}

pub fn watermark_photo(
    photo_path: &Path,
    watermark_src: &Path,
    out_path: &Path,
    size_pct: f32,
    anchor: &str,
    margin: i64,
    opacity: f32,
) -> Result<(), String> {
    let photo = open_photo(photo_path)?;
    let target_w = ((photo.width() as f32 * size_pct / 100.0).round() as u32).max(1);
    let mut wm = load_watermark(watermark_src, target_w)?;
    apply_opacity(&mut wm, opacity);
    let composed = compose(&photo, &wm, anchor, margin);
    save_composed(&composed, photo_path, out_path)
}

/// Compose a preview-sized image (thumbnail of the photo + scaled watermark).
/// Returns RGBA bytes that the caller can encode to PNG.
pub fn preview_image(
    photo_path: &Path,
    watermark_src: &Path,
    size_pct: f32,
    anchor: &str,
    margin: i64,
    opacity: f32,
    max_dim: u32,
) -> Result<RgbaImage, String> {
    let photo = open_photo(photo_path)?;
    // Thumbnail keeps preview snappy and lets the watermark scale match the user's view.
    let thumb = photo.thumbnail(max_dim, max_dim);
    let target_w = ((thumb.width() as f32 * size_pct / 100.0).round() as u32).max(1);
    let mut wm = load_watermark(watermark_src, target_w)?;
    apply_opacity(&mut wm, opacity);
    Ok(compose(&thumb, &wm, anchor, margin))
}
