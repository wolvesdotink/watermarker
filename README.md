<p align="center">
  <img src="app-icon.svg" alt="Watermarker" width="160" height="160">
</p>

# Watermarker

Batch-watermark a folder of photos with a PNG or SVG mark, on macOS.

Point Watermarker at a folder, pick a watermark, and every photo inside gets the mark composited on top — at the size, opacity, and position you set, with a live preview that updates as you drag the sliders. SVG marks are rasterized fresh at the output resolution, so they stay crisp at any photo size. EXIF orientation is read and re-applied so portrait phone photos land right-side up. Subfolder structure is mirrored into the output directory so same-named files in different subfolders can't overwrite each other. Built on [Tauri](https://tauri.app) with a Rust image pipeline ([image](https://crates.io/crates/image), [resvg](https://crates.io/crates/resvg), [tiny-skia](https://crates.io/crates/tiny-skia)) and a vanilla HTML/CSS/JS frontend with native macOS Liquid Glass vibrancy.

**Website:** [wolves.ink/projects/watermarker](https://wolves.ink/projects/watermarker)
**By:** [Wolves Software](https://wolves.ink)

## Install

Download the latest `Watermarker.dmg` from the [releases page](https://github.com/wolvesdotink/watermarker/releases/latest), open it, and drag Watermarker to Applications.

The app is signed and notarized by Apple, so it should open without warnings on first launch. It self-updates on launch from the same release feed; flip on beta updates in Settings if you want the prerelease channel.

### Requirements

- macOS 13 (Ventura) or later
- Apple Silicon (arm64) — Intel support is on the roadmap

## Build from source

```bash
pnpm install
pnpm tauri dev                # run in development
bash scripts/build-macos.sh   # local release build (.app + .dmg)
```

The Rust image pipeline has a standalone smoke test that builds a synthetic photo + watermark in `/tmp` and verifies the round-trip:

```bash
cd src-tauri && cargo run --example smoke
```

Release builds (signed, notarized, with auto-updater payload) happen in CI when a `v*` tag is pushed. Cut a release with `bash scripts/bump.sh patch|minor|major [--beta]`, which bumps the version across `package.json`, `Cargo.toml`, `Cargo.lock`, and `tauri.conf.json`, then commits, tags, and pushes. See [`.github/workflows/release.yml`](.github/workflows/release.yml) and [`scripts/build-macos.sh`](scripts/build-macos.sh).

## License

Watermarker is licensed under the [MIT License](LICENSE).
