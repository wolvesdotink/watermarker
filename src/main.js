const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

import { createUpdater } from "./updater.js";

const $ = (id) => document.getElementById(id);

const state = {
  folder: "",
  watermark: "",
  output: "",
  size: 20,
  opacity: 1.0,
  margin: 24,
  position: "bottom-right",
  photoCount: null, // null = unknown, number = result of count_photos
};

const els = {};
let previewTimer = null;
let previewSeq = 0;

window.addEventListener("DOMContentLoaded", async () => {
  cacheEls();
  wireDragRegions();
  await hydrateDefaults();
  wireSliders();
  wirePositions();
  wirePickers();
  wireRun();
  await listenRunEvents();
  wireSettingsDrawer();
  wireUpdater();
});

function wireDragRegions() {
  // Tauri 2's data-tauri-drag-region attribute is unreliable on transparent
  // windows with an overlay title bar; call startDragging() explicitly instead.
  const win = window.__TAURI__?.window?.getCurrentWindow?.();
  if (!win) return;
  document.querySelectorAll("[data-tauri-drag-region]").forEach((el) => {
    el.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      if (e.target.closest("button, input, a, [data-no-drag]")) return;
      e.preventDefault();
      win.startDragging().catch(() => {});
    });
    // Double-click to maximize, matching native behavior.
    el.addEventListener("dblclick", (e) => {
      if (e.target.closest("button, input, a, [data-no-drag]")) return;
      win.toggleMaximize?.().catch(() => {});
    });
  });
}

function cacheEls() {
  els.pickFolder = $("pick-folder");
  els.pickWatermark = $("pick-watermark");
  els.pickOutput = $("pick-output");
  els.size = $("size");
  els.sizeVal = $("size-val");
  els.opacity = $("opacity");
  els.opacityVal = $("opacity-val");
  els.margin = $("margin");
  els.marginVal = $("margin-val");
  els.positions = document.querySelectorAll("#positions button");
  els.run = $("run");
  els.status = $("status");
  els.progress = $("progress");
  els.placeholder = $("placeholder");
  els.placeholderTitle = els.placeholder?.querySelector(".placeholder-title");
  els.placeholderSub = els.placeholder?.querySelector(".placeholder-sub");
  els.previewImg = $("preview-img");
  els.canvasLoader = $("canvas-loader");
  els.settingsOpen = $("settings-open");
  els.settingsClose = $("settings-close");
  els.settingsDrawer = $("settings-drawer");
  els.settingsBackdrop = $("settings-backdrop");
  els.settingsBadge = $("settings-badge");
  els.updateBanner = $("update-banner");
  els.updateBannerLabel = $("update-banner-label");
}

function wireSettingsDrawer() {
  if (!els.settingsDrawer) return;
  const open = () => {
    els.settingsDrawer.setAttribute("aria-hidden", "false");
    els.settingsClose?.focus();
  };
  const close = () => {
    els.settingsDrawer.setAttribute("aria-hidden", "true");
    els.settingsOpen?.focus();
  };
  els.settingsOpen?.addEventListener("click", open);
  els.settingsClose?.addEventListener("click", close);
  els.settingsBackdrop?.addEventListener("click", close);
  els.updateBanner?.addEventListener("click", open);
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    if (els.settingsDrawer.getAttribute("aria-hidden") === "false") close();
  });
}

async function hydrateDefaults() {
  try {
    state.output = await invoke("default_output_folder");
    setPickerPath(els.pickOutput, state.output);
  } catch {
    /* leave empty */
  }
  setPosition("bottom-right");
}

function wireSliders() {
  const sliders = [
    {
      input: els.size,
      val: els.sizeVal,
      key: "size",
      fmt: (v) => `${v.toFixed(0)}%`,
    },
    {
      input: els.opacity,
      val: els.opacityVal,
      key: "opacity",
      fmt: (v) => `${Math.round(v * 100)}%`,
    },
    {
      input: els.margin,
      val: els.marginVal,
      key: "margin",
      fmt: (v) => `${v.toFixed(0)} px`,
    },
  ];
  for (const s of sliders) {
    s.val.textContent = s.fmt(parseFloat(s.input.value));
    s.input.addEventListener("input", () => {
      const v = parseFloat(s.input.value);
      state[s.key] = v;
      s.val.textContent = s.fmt(v);
      schedulePreview();
    });
  }
}

function wirePositions() {
  els.positions.forEach((btn) => {
    btn.addEventListener("click", () => setPosition(btn.dataset.pos));
  });
}

function setPosition(pos) {
  state.position = pos;
  els.positions.forEach((b) => {
    const active = b.dataset.pos === pos;
    b.classList.toggle("active", active);
    b.setAttribute("aria-checked", active ? "true" : "false");
  });
  schedulePreview();
}

function wirePickers() {
  els.pickFolder.addEventListener("click", async () => {
    const path = await invoke("pick_folder");
    if (path) {
      state.folder = path;
      setPickerPath(els.pickFolder, path);
      refreshFolderCount();
      schedulePreview();
    }
  });

  els.pickWatermark.addEventListener("click", async () => {
    const path = await invoke("pick_watermark_file");
    if (path) {
      state.watermark = path;
      setPickerPath(els.pickWatermark, path);
      schedulePreview();
    }
  });

  els.pickOutput.addEventListener("click", async () => {
    const path = await invoke("pick_folder");
    if (path) {
      state.output = path;
      setPickerPath(els.pickOutput, path);
    }
  });
}

function setPickerPath(button, path) {
  const span = button.querySelector(".picker-path");
  if (path) {
    const name = path.split("/").pop() || path;
    span.textContent = name;
    span.title = path;
  } else {
    span.textContent = "";
    span.removeAttribute("title");
  }
}

async function refreshFolderCount() {
  if (!state.folder) {
    state.photoCount = null;
    renderRunButton();
    return;
  }
  try {
    state.photoCount = await invoke("count_photos", {
      args: { folder: state.folder },
    });
  } catch {
    state.photoCount = 0;
  }
  renderRunButton();
}

function renderRunButton() {
  const c = state.photoCount;
  if (c === null) {
    els.run.textContent = "Watermark Photos";
    els.run.disabled = false;
  } else if (c === 0) {
    els.run.textContent = "No Photos Found";
    els.run.disabled = true;
  } else if (c === 1) {
    els.run.textContent = "Watermark 1 Photo";
    els.run.disabled = false;
  } else {
    els.run.textContent = `Watermark ${c.toLocaleString()} Photos`;
    els.run.disabled = false;
  }
}

function schedulePreview() {
  if (previewTimer) clearTimeout(previewTimer);
  previewTimer = setTimeout(refreshPreview, 120);
}

async function refreshPreview() {
  if (!state.folder) return;
  const mySeq = ++previewSeq;
  showLoader();
  try {
    const dataUrl = state.watermark
      ? await invoke("preview", {
          args: {
            folder: state.folder,
            watermark: state.watermark,
            size: state.size,
            opacity: state.opacity,
            margin: state.margin,
            position: state.position,
          },
        })
      : await invoke("photo_preview", { args: { folder: state.folder } });
    if (mySeq !== previewSeq) return;
    hideLoader();
    showPreview(dataUrl);
  } catch (e) {
    if (mySeq !== previewSeq) return;
    hideLoader();
    const msg = typeof e === "string" ? e : (e && e.message) || "Preview failed";
    showPlaceholder("Preview unavailable", msg);
  }
}

let loaderShowTimer = null;
function showLoader() {
  // Brief delay avoids flashing for fast (cached) operations.
  if (loaderShowTimer) clearTimeout(loaderShowTimer);
  loaderShowTimer = setTimeout(() => {
    els.canvasLoader?.classList.add("visible");
  }, 140);
}
function hideLoader() {
  if (loaderShowTimer) {
    clearTimeout(loaderShowTimer);
    loaderShowTimer = null;
  }
  els.canvasLoader?.classList.remove("visible");
}

function showPreview(src) {
  els.placeholder.hidden = true;
  els.previewImg.hidden = false;
  els.previewImg.src = src;
}

function showPlaceholder(title, sub) {
  els.previewImg.hidden = true;
  els.placeholder.hidden = false;
  if (title) els.placeholderTitle.textContent = title;
  if (sub) els.placeholderSub.textContent = sub;
}

function wireRun() {
  els.run.addEventListener("click", async () => {
    if (!state.folder || !state.watermark || !state.output) {
      setStatus("Pick folder, watermark, and output first.", "err");
      return;
    }
    els.run.disabled = true;
    els.progress.hidden = false;
    els.progress.value = 0;
    els.progress.max = 1;
    setStatus("Starting…");
    try {
      await invoke("run_batch", {
        args: {
          folder: state.folder,
          watermark: state.watermark,
          output: state.output,
          size: state.size,
          opacity: state.opacity,
          margin: state.margin,
          position: state.position,
        },
      });
    } catch (e) {
      setStatus(typeof e === "string" ? e : (e && e.message) || "Run failed", "err");
      renderRunButton();
    }
  });
}

async function listenRunEvents() {
  await listen("run", (event) => {
    const p = event.payload;
    if (!p || !p.type) return;
    if (p.type === "start") {
      els.progress.max = p.total;
      els.progress.value = 0;
      setStatus(`Starting · 0 of ${p.total}`);
    } else if (p.type === "progress") {
      els.progress.value = p.i;
      const tail = p.ok ? "" : ` · ${p.error || "error"}`;
      setStatus(`${p.i} of ${els.progress.max} · ${p.name}${tail}`, p.ok ? null : "err");
    } else if (p.type === "done") {
      renderRunButton();
      if (p.failures.length) {
        setStatus(`Finished with ${p.failures.length} error(s)`, "err");
      } else {
        setStatus(`Finished ${p.total} photo${p.total === 1 ? "" : "s"}`, "ok");
      }
    } else if (p.type === "error") {
      renderRunButton();
      setStatus(p.message, "err");
    }
  });
}

function setStatus(msg, kind) {
  els.status.textContent = msg;
  els.status.className = "status" + (kind ? " " + kind : "");
}

function wireUpdater() {
  const card = $("update-card");
  const label = $("update-label");
  const version = $("update-version");
  const action = $("update-action");
  const progressWrap = $("update-progress-wrap");
  const progress = $("update-progress");
  const notes = $("update-notes");
  const toggle = $("update-beta-toggle");
  const appVersion = $("app-version");

  if (!card || !action) return;

  const updater = createUpdater({ bootCheck: true });

  // Persisted beta toggle state.
  toggle.checked = updater.getChannel() === "beta";
  toggle.addEventListener("change", () => {
    updater.setChannel(toggle.checked ? "beta" : "stable");
  });

  // Single click handler — meaning depends on current state.
  action.addEventListener("click", () => {
    const s = updater.state.status;
    if (s === "available") updater.install();
    else if (s === "ready") updater.restart();
    else updater.checkNow();
  });

  updater.subscribe((state) => {
    card.dataset.state = state.status;

    switch (state.status) {
      case "idle":
        label.textContent = "You're up to date";
        version.textContent = "";
        action.textContent = "Check";
        action.disabled = false;
        progressWrap.hidden = true;
        notes.hidden = true;
        notes.textContent = "";
        break;
      case "checking":
        label.textContent = "Checking…";
        version.textContent = "";
        action.textContent = "Checking…";
        action.disabled = true;
        progressWrap.hidden = true;
        notes.hidden = true;
        break;
      case "available":
        label.textContent = "Update available";
        version.textContent = state.newVersion ? `v${state.newVersion}` : "";
        action.textContent = "Install";
        action.disabled = false;
        progressWrap.hidden = true;
        if (state.notes && state.notes.trim()) {
          notes.textContent = state.notes;
          notes.hidden = false;
        } else {
          notes.hidden = true;
        }
        break;
      case "downloading": {
        label.textContent = "Installing update…";
        const pct =
          state.totalBytes > 0
            ? Math.min(99, Math.floor((state.downloaded / state.totalBytes) * 100))
            : null;
        version.textContent = pct === null ? "" : `${pct}%`;
        action.textContent = "Installing…";
        action.disabled = true;
        progressWrap.hidden = false;
        if (state.totalBytes > 0) {
          progress.max = state.totalBytes;
          progress.value = state.downloaded;
        } else {
          progress.removeAttribute("value");
        }
        notes.hidden = true;
        break;
      }
      case "ready":
        label.textContent = "Update installed";
        version.textContent = "";
        action.textContent = "Restart";
        action.disabled = false;
        progressWrap.hidden = true;
        notes.hidden = true;
        break;
      case "error":
        label.textContent = "Update failed";
        version.textContent = "";
        action.textContent = "Retry";
        action.disabled = false;
        progressWrap.hidden = true;
        if (state.error) {
          notes.textContent = state.error;
          notes.hidden = false;
        } else {
          notes.hidden = true;
        }
        break;
    }

    renderUpdateBanner(state);
  });

  updater.startBootCheck();

  // Stamp the current app version into the sidebar footer text. With
  // `withGlobalTauri: true` set in tauri.conf.json, the core `app` API is
  // exposed on the global — avoids needing a bundler.
  (async () => {
    try {
      const v = await window.__TAURI__.app.getVersion();
      appVersion.textContent = `Watermarker v${v}`;
    } catch {
      /* outside Tauri runtime — leave blank */
    }
  })();
}

function renderUpdateBanner(state) {
  const banner = els.updateBanner;
  const badge = els.settingsBadge;
  if (!banner || !badge) return;

  let label = "";
  let showBadge = false;
  switch (state.status) {
    case "available":
      label = state.newVersion
        ? `Update available · v${state.newVersion}`
        : "Update available";
      showBadge = true;
      break;
    case "downloading": {
      const pct =
        state.totalBytes > 0
          ? Math.min(99, Math.floor((state.downloaded / state.totalBytes) * 100))
          : null;
      label = pct === null ? "Installing update…" : `Installing update · ${pct}%`;
      break;
    }
    case "ready":
      label = "Update ready · restart to apply";
      showBadge = true;
      break;
  }

  if (label) {
    banner.dataset.state = state.status;
    els.updateBannerLabel.textContent = label;
    banner.hidden = false;
  } else {
    banner.hidden = true;
  }
  badge.hidden = !showBadge;
}
