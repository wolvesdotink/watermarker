use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, ImageReader, RgbaImage,
};

pub const PHOTO_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp"];

pub const ANCHORS: &[&str] = &[
    "top-left", "top", "top-right",
    "left", "center", "right",
    "bottom-left", "bottom", "bottom-right",
];

/// Default JPEG quality used when callers don't pass an explicit value (e.g.
/// the smoke example). The batch UI exposes a slider that overrides this.
pub const DEFAULT_JPEG_QUALITY: u8 = 90;

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

/// Like `open_photo`, but for JPEG inputs uses jpeg-decoder's IDCT scale to
/// decode at 1/2, 1/4, or 1/8 directly — saving most of the work when we're
/// going to downsample to `max_dim` anyway. Falls back to the full decode
/// path for non-JPEG inputs or if anything in the fast path errors out.
pub fn open_photo_scaled(path: &Path, max_dim: u32) -> Result<DynamicImage, String> {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if (ext == "jpg" || ext == "jpeg") && max_dim > 0 {
        if let Ok(img) = open_jpeg_scaled(path, max_dim) {
            return Ok(img);
        }
    }
    open_photo(path)
}

fn open_jpeg_scaled(path: &Path, max_dim: u32) -> Result<DynamicImage, String> {
    use std::io::BufReader;

    let file = File::open(path).map_err(|e| format!("open jpeg: {e}"))?;
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(file));
    decoder
        .read_info()
        .map_err(|e| format!("read jpeg info: {e}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| "missing jpeg info".to_string())?;

    // EXIF orientation must be read before consuming the decoder. The image
    // crate's parser handles every byte-endian + IFD case for us.
    let orientation = decoder
        .exif_data()
        .and_then(image::metadata::Orientation::from_exif_chunk)
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let orig_w = info.width as u32;
    let orig_h = info.height as u32;
    let long_edge = orig_w.max(orig_h);

    // Pick the largest divisor in {1, 2, 4, 8} that keeps the long edge >=
    // max_dim. A later bilinear/Lanczos resize lands on the exact preview size.
    let mut divisor: u32 = 8;
    while divisor > 1 && long_edge / divisor < max_dim {
        divisor /= 2;
    }
    let req_w = (orig_w / divisor).max(1).min(u16::MAX as u32) as u16;
    let req_h = (orig_h / divisor).max(1).min(u16::MAX as u32) as u16;

    let (out_w, out_h) = decoder
        .scale(req_w, req_h)
        .map_err(|e| format!("scale jpeg: {e}"))?;
    let pixels = decoder
        .decode()
        .map_err(|e| format!("decode jpeg: {e}"))?;

    let mut img = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let buf = image::RgbImage::from_raw(out_w as u32, out_h as u32, pixels)
                .ok_or_else(|| "RgbImage from_raw failed".to_string())?;
            DynamicImage::ImageRgb8(buf)
        }
        jpeg_decoder::PixelFormat::L8 => {
            let buf = image::GrayImage::from_raw(out_w as u32, out_h as u32, pixels)
                .ok_or_else(|| "GrayImage from_raw failed".to_string())?;
            DynamicImage::ImageLuma8(buf)
        }
        // CMYK / 16-bit grayscale are unusual for camera JPEGs; let the
        // fallback path handle them so we don't have to support every variant.
        _ => return Err("unsupported jpeg pixel format".into()),
    };

    img.apply_orientation(orientation);
    Ok(img)
}

/// A parsed watermark ready to be rasterized at any target width.
/// Built once at the start of a batch and shared across photos.
pub enum WatermarkSource {
    /// SVG kept as a parsed usvg tree so we can re-rasterize at any width.
    /// Boxed because `Tree` is several hundred bytes and the enum is passed
    /// around frequently — keeps it pointer-sized.
    Svg(Box<resvg::usvg::Tree>),
    /// PNG/other raster watermark held at its source resolution.
    Raster(RgbaImage),
}

impl WatermarkSource {
    pub fn load(path: &Path) -> Result<Self, String> {
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if ext == "svg" {
            let data = fs::read(path).map_err(|e| format!("read svg: {e}"))?;
            let opt = resvg::usvg::Options::default();
            let tree = resvg::usvg::Tree::from_data(&data, &opt)
                .map_err(|e| format!("parse svg: {e}"))?;
            Ok(WatermarkSource::Svg(Box::new(tree)))
        } else {
            let img = image::open(path).map_err(|e| format!("open watermark: {e}"))?;
            Ok(WatermarkSource::Raster(img.to_rgba8()))
        }
    }
}

/// Resize a parsed watermark to exactly `target_w` pixels wide. Height keeps
/// the source aspect ratio. Opacity is applied separately so this can be cached
/// by target width.
pub fn resize_watermark(source: &WatermarkSource, target_w: u32) -> Result<RgbaImage, String> {
    let target_w = target_w.max(1);
    match source {
        WatermarkSource::Svg(tree) => rasterize_svg_tree(tree, target_w),
        WatermarkSource::Raster(img) => {
            let (w, h) = (img.width(), img.height());
            let ratio = target_w as f32 / w as f32;
            let new_h = ((h as f32) * ratio).round().max(1.0) as u32;
            Ok(image::imageops::resize(img, target_w, new_h, FilterType::Lanczos3))
        }
    }
}

fn rasterize_svg_tree(
    tree: &resvg::usvg::Tree,
    target_w: u32,
) -> Result<RgbaImage, String> {
    let svg_size = tree.size();
    let scale = target_w as f32 / svg_size.width();
    let out_w = target_w.max(1);
    let out_h = ((svg_size.height() * scale).round() as u32).max(1);

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h)
        .ok_or_else(|| "tiny_skia: failed to allocate pixmap".to_string())?;
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(tree, transform, &mut pixmap.as_mut());

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

/// Path-based one-shot loader. Parses then resizes. Kept so the smoke example
/// and the preview path still have a single-call entry point.
pub fn load_watermark(path: &Path, target_width_px: u32) -> Result<RgbaImage, String> {
    let source = WatermarkSource::load(path)?;
    resize_watermark(&source, target_width_px)
}

/// Bounded, thread-safe cache of resized watermarks keyed by target width.
/// Most folders contain photos of one or two distinct widths, so after the
/// first photo of each width subsequent lookups are a hashmap hit + RGBA clone.
/// Internal mutability via `Mutex` lets it be shared across `rayon` workers
/// without juggling per-thread state.
pub struct WatermarkCache {
    inner: Mutex<CacheInner>,
    cap: usize,
}

struct CacheInner {
    entries: HashMap<u32, RgbaImage>,
    order: Vec<u32>,
}

impl WatermarkCache {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            inner: Mutex::new(CacheInner {
                entries: HashMap::with_capacity(cap),
                order: Vec::with_capacity(cap),
            }),
            cap,
        }
    }

    /// Fetch a resized watermark, computing it if missing. The expensive
    /// resize/rasterize happens outside the lock so other threads aren't
    /// blocked on it. Returns an owned clone the caller can mutate.
    pub fn get_or_insert(
        &self,
        source: &WatermarkSource,
        target_w: u32,
    ) -> Result<RgbaImage, String> {
        // Fast path — read with the lock held briefly.
        if let Ok(inner) = self.inner.lock() {
            if let Some(img) = inner.entries.get(&target_w) {
                return Ok(img.clone());
            }
        }
        // Slow path — compute outside the lock, then publish.
        let resized = resize_watermark(source, target_w)?;
        let mut inner = self.inner.lock().map_err(|_| "watermark cache poisoned".to_string())?;
        // Re-check: another thread may have inserted while we were resizing.
        if !inner.entries.contains_key(&target_w) {
            if inner.entries.len() >= self.cap {
                if let Some(oldest) = inner.order.first().copied() {
                    inner.entries.remove(&oldest);
                    inner.order.remove(0);
                }
            }
            inner.entries.insert(target_w, resized.clone());
            inner.order.push(target_w);
        }
        Ok(resized)
    }
}

/// Scale alpha in-place. Walks the raw byte slice and uses fixed-point math
/// (`(a * o) >> 8` where `o` is in `[0, 256]`) to avoid float work per pixel.
pub fn apply_opacity(img: &mut RgbaImage, opacity: f32) {
    if opacity >= 1.0 {
        return;
    }
    let clamped = opacity.clamp(0.0, 1.0);
    // o ∈ [0, 256]; o = 256 yields the identity multiply via the shift.
    let o = (clamped * 256.0).round().min(256.0) as u32;
    for chunk in img.as_mut().chunks_exact_mut(4) {
        chunk[3] = ((chunk[3] as u32 * o) >> 8) as u8;
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
    jpeg_quality: u8,
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
        "jpg" | "jpeg" => save_jpeg(composed, out_path, jpeg_quality),
        _ => composed.save(out_path).map_err(|e| format!("save image: {e}")),
    }
}

fn save_jpeg(composed: &RgbaImage, out_path: &Path, quality: u8) -> Result<(), String> {
    let (w, h) = (composed.width(), composed.height());
    // Drop alpha into a single RGB buffer in one pass — avoids the
    // DynamicImage::ImageRgba8(_.clone()).to_rgb8() double allocation.
    let raw = composed.as_raw();
    let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for chunk in raw.chunks_exact(4) {
        rgb.push(chunk[0]);
        rgb.push(chunk[1]);
        rgb.push(chunk[2]);
    }
    let file = File::create(out_path).map_err(|e| format!("create jpeg: {e}"))?;
    let writer = BufWriter::new(file);
    let encoder = JpegEncoder::new_with_quality(writer, quality.clamp(1, 100));
    encoder
        .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
        .map_err(|e| format!("encode jpeg: {e}"))
}

/// Convenience wrapper: parse the watermark from disk for a single photo.
/// Used by the smoke example and any callers that don't have a parsed source.
pub fn watermark_photo(
    photo_path: &Path,
    watermark_src: &Path,
    out_path: &Path,
    size_pct: f32,
    anchor: &str,
    margin: i64,
    opacity: f32,
) -> Result<(), String> {
    let source = WatermarkSource::load(watermark_src)?;
    watermark_photo_with_source(
        photo_path,
        &source,
        out_path,
        size_pct,
        anchor,
        margin,
        opacity,
        DEFAULT_JPEG_QUALITY,
        None,
    )
}

/// Watermark a single photo using a pre-parsed source plus an optional resize
/// cache. Hot path for the batch loop — the SVG/PNG decode happens once for
/// the entire run instead of N times.
#[allow(clippy::too_many_arguments)]
pub fn watermark_photo_with_source(
    photo_path: &Path,
    source: &WatermarkSource,
    out_path: &Path,
    size_pct: f32,
    anchor: &str,
    margin: i64,
    opacity: f32,
    jpeg_quality: u8,
    cache: Option<&WatermarkCache>,
) -> Result<(), String> {
    let photo = open_photo(photo_path)?;
    let target_w = ((photo.width() as f32 * size_pct / 100.0).round() as u32).max(1);
    let mut wm = match cache {
        Some(c) => c.get_or_insert(source, target_w)?,
        None => resize_watermark(source, target_w)?,
    };
    apply_opacity(&mut wm, opacity);
    let composed = compose(&photo, &wm, anchor, margin);
    save_composed(&composed, photo_path, out_path, jpeg_quality)
}

/// Compose a preview-sized image (thumbnail of the photo + scaled watermark).
/// Returns RGBA bytes that the caller can encode to PNG/JPEG.
pub fn preview_image(
    photo_path: &Path,
    watermark_src: &Path,
    size_pct: f32,
    anchor: &str,
    margin: i64,
    opacity: f32,
    max_dim: u32,
) -> Result<RgbaImage, String> {
    // Scale-on-decode for JPEG sources: a 24 MP source decodes 16–64x faster
    // when we know we only need a 720 px thumbnail.
    let photo = open_photo_scaled(photo_path, max_dim)?;
    let thumb = photo.thumbnail(max_dim, max_dim);
    let target_w = ((thumb.width() as f32 * size_pct / 100.0).round() as u32).max(1);
    let source = WatermarkSource::load(watermark_src)?;
    let mut wm = resize_watermark(&source, target_w)?;
    apply_opacity(&mut wm, opacity);
    Ok(compose(&thumb, &wm, anchor, margin))
}
