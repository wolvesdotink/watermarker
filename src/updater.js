/**
 * updater.js — channel-aware in-app updater state machine.
 *
 * Talks to the Rust-side `watermarker_updater_*` commands (NOT the
 * `@tauri-apps/plugin-updater` JS API) because the channel choice —
 * stable vs. beta — has to happen in Rust per call: the plugin's global
 * Builder doesn't accept endpoints, only `tauri.conf.json` does. Our
 * commands always call `app.updater_builder().endpoints(...)` with the
 * URL matching the requested channel.
 *
 * Lifecycle:
 *   idle → checking → available → downloading → ready
 *                  ↘ idle (no update)        ↘ error (any failure)
 *
 * Dev-mode behavior:
 *   The Rust commands need a real signed bundle + valid pubkey to do
 *   anything useful. In `pnpm tauri dev` every call errors; we surface
 *   that as `status: 'error'` rather than crashing.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const CHANNEL_KEY = "watermarker.update_channel";
const BOOT_QUIET_MS = 4000;

const initialState = Object.freeze({
  status: "idle", // 'idle' | 'checking' | 'available' | 'downloading' | 'ready' | 'error'
  newVersion: null,
  notes: null,
  downloaded: 0,
  totalBytes: 0,
  error: null,
});

export function createUpdater({ bootCheck = true } = {}) {
  let state = { ...initialState };
  const subscribers = new Set();
  let bootTimer = null;
  let unlistenProgress = null;
  let unlistenFinish = null;

  function setState(next) {
    state = { ...state, ...next };
    for (const fn of subscribers) {
      try {
        fn(state);
      } catch (e) {
        console.error("[updater] subscriber threw:", e);
      }
    }
  }

  function subscribe(fn) {
    subscribers.add(fn);
    fn(state);
    return () => subscribers.delete(fn);
  }

  function getChannel() {
    try {
      const v = localStorage.getItem(CHANNEL_KEY);
      return v === "beta" ? "beta" : "stable";
    } catch {
      return "stable";
    }
  }

  function setChannel(channel) {
    const normalized = channel === "beta" ? "beta" : "stable";
    try {
      localStorage.setItem(CHANNEL_KEY, normalized);
    } catch {
      /* private mode etc. — ignore */
    }
  }

  async function checkNow() {
    setState({ status: "checking", error: null });
    try {
      const result = await invoke("watermarker_updater_check", {
        channel: getChannel(),
      });
      if (!result) {
        setState({ ...initialState, status: "idle" });
        return;
      }
      setState({
        status: "available",
        newVersion: result.version,
        notes: result.body,
        downloaded: 0,
        totalBytes: 0,
        error: null,
      });
    } catch (e) {
      // Most common reason in production: no network. Most common in dev:
      // signed bundle / pubkey not present.
      setState({
        ...initialState,
        status: "error",
        error: e instanceof Error ? e.message : String(e),
      });
    }
  }

  async function install() {
    setState({ status: "downloading", downloaded: 0, totalBytes: 0 });
    try {
      // Subscribe to progress + finish events for the duration of this
      // install. The Rust `watermarker_updater_install` command emits
      // per-chunk progress and a single `finished` event when the byte
      // stream ends (BEFORE signature verification + extract — we don't
      // transition to 'ready' on 'finished' for that reason).
      unlistenProgress = await listen(
        "watermarker://updater-progress",
        ({ payload }) => {
          const chunk = (payload && payload.chunkLength) || 0;
          const total = (payload && payload.contentLength) || 0;
          setState({
            downloaded: state.downloaded + chunk,
            totalBytes: total > 0 ? total : state.totalBytes,
          });
        },
      );
      unlistenFinish = await listen("watermarker://updater-finished", () => {
        // No state transition here: 'finished' fires the moment the byte
        // stream ends, before signature verification + extract. We use
        // the command promise's resolution as the single source of truth.
      });

      await invoke("watermarker_updater_install");
      setState({ status: "ready" });
    } catch (e) {
      console.error("[updater] install failed:", e);
      setState({
        status: "error",
        error: e instanceof Error ? e.message : String(e),
      });
    } finally {
      unlistenProgress?.();
      unlistenProgress = null;
      unlistenFinish?.();
      unlistenFinish = null;
    }
  }

  async function restart() {
    try {
      // tauri-plugin-process exposes `relaunch` via this IPC command name.
      // Calling it through invoke() avoids needing a bundler to resolve
      // the `@tauri-apps/plugin-process` bare specifier.
      await invoke("plugin:process|restart");
    } catch (e) {
      setState({
        status: "error",
        error: e instanceof Error ? e.message : String(e),
      });
    }
  }

  function startBootCheck() {
    if (!bootCheck) return;
    bootTimer = window.setTimeout(() => {
      void checkNow();
    }, BOOT_QUIET_MS);
  }

  function dispose() {
    if (bootTimer !== null) {
      window.clearTimeout(bootTimer);
      bootTimer = null;
    }
    unlistenProgress?.();
    unlistenProgress = null;
    unlistenFinish?.();
    unlistenFinish = null;
    subscribers.clear();
  }

  return {
    get state() {
      return state;
    },
    subscribe,
    checkNow,
    install,
    restart,
    getChannel,
    setChannel,
    startBootCheck,
    dispose,
  };
}
