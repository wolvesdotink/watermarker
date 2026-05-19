// Smoke test for the watermark pipeline. Run with:
//   cargo run --example smoke
// Generates a fake photo + watermark in /tmp, watermarks it, and reads the result back.

use std::path::PathBuf;

use image::{ImageBuffer, Rgb, RgbImage, Rgba, RgbaImage};
use watermarker_lib::pipeline::{list_photos, preview_image, watermark_photo};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = PathBuf::from("/tmp/watermarker_smoke");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("photos"))?;
    std::fs::create_dir_all(tmp.join("out"))?;

    // Fake photo (1600x1000, solid blue).
    let photo: RgbImage = ImageBuffer::from_fn(1600, 1000, |_, _| Rgb([40, 90, 140]));
    let photo_path = tmp.join("photos/sample.jpg");
    photo.save(&photo_path)?;

    // Fake watermark PNG (400x120, opaque white).
    let wm: RgbaImage = ImageBuffer::from_fn(400, 120, |_, _| Rgba([255, 255, 255, 255]));
    let wm_path = tmp.join("wm.png");
    wm.save(&wm_path)?;

    let out_path = tmp.join("out/sample.jpg");
    watermark_photo(&photo_path, &wm_path, &out_path, 25.0, "bottom-right", 40, 0.6)?;

    let result = image::open(&out_path)?.to_rgb8();
    assert_eq!(result.width(), 1600);
    assert_eq!(result.height(), 1000);

    // Watermark is 25% of 1600 = 400 wide x 120 tall, anchored bottom-right with margin 40.
    // Center of watermark lands at approximately (1360, 900).
    let wm_center = *result.get_pixel(1360, 900);
    let bg = *result.get_pixel(50, 50);
    // Background should still be ~(40, 90, 140) — untouched.
    assert!(bg[0].abs_diff(40) < 5, "bg R drifted: {:?}", bg);
    assert!(bg[1].abs_diff(90) < 5, "bg G drifted: {:?}", bg);
    assert!(bg[2].abs_diff(140) < 5, "bg B drifted: {:?}", bg);
    // Watermark center: opaque white blended at 0.6 over the blue bg.
    // R ≈ 255*0.6 + 40*0.4 = 169, G ≈ 189, B ≈ 209. Allow ±10 for JPEG.
    assert!(wm_center[0].abs_diff(169) < 12, "wm R off: {:?}", wm_center);
    assert!(wm_center[1].abs_diff(189) < 12, "wm G off: {:?}", wm_center);
    assert!(wm_center[2].abs_diff(209) < 12, "wm B off: {:?}", wm_center);
    println!(
        "OK: wrote {} (bg={:?}, wm-center={:?})",
        out_path.display(),
        bg.0,
        wm_center.0
    );

    // Also try the preview path.
    let preview = preview_image(&photo_path, &wm_path, 25.0, "bottom-right", 40, 0.6, 720)?;
    assert!(preview.width() <= 720 && preview.height() <= 720);
    println!("OK: preview {}x{}", preview.width(), preview.height());

    // Recursive walk: place a same-named file in a nested folder plus a dot-folder
    // (which should be skipped) and verify list_photos returns both real photos.
    std::fs::create_dir_all(tmp.join("photos/trip/europe"))?;
    std::fs::create_dir_all(tmp.join("photos/.hidden"))?;
    let nested: RgbImage = ImageBuffer::from_fn(800, 600, |_, _| Rgb([20, 20, 20]));
    nested.save(tmp.join("photos/trip/europe/sample.jpg"))?; // same filename as the root one
    nested.save(tmp.join("photos/.hidden/skipme.jpg"))?;

    let photos = list_photos(&tmp.join("photos"));
    assert_eq!(photos.len(), 2, "expected 2 photos (skipping .hidden), got {photos:?}");
    let names: Vec<String> = photos
        .iter()
        .map(|p| p.strip_prefix(tmp.join("photos")).unwrap().display().to_string())
        .collect();
    assert!(names.contains(&"sample.jpg".to_string()));
    assert!(names.contains(&"trip/europe/sample.jpg".to_string()) ||
            names.contains(&"trip\\europe\\sample.jpg".to_string()));
    println!("OK: list_photos walked recursively, skipped dot-folders ({names:?})");

    Ok(())
}
