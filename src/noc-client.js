/**
 * The page's side of the feed: one data router, one source, and the Rust
 * engine behind Tauri filling it.
 *
 * Load it after the grid core and `modules/data-router`, then, in this order:
 *
 *     const data = NocTauri.create({ mode: 'recorded', rate: 12 });
 *     data.router.attach(myAlarmGrid, 'alarm');        // attach BEFORE start()
 *     data.on('status', (status) => { ... });
 *     await data.start();
 *
 * **Attach before you start.** A route attached after rows have been loaded
 * receives nothing until the next delta for its partition.
 *
 * Every row on the wall is built in Rust (`src-tauri/src/noc/mod.rs`) and
 * arrives here in one of two ways:
 *
 *   - `noc_snapshot`, a command answering the whole world for `handle.load()`
 *     (in `live`, after Rust has read the footprint from PeeringDB);
 *   - `noc:changes`, an event carrying `[{ op, row }]` for `handle.apply()`
 *     four times a second while the feed runs.
 *
 * In `live` the routing events are the real RIS Live feed, read by Rust over
 * a websocket; `recorded` replays a saved ten minutes; `simulated` invents them.
 * Nothing here shapes a row. The router keys on `kind`, then `id`.
 */
(function (root) {
  "use strict";

  const MODES = ["live", "recorded", "simulated"];

  function create(options) {
    const settings = options || {};
    const mode = settings.mode || "live";
    if (MODES.indexOf(mode) < 0)
      throw new Error(`[noc-tauri] mode must be one of ${MODES.join(", ")}, not "${mode}".`);
    const tauri = root.__TAURI__;
    if (!tauri || !tauri.core || !tauri.event)
      throw new Error("[noc-tauri] this page must run inside the Tauri shell (withGlobalTauri).");
    const { invoke } = tauri.core;
    const { listen } = tauri.event;
    const createDataRouter = (root.LatticeGridDataRouter || {}).createDataRouter;
    if (typeof createDataRouter !== "function")
      throw new Error("[noc-tauri] load modules/data-router.min.js before this file.");

    const router = createDataRouter({
      key: "kind",
      rowKey: "id",
      overlap: true,
      batch: { intervalMs: 250 },
      coalesce: true,
      metricsInterval: 1000,
    });
    const feed = router.addSource("rust");
    const listeners = [];
    const state = { mode, rate: Number(settings.rate) > 0 ? Number(settings.rate) : 12, started: false, unlisten: [], last: null };

    function emit(status) {
      state.last = status;
      for (const handler of listeners.slice()) {
        try { handler(status); } catch (error) { console.error("[noc-tauri] a status listener threw", error); }
      }
    }

    /** Ask Rust for the world in `mode`, and put it on the wall. */
    async function load() {
      const rows = await invoke("noc_snapshot", { mode: state.mode, rate: state.rate });
      feed.load(rows);
      return rows.length;
    }

    /** Load the world and open the feed. Attach every viewer before this. */
    async function start() {
      if (state.started) return;
      state.started = true;
      emit({ mode: state.mode, state: "loading", note: "Building the world in Rust.", at: Date.now() });
      const count = await load();
      state.unlisten.push(
        await listen("noc:changes", (event) => feed.apply(event.payload)),
        await listen("noc:status", (event) => emit(event.payload)),
      );
      const status = await invoke("noc_start");
      emit(Object.assign({}, status, { note: `${status.note} ${count} rows loaded.` }));
    }

    /** Close the feed. What is already routed stays. */
    async function stop() {
      if (!state.started) return;
      state.started = false;
      await invoke("noc_stop");
      for (const off of state.unlisten.splice(0)) off();
      router.flushStream();
      emit({ mode: state.mode, state: "stopped", note: "The routing feed is closed.", at: Date.now() });
    }

    /** Switch mode: stop, rebuild the world in Rust, reload the source, restart. */
    async function restart(next) {
      if (next && next.mode) {
        if (MODES.indexOf(next.mode) < 0) throw new Error(`[noc-tauri] unknown mode "${next.mode}".`);
        state.mode = next.mode;
      }
      if (next && Number(next.rate) > 0) state.rate = Number(next.rate);
      await stop();
      await start();
    }

    /** Change the feed's pace without rebuilding the world. */
    async function setRate(rate) {
      if (!(Number(rate) > 0)) return;
      state.rate = Number(rate);
      await invoke("noc_set_rate", { rate: state.rate });
    }

    /** An operator clearing one alarm by hand; resolves true if it was live. */
    const clearAlarm = (id) => invoke("noc_clear_alarm", { id: String(id) });

    function on(name, handler) {
      if (name !== "status" || typeof handler !== "function") return () => {};
      listeners.push(handler);
      return () => { const at = listeners.indexOf(handler); if (at >= 0) listeners.splice(at, 1); };
    }

    const destroy = async () => { await stop(); listeners.length = 0; router.destroy(); };

    return {
      router,
      start, stop, restart, setRate, clearAlarm, on, destroy,
      get mode() { return state.mode; },
      get rate() { return state.rate; },
      get status() { return state.last; },
    };
  }

  root.NocTauri = { create, modes: MODES };
})(window);
