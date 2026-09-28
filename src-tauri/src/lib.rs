//! The Tauri side: one engine behind a mutex, a feed thread that steps it four
//! times a second, the network tasks for `live`, and the commands the page calls.
//!
//! Events to the page:
//! * `noc:changes` — `[{ op: 'upsert'|'delete', row }]`, for `handle.apply()`.
//! * `noc:status` — the status line, once a second.

mod noc;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};

use noc::live::{self, SocketTask};
use noc::{now_ms, recorded_facilities, rows::Status, Engine, Mode};

const TICK: Duration = Duration::from_millis(250);

/// The one world the app shows, and the generation of the tasks feeding it.
#[derive(Default)]
struct Feed {
    engine: Arc<Mutex<Option<Engine>>>,
    /// Bumped on every start and stop; a feed task exits when it sees a new one.
    generation: Arc<AtomicU64>,
}

/// Build a fresh world for `mode` and return every row in it. Any feed
/// already running is stopped: the page attaches its viewers, loads this,
/// then calls `noc_start`. In `live` the footprint is read from PeeringDB
/// first, and the recorded copy stands in if that cannot be had.
#[tauri::command]
async fn noc_snapshot(feed: State<'_, Feed>, mode: String, rate: Option<f64>) -> Result<Vec<Value>, String> {
    let mode = Mode::parse(&mode)?;
    feed.generation.fetch_add(1, Ordering::SeqCst);
    let rate = rate.unwrap_or(12.0);
    let mut engine = match mode {
        Mode::Live => match live::fetch_facilities().await {
            Ok(facilities) => Engine::from_facilities(mode, rate, facilities)?,
            Err(error) => {
                let mut engine = Engine::from_facilities(mode, rate, recorded_facilities()?)?;
                engine.set_state(
                    "degraded",
                    &format!("PeeringDB could not be read ({error}), so the footprint is the recorded copy."),
                );
                engine
            }
        },
        Mode::Recorded | Mode::Simulated => Engine::new(mode, rate)?,
    };
    let rows = engine.snapshot();
    *feed.engine.lock().map_err(|_| "the world is poisoned")? = Some(engine);
    Ok(rows)
}

/// Open the feed: a thread steps the engine every 250 ms and emits what moved.
/// In `live` a socket task feeds it routing events and an HTTP task reads
/// today's outage detections.
#[tauri::command]
fn noc_start(app: AppHandle, feed: State<'_, Feed>) -> Result<Status, String> {
    let generation = feed.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let (first, mode) = {
        let guard = feed.engine.lock().map_err(|_| "the world is poisoned")?;
        let engine = guard.as_ref().ok_or("call noc_snapshot before noc_start")?;
        (engine.status(now_ms()), engine.mode())
    };
    let flag = Arc::clone(&feed.generation);
    thread::Builder::new()
        .name(format!("noc-feed-{generation}"))
        .spawn(move || run_feed(app, flag, generation))
        .map_err(|e| format!("the feed thread could not start: {e}"))?;
    if mode == Mode::Live {
        tauri::async_runtime::spawn(
            SocketTask { engine: Arc::clone(&feed.engine), flag: Arc::clone(&feed.generation), generation }.run(),
        );
        let engine = Arc::clone(&feed.engine);
        let flag = Arc::clone(&feed.generation);
        tauri::async_runtime::spawn(async move {
            let (detections, note) = match live::fetch_detections().await {
                Ok(rows) => (rows, None),
                Err(error) => (Vec::new(), Some(format!("IODA could not be read ({error}), so the recorded detections are shown."))),
            };
            if flag.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Ok(mut guard) = engine.lock() {
                if let Some(engine) = guard.as_mut() {
                    if detections.is_empty() {
                        engine.load_recorded_outages(now_ms());
                    } else {
                        engine.apply_detections(detections, now_ms());
                    }
                    if let Some(note) = note {
                        eprintln!("[noc] {note}");
                    }
                }
            }
        });
    }
    Ok(first)
}

fn run_feed(app: AppHandle, flag: Arc<AtomicU64>, generation: u64) {
    loop {
        thread::sleep(TICK);
        if flag.load(Ordering::SeqCst) != generation {
            return;
        }
        let feed = app.state::<Feed>();
        let (changes, status) = {
            let Ok(mut guard) = feed.engine.lock() else { return };
            let Some(engine) = guard.as_mut() else { return };
            engine.tick(now_ms())
        };
        if !changes.is_empty() {
            let _ = app.emit("noc:changes", &changes);
        }
        if let Some(status) = status {
            let _ = app.emit("noc:status", &status);
        }
    }
}

/// Close the feed. What is already on the wall stays.
#[tauri::command]
fn noc_stop(feed: State<'_, Feed>) -> Result<(), String> {
    feed.generation.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Change how many routing events a second the replay or the simulation moves.
#[tauri::command]
fn noc_set_rate(feed: State<'_, Feed>, rate: f64) -> Result<(), String> {
    let mut guard = feed.engine.lock().map_err(|_| "the world is poisoned")?;
    if let Some(engine) = guard.as_mut() {
        engine.set_rate(rate);
    }
    Ok(())
}

/// An operator clearing one alarm by hand. The cleared row reaches the page
/// on the next tick, like any other change.
#[tauri::command]
fn noc_clear_alarm(feed: State<'_, Feed>, id: String) -> Result<bool, String> {
    let mut guard = feed.engine.lock().map_err(|_| "the world is poisoned")?;
    let engine = guard.as_mut().ok_or("no world yet")?;
    Ok(engine.clear_alarm(&id, now_ms()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Feed::default())
        .invoke_handler(tauri::generate_handler![noc_snapshot, noc_start, noc_stop, noc_set_rate, noc_clear_alarm])
        .run(tauri::generate_context!())
        .expect("error while running the Lattice NOC application");
}
