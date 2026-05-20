pub mod commands;
pub mod pipeline;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbaImage};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tauri_plugin_dialog::DialogExt;

// Quality used for the on-the-fly preview JPEG. Independent of the user-facing
// batch quality slider — preview just needs to look right at 720 px.
const PREVIEW_JPEG_QUALITY: u8 = 80;

// Single-slot caches keyed on (path, mtime, max_dim). Reusing a decoded photo
// across slider drags drops most of the per-update cost; we re-decode only on
// folder change, file edit, or preview size change.
struct PreviewCache {
    photo: Option<CachedPhoto>,
    watermark: Option<CachedWatermark>,
}

struct CachedPhoto {
    path: PathBuf,
    mtime: SystemTime,
    max_dim: u32,
    thumb: Arc<DynamicImage>,
}

struct CachedWatermark {
    path: PathBuf,
    mtime: SystemTime,
    source: Arc<pipeline::WatermarkSource>,
}

static PREVIEW_CACHE: OnceLock<Mutex<PreviewCache>> = OnceLock::new();

fn preview_cache() -> &'static Mutex<PreviewCache> {
    PREVIEW_CACHE.get_or_init(|| {
        Mutex::new(PreviewCache {
            photo: None,
            watermark: None,
        })
    })
}

fn mtime_of(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn cached_photo_thumb(path: &Path, max_dim: u32) -> Result<Arc<DynamicImage>, String> {
    let mtime = mtime_of(path);
    if let Ok(lock) = preview_cache().lock() {
        if let Some(entry) = &lock.photo {
            if entry.path == path && entry.mtime == mtime && entry.max_dim == max_dim {
                return Ok(entry.thumb.clone());
            }
        }
    }
    let img = pipeline::open_photo_scaled(path, max_dim)?;
    let thumb = Arc::new(img.thumbnail(max_dim, max_dim));
    if let Ok(mut lock) = preview_cache().lock() {
        lock.photo = Some(CachedPhoto {
            path: path.to_path_buf(),
            mtime,
            max_dim,
            thumb: thumb.clone(),
        });
    }
    Ok(thumb)
}

fn cached_watermark(path: &Path) -> Result<Arc<pipeline::WatermarkSource>, String> {
    let mtime = mtime_of(path);
    if let Ok(lock) = preview_cache().lock() {
        if let Some(entry) = &lock.watermark {
            if entry.path == path && entry.mtime == mtime {
                return Ok(entry.source.clone());
            }
        }
    }
    let src = Arc::new(pipeline::WatermarkSource::load(path)?);
    if let Ok(mut lock) = preview_cache().lock() {
        lock.watermark = Some(CachedWatermark {
            path: path.to_path_buf(),
            mtime,
            source: src.clone(),
        });
    }
    Ok(src)
}

/// Encode an RGBA buffer as JPEG, flattening alpha onto a white background.
/// Watermarked composites are almost entirely opaque (alpha=255) so the slow
/// blend path runs only on the watermark's anti-aliased edges.
fn encode_preview_jpeg(composed: &RgbaImage) -> Result<Vec<u8>, String> {
    let (w, h) = (composed.width(), composed.height());
    let pixels = (w as usize) * (h as usize);
    let mut rgb = Vec::with_capacity(pixels * 3);
    for chunk in composed.as_raw().chunks_exact(4) {
        let a = chunk[3];
        if a == 255 {
            rgb.push(chunk[0]);
            rgb.push(chunk[1]);
            rgb.push(chunk[2]);
        } else if a == 0 {
            rgb.push(255);
            rgb.push(255);
            rgb.push(255);
        } else {
            let a16 = a as u16;
            let inv = 255 - a16;
            rgb.push(((chunk[0] as u16 * a16 + 255 * inv + 127) / 255) as u8);
            rgb.push(((chunk[1] as u16 * a16 + 255 * inv + 127) / 255) as u8);
            rgb.push(((chunk[2] as u16 * a16 + 255 * inv + 127) / 255) as u8);
        }
    }
    let mut buf = Vec::with_capacity(pixels);
    let encoder = JpegEncoder::new_with_quality(&mut buf, PREVIEW_JPEG_QUALITY);
    encoder
        .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
        .map_err(|e| format!("encode preview jpeg: {e}"))?;
    Ok(buf)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewArgs {
    folder: String,
    watermark: String,
    size: f32,
    opacity: f32,
    margin: i64,
    position: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunArgs {
    folder: String,
    watermark: String,
    output: String,
    size: f32,
    opacity: f32,
    margin: i64,
    position: String,
    /// JPEG output quality (1..=100). Defaults to the pipeline default when
    /// missing so older frontend builds keep working.
    #[serde(default = "default_jpeg_quality")]
    jpeg_quality: u8,
}

fn default_jpeg_quality() -> u8 {
    pipeline::DEFAULT_JPEG_QUALITY
}

#[derive(Serialize, Clone)]
struct Failure {
    name: String,
    error: String,
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RunEvent {
    Start { total: usize },
    Progress {
        i: usize,
        name: String,
        ok: bool,
        error: Option<String>,
    },
    Done {
        total: usize,
        failures: Vec<Failure>,
        output: String,
    },
    Error { message: String },
}

fn validate_position(p: &str) -> Result<(), String> {
    if pipeline::ANCHORS.contains(&p) {
        Ok(())
    } else {
        Err(format!("Invalid position: {p}"))
    }
}

#[tauri::command]
async fn preview(args: PreviewArgs) -> Result<tauri::ipc::Response, String> {
    validate_position(&args.position)?;

    // Image work is CPU-bound; keep it off the async runtime.
    tauri::async_runtime::spawn_blocking(move || -> Result<tauri::ipc::Response, String> {
        let folder = PathBuf::from(&args.folder);
        let photos = pipeline::list_photos(&folder);
        let photo_path = photos
            .first()
            .ok_or_else(|| "No photos in folder".to_string())?;
        let wm_path = PathBuf::from(&args.watermark);
        if !wm_path.is_file() {
            return Err("Watermark file not found".into());
        }

        // Cache hits skip both the decode and the SVG parse — the common case
        // during slider drags.
        let thumb = cached_photo_thumb(photo_path, 720)?;
        let source = cached_watermark(&wm_path)?;

        let target_w = ((thumb.width() as f32 * args.size / 100.0).round() as u32).max(1);
        let mut wm = pipeline::resize_watermark(&source, target_w)?;
        pipeline::apply_opacity(&mut wm, args.opacity);
        let composed = pipeline::compose(&thumb, &wm, &args.position, args.margin);

        let bytes = encode_preview_jpeg(&composed)?;
        Ok(tauri::ipc::Response::new(bytes))
    })
    .await
    .map_err(|e| format!("preview task: {e}"))?
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PhotoPreviewArgs {
    folder: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountArgs {
    folder: String,
}

/// Recursive count of watermarkable photos in the folder.
#[tauri::command]
async fn count_photos(args: CountArgs) -> usize {
    tauri::async_runtime::spawn_blocking(move || {
        pipeline::list_photos(&PathBuf::from(&args.folder)).len()
    })
    .await
    .unwrap_or(0)
}

/// Returns just the first photo in the folder (recursively) as a thumbnailed
/// JPEG — used to populate the canvas before a watermark is chosen.
#[tauri::command]
async fn photo_preview(args: PhotoPreviewArgs) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<tauri::ipc::Response, String> {
        let folder = PathBuf::from(&args.folder);
        let photos = pipeline::list_photos(&folder);
        let photo_path = photos
            .first()
            .ok_or_else(|| "No photos in folder".to_string())?;

        let thumb = cached_photo_thumb(photo_path, 720)?;
        let rgba = thumb.to_rgba8();
        let bytes = encode_preview_jpeg(&rgba)?;
        Ok(tauri::ipc::Response::new(bytes))
    })
    .await
    .map_err(|e| format!("preview task: {e}"))?
}

#[tauri::command]
async fn run_batch(app: AppHandle, args: RunArgs) -> Result<(), String> {
    let app_for_task = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_batch_inner(app_for_task, args);
    })
    .await
    .map_err(|e| format!("run task: {e}"))?;
    Ok(())
}

fn run_batch_inner(app: AppHandle, args: RunArgs) {
    let emit = |ev: RunEvent| {
        let _ = app.emit("run", ev);
    };

    if let Err(msg) = validate_position(&args.position) {
        emit(RunEvent::Error { message: msg });
        return;
    }

    let folder = PathBuf::from(&args.folder);
    let wm_path = PathBuf::from(&args.watermark);
    let output = PathBuf::from(&args.output);

    if !wm_path.is_file() {
        emit(RunEvent::Error {
            message: "Watermark file not found".into(),
        });
        return;
    }
    let photos = pipeline::list_photos(&folder);
    if photos.is_empty() {
        emit(RunEvent::Error {
            message: "No photos in folder".into(),
        });
        return;
    }

    // Parse the watermark once for the whole batch (SVG XML parse + tiny_skia
    // rasterize would otherwise repeat per photo).
    let source = match pipeline::WatermarkSource::load(&wm_path) {
        Ok(s) => s,
        Err(e) => {
            emit(RunEvent::Error { message: e });
            return;
        }
    };

    let total = photos.len();
    emit(RunEvent::Start { total });

    // Shared, lock-protected resize cache (most folders have 1–2 widths).
    let cache = pipeline::WatermarkCache::new(8);
    let done = AtomicUsize::new(0);
    let failures: Mutex<Vec<Failure>> = Mutex::new(Vec::new());

    photos.par_iter().for_each(|photo| {
        // Mirror the source's subfolder structure under the output root so
        // that same-named files in different subfolders don't overwrite each
        // other.
        let rel = photo.strip_prefix(&folder).unwrap_or(photo.as_path());
        let display_name = rel.to_string_lossy().into_owned();
        let out_path = output.join(rel);

        let result = pipeline::watermark_photo_with_source(
            photo,
            &source,
            &out_path,
            args.size,
            &args.position,
            args.margin,
            args.opacity,
            args.jpeg_quality,
            Some(&cache),
        );
        // `i` is the completion count now (workers finish out of order). The
        // frontend uses it only to drive the <progress> bar, so monotonicity
        // is what matters, not strict source-order pairing with `name`.
        let i = done.fetch_add(1, Ordering::Relaxed) + 1;
        match result {
            Ok(()) => {
                let _ = app.emit("run", RunEvent::Progress {
                    i,
                    name: display_name,
                    ok: true,
                    error: None,
                });
            }
            Err(e) => {
                if let Ok(mut f) = failures.lock() {
                    f.push(Failure {
                        name: display_name.clone(),
                        error: e.clone(),
                    });
                }
                let _ = app.emit("run", RunEvent::Progress {
                    i,
                    name: display_name,
                    ok: false,
                    error: Some(e),
                });
            }
        }
    });

    let mut failures = failures.into_inner().unwrap_or_default();
    // Sort for deterministic UX — parallel completion order is non-deterministic.
    failures.sort_by(|a, b| a.name.cmp(&b.name));

    emit(RunEvent::Done {
        total,
        failures,
        output: output.to_string_lossy().into_owned(),
    });
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .and_then(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
async fn pick_watermark_file(app: AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("Watermark", &["png", "svg"])
            .blocking_pick_file()
            .and_then(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
fn default_output_folder() -> String {
    std::env::current_dir()
        .map(|p| p.join("watermarked").to_string_lossy().into_owned())
        .unwrap_or_else(|_| "watermarked".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Cap rayon's global pool so we don't blow up memory: each worker can
    // hold a fully-decoded photo (~96 MB for a 24 MP shot). Beyond the
    // physical perf-core count on Apple Silicon, threads contend for memory
    // bandwidth rather than gaining throughput.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(num_cpus::get_physical().clamp(4, 8))
        .build_global();

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_dialog::init());

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder
            .plugin(tauri_plugin_updater::Builder::new().build())
            .plugin(tauri_plugin_process::init())
            .manage(commands::updater::PendingUpdate::new(None));
    }

    builder
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
                if let Some(window) = app.get_webview_window("main") {
                    // Sidebar material gives the cleaner Liquid-Glass-ish base —
                    // strong blur with a subtle saturation lift that adapts to light/dark.
                    let _ = apply_vibrancy(
                        &window,
                        NSVisualEffectMaterial::Sidebar,
                        Some(NSVisualEffectState::Active),
                        None,
                    );
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            preview,
            photo_preview,
            count_photos,
            run_batch,
            pick_folder,
            pick_watermark_file,
            default_output_folder,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            commands::updater::watermarker_updater_check,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            commands::updater::watermarker_updater_install,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
