#!/usr/bin/env bash
# build-macos.sh — Build and package Watermarker as a macOS .app and .dmg
#
# Usage:
#   bash scripts/build-macos.sh [OPTIONS]
#
# Options:
#   --debug       Build in debug mode (default: release)
#   --no-dmg      Skip DMG creation
#   --notarize    Submit the .app and .dmg to Apple notarization and staple
#                 the ticket. Requires APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
#                 in the environment. Implies that APPLE_SIGNING_IDENTITY is
#                 a real Developer ID identity (not the ad-hoc "-").
#   --updater     After the app is signed (and notarized if --notarize was
#                 also passed), produce the Tauri updater payload:
#                   - Watermarker.app.tar.gz
#                   - Watermarker.app.tar.gz.sig
#                 Requires TAURI_SIGNING_PRIVATE_KEY in the environment.
#                 TAURI_SIGNING_PRIVATE_KEY_PASSWORD optional.
#   --clean       Clean build artifacts before building
#   --verbose     Enable verbose output
#
# Environment:
#   APPLE_SIGNING_IDENTITY              Code signing identity (default: ad-hoc "-")
#   APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID   Required for --notarize
#   TAURI_SIGNING_PRIVATE_KEY           Required for --updater
#   TAURI_SIGNING_PRIVATE_KEY_PASSWORD  Optional, if the key has a password

set -euo pipefail

# ─── Configuration ───────────────────────────────────────────────────────────

APP_NAME="Watermarker"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TAURI_DIR="$PROJECT_ROOT/src-tauri"

# Defaults
PROFILE="release"
BUILD_FLAGS=()
SKIP_DMG=false
NOTARIZE=false
UPDATER=false
VERBOSE=false
CLEAN=false

# ─── Argument Parsing ────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)
            PROFILE="debug"
            BUILD_FLAGS+=("--debug")
            shift
            ;;
        --no-dmg)
            SKIP_DMG=true
            shift
            ;;
        --notarize)
            NOTARIZE=true
            shift
            ;;
        --updater)
            UPDATER=true
            shift
            ;;
        --clean)
            CLEAN=true
            shift
            ;;
        --verbose)
            VERBOSE=true
            BUILD_FLAGS+=("--verbose")
            shift
            ;;
        --help|-h)
            head -25 "$0" | tail -23
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# ─── Derived Paths ───────────────────────────────────────────────────────────

TARGET_DIR="$TAURI_DIR/target/$PROFILE"
APP_BUNDLE="$TARGET_DIR/bundle/macos/$APP_NAME.app"
BINARY="$APP_BUNDLE/Contents/MacOS/$APP_NAME"
SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"

# ─── Helpers ─────────────────────────────────────────────────────────────────

log()  { echo "==> $*"; }
info() { echo "    $*"; }
warn() { echo "⚠️  $*"; }
err()  { echo "❌  $*" >&2; exit 1; }
verbose() { $VERBOSE && echo "    [verbose] $*" || true; }

# ─── Phase 1: Prerequisites ─────────────────────────────────────────────────

log "Checking prerequisites..."

command -v pnpm  >/dev/null 2>&1 || err "pnpm is not installed. Install from https://pnpm.io"
command -v cargo >/dev/null 2>&1 || err "cargo is not installed. Install from https://rustup.rs"

if $NOTARIZE; then
    [[ "$SIGNING_IDENTITY" != "-" ]] \
        || err "--notarize requires APPLE_SIGNING_IDENTITY (cannot notarize an ad-hoc signed app)"
    : "${APPLE_ID:?--notarize requires APPLE_ID env var}"
    : "${APPLE_PASSWORD:?--notarize requires APPLE_PASSWORD env var (app-specific password)}"
    : "${APPLE_TEAM_ID:?--notarize requires APPLE_TEAM_ID env var}"
    command -v xcrun >/dev/null 2>&1 \
        || err "xcrun not found. Install Xcode Command Line Tools for notarytool/stapler."
fi

if $UPDATER; then
    : "${TAURI_SIGNING_PRIVATE_KEY:?--updater requires TAURI_SIGNING_PRIVATE_KEY env var}"
fi

info "pnpm $(pnpm --version), cargo $(cargo --version | awk '{print $2}')"

# ─── Phase 2: Clean (optional) ──────────────────────────────────────────────

if $CLEAN; then
    log "Cleaning build artifacts..."
    (cd "$TAURI_DIR" && cargo clean)
fi

# ─── Phase 3: Build ─────────────────────────────────────────────────────────

log "Building $APP_NAME ($PROFILE)..."

# `--bundles app` produces just the .app; we build the DMG ourselves later
# so we can sign + notarize between .app finalization and DMG creation.
BUILD_CMD=(pnpm tauri build --bundles app)

if [[ ${#BUILD_FLAGS[@]} -gt 0 ]]; then
    BUILD_CMD+=("${BUILD_FLAGS[@]}")
fi

verbose "Running: ${BUILD_CMD[*]}"
(cd "$PROJECT_ROOT" && "${BUILD_CMD[@]}")

[[ -d "$APP_BUNDLE" ]] || err "Build failed: $APP_BUNDLE not found"

log "App bundle created at $APP_BUNDLE"

# ─── Phase 4: Codesign ──────────────────────────────────────────────────────

ENTITLEMENTS="$TAURI_DIR/entitlements.plist"
[[ -f "$ENTITLEMENTS" ]] || err "Entitlements file not found: $ENTITLEMENTS"

log "Code signing ($( [[ "$SIGNING_IDENTITY" == "-" ]] && echo "ad-hoc" || echo "$SIGNING_IDENTITY" ))..."
info "Entitlements: $ENTITLEMENTS"

# Sign the main executable with entitlements and hardened runtime
codesign --force --sign "$SIGNING_IDENTITY" --options runtime \
    --entitlements "$ENTITLEMENTS" "$BINARY"
verbose "Signed main executable with entitlements"

# Sign the app bundle (seals resources)
codesign --force --sign "$SIGNING_IDENTITY" --options runtime \
    --entitlements "$ENTITLEMENTS" "$APP_BUNDLE"
log "Code signing complete"

# ─── Phase 5: Notarize the .app ─────────────────────────────────────────────
#
# Notarization must happen on a fully-signed bundle. Apple's notarytool zips
# the .app for upload, scans for Hardened Runtime + valid signatures, and
# waits for the verdict. Stapling embeds the resulting ticket so Gatekeeper
# can verify offline on first launch.

if $NOTARIZE; then
    log "Notarizing $APP_NAME.app (this can take a few minutes)..."

    NOTARIZE_ZIP="$(mktemp -t watermarker-notarize-XXXXXX).zip"
    trap 'rm -f "$NOTARIZE_ZIP"' EXIT

    # ditto preserves resource forks + extended attributes; zip(1) does not.
    /usr/bin/ditto -c -k --keepParent "$APP_BUNDLE" "$NOTARIZE_ZIP"

    xcrun notarytool submit "$NOTARIZE_ZIP" \
        --apple-id "$APPLE_ID" \
        --password "$APPLE_PASSWORD" \
        --team-id  "$APPLE_TEAM_ID" \
        --wait

    rm -f "$NOTARIZE_ZIP"

    log "Stapling notarization ticket to $APP_NAME.app..."
    xcrun stapler staple "$APP_BUNDLE"
    xcrun stapler validate "$APP_BUNDLE"
fi

# ─── Phase 6: Generate updater payload ──────────────────────────────────────
#
# Tauri's updater plugin downloads a gzipped tarball of the new .app and
# verifies it against a minisign signature embedded in latest.json.

if $UPDATER; then
    log "Generating updater payload..."

    BUNDLE_MACOS_DIR="$(dirname "$APP_BUNDLE")"
    UPDATER_TARBALL="$BUNDLE_MACOS_DIR/$APP_NAME.app.tar.gz"

    # Tar from the bundle's parent so the archive has $APP_NAME.app at root.
    (
        cd "$BUNDLE_MACOS_DIR"
        # COPYFILE_DISABLE strips ._* AppleDouble metadata files that Finder
        # adds to network volumes — they break tar reproducibility.
        COPYFILE_DISABLE=1 tar -czf "$APP_NAME.app.tar.gz" "$APP_NAME.app"
    )

    log "Signing updater payload with Tauri minisign key..."

    # `tauri signer sign` writes <FILE>.sig next to the input file.
    TAURI_SIGNING_PRIVATE_KEY="$TAURI_SIGNING_PRIVATE_KEY" \
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" \
    pnpm tauri signer sign \
        --private-key "$TAURI_SIGNING_PRIVATE_KEY" \
        --password "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" \
        "$UPDATER_TARBALL"

    [[ -f "${UPDATER_TARBALL}.sig" ]] || err "Updater signature not produced."

    log "Updater payload: $UPDATER_TARBALL"
    log "Updater signature: ${UPDATER_TARBALL}.sig"
fi

# ─── Phase 7: Create DMG ────────────────────────────────────────────────────

if $SKIP_DMG; then
    log "Skipping DMG creation (--no-dmg)"
    DMG_PATH=""
else
    if ! command -v create-dmg >/dev/null; then
        err "create-dmg not found. Install with: brew install create-dmg"
    fi

    log "Creating DMG..."

    VERSION="$(python3 -c "import json; print(json.load(open('$PROJECT_ROOT/package.json'))['version'])")"
    DMG_NAME="${APP_NAME}_${VERSION}_$(uname -m).dmg"
    DMG_PATH="$TARGET_DIR/bundle/dmg/$DMG_NAME"

    VOLUME_ICON="$PROJECT_ROOT/src-tauri/icons/icon.icns"

    # create-dmg copies the *contents* of its source folder into the disk image.
    DMG_STAGING="$(mktemp -d)"
    trap "rm -rf '$DMG_STAGING'" EXIT
    cp -R "$APP_BUNDLE" "$DMG_STAGING/"

    mkdir -p "$(dirname "$DMG_PATH")"
    rm -f "$DMG_PATH"

    create-dmg \
        --volname "$APP_NAME" \
        --volicon "$VOLUME_ICON" \
        --window-pos 200 120 \
        --window-size 600 380 \
        --icon-size 128 \
        --text-size 13 \
        --icon "$APP_NAME.app" 160 170 \
        --hide-extension "$APP_NAME.app" \
        --app-drop-link 440 170 \
        --no-internet-enable \
        "$DMG_PATH" \
        "$DMG_STAGING"

    log "DMG created at $DMG_PATH"

    # ─── Phase 7.5: Sign + notarize the DMG ─────────────────────────────────
    #
    # Even with a fully-notarized .app inside, the DMG itself needs its own
    # signature + notarization for Gatekeeper to accept it without warnings
    # when the user double-clicks the disk image.

    if [[ "$SIGNING_IDENTITY" != "-" ]]; then
        log "Code signing DMG..."
        codesign --force --sign "$SIGNING_IDENTITY" --timestamp "$DMG_PATH"
        codesign --verify --verbose=2 "$DMG_PATH"
    fi

    if $NOTARIZE; then
        log "Notarizing DMG..."
        xcrun notarytool submit "$DMG_PATH" \
            --apple-id "$APPLE_ID" \
            --password "$APPLE_PASSWORD" \
            --team-id  "$APPLE_TEAM_ID" \
            --wait

        log "Stapling notarization ticket to DMG..."
        xcrun stapler staple "$DMG_PATH"
        xcrun stapler validate "$DMG_PATH"
    fi
fi

# ─── Phase 8: Summary ───────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Build complete!                                            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  App:  $APP_BUNDLE"
echo "        $(du -sh "$APP_BUNDLE" | awk '{print $1}')"

if [[ -n "${DMG_PATH:-}" ]] && [[ -f "$DMG_PATH" ]]; then
    echo "  DMG:  $DMG_PATH"
    echo "        $(du -sh "$DMG_PATH" | awk '{print $1}')"
fi

if $UPDATER; then
    BUNDLE_MACOS_DIR="$(dirname "$APP_BUNDLE")"
    UPDATER_TARBALL="$BUNDLE_MACOS_DIR/$APP_NAME.app.tar.gz"
    if [[ -f "$UPDATER_TARBALL" ]]; then
        echo ""
        echo "  Updater payload:    $UPDATER_TARBALL"
        echo "                      $(du -sh "$UPDATER_TARBALL" | awk '{print $1}')"
    fi
    if [[ -f "${UPDATER_TARBALL}.sig" ]]; then
        echo "  Updater signature:  ${UPDATER_TARBALL}.sig"
    fi
fi

if $NOTARIZE; then
    echo ""
    echo "  Notarized: yes (stapled to .app$( [[ -f "${DMG_PATH:-}" ]] && echo " and .dmg" ))"
fi

echo ""
