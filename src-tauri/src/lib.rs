pub mod commands;
pub mod pipeline;

use std::path::PathBuf;

use base64::Engine;
use image::ImageEncoder;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tauri_plugin_dialog::DialogExt;

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
async fn preview(args: PreviewArgs) -> Result<String, String> {
    validate_position(&args.position)?;

    // Image work is CPU-bound; keep it off the async runtime.
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let folder = PathBuf::from(&args.folder);
        let photos = pipeline::list_photos(&folder);
        let photo = photos
            .first()
            .ok_or_else(|| "No photos in folder".to_string())?;
        let wm_path = PathBuf::from(&args.watermark);
        if !wm_path.is_file() {
            return Err("Watermark file not found".into());
        }

        let composed = pipeline::preview_image(
            photo,
            &wm_path,
            args.size,
            &args.position,
            args.margin,
            args.opacity,
            720,
        )?;

        let mut buf = Vec::with_capacity(composed.as_raw().len());
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(
                composed.as_raw(),
                composed.width(),
                composed.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| format!("png encode: {e}"))?;

        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        Ok(format!("data:image/png;base64,{b64}"))
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
/// PNG data URL — used to populate the canvas before a watermark is chosen.
#[tauri::command]
async fn photo_preview(args: PhotoPreviewArgs) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let folder = PathBuf::from(&args.folder);
        let photos = pipeline::list_photos(&folder);
        let photo = photos
            .first()
            .ok_or_else(|| "No photos in folder".to_string())?;

        let img = pipeline::open_photo(photo)?;
        let thumb = img.thumbnail(720, 720).to_rgba8();

        let mut buf = Vec::with_capacity(thumb.as_raw().len());
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(
                thumb.as_raw(),
                thumb.width(),
                thumb.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| format!("png encode: {e}"))?;

        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        Ok(format!("data:image/png;base64,{b64}"))
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

    let total = photos.len();
    emit(RunEvent::Start { total });

    let mut failures = Vec::new();
    for (idx, photo) in photos.iter().enumerate() {
        let i = idx + 1;
        // Mirror the source's subfolder structure under the output root so that
        // collisions between same-named files in different subfolders can't
        // overwrite each other.
        let rel = photo.strip_prefix(&folder).unwrap_or(photo.as_path());
        let display_name = rel.to_string_lossy().into_owned();
        let out_path = output.join(rel);

        match pipeline::watermark_photo(
            photo,
            &wm_path,
            &out_path,
            args.size,
            &args.position,
            args.margin,
            args.opacity,
        ) {
            Ok(()) => emit(RunEvent::Progress {
                i,
                name: display_name,
                ok: true,
                error: None,
            }),
            Err(e) => {
                failures.push(Failure {
                    name: display_name.clone(),
                    error: e.clone(),
                });
                emit(RunEvent::Progress {
                    i,
                    name: display_name,
                    ok: false,
                    error: Some(e),
                });
            }
        }
    }

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
