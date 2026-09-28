//! The network: the RIS Live websocket, PeeringDB and IODA. Everything here
//! runs on Tauri's async runtime and hands what it reads to the engine under
//! its mutex; nothing here shapes a row.
//!
//! The three sources are the browser demo's, verbatim:
//!
//! * `wss://ris-live.ripe.net/v1/ws/` — every BGP UPDATE whose AS path holds
//!   the backbone, as it happens. Thousands a minute in a busy hour.
//! * `https://www.peeringdb.com/api/` — the facilities the backbone's network
//!   is present in: the footprint on the map.
//! * `https://api.ioda.inetintel.cc.gatech.edu/v2/` — today's country-level
//!   outage detections.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use super::{now_ms, Detection, Engine, Facility};

/// The backbone the wall follows: a real AS, and its PeeringDB network.
pub const CARRIER_ASN: u32 = 3257;
const CARRIER_NET_ID: u32 = 14;
const PEERINGDB: &str = "https://www.peeringdb.com/api/";
const IODA: &str = "https://api.ioda.inetintel.cc.gatech.edu/v2/";
const RIS_LIVE: &str = "wss://ris-live.ripe.net/v1/ws/?client=lattice-noc-tauri";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent("lattice-noc-tauri/0.1 (latticegrid.dev demo)")
        .build()
        .map_err(|e| format!("the HTTP client could not be built: {e}"))
}

/// Read JSON, and say what answered when it did not work.
async fn json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    let response = client.get(url).send().await.map_err(|e| format!("{url} could not be reached ({e})"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{url} answered {status}"));
    }
    response.json::<Value>().await.map_err(|e| format!("{url} answered something that is not JSON ({e})"))
}

/// The footprint from PeeringDB: the network's facilities, read a hundred at a time.
pub async fn fetch_facilities() -> Result<Vec<Facility>, String> {
    let client = client()?;
    let listing = json(&client, &format!("{PEERINGDB}netfac?net_id={CARRIER_NET_ID}&depth=0")).await?;
    let ids: Vec<i64> = listing["data"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|row| row["fac_id"].as_i64()).filter(|id| *id > 0).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Err("PeeringDB listed no facilities".to_string());
    }
    let mut facilities = Vec::new();
    for chunk in ids.chunks(100) {
        let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let page = json(&client, &format!("{PEERINGDB}fac?id__in={list}")).await?;
        if let Some(rows) = page["data"].as_array() {
            facilities.extend(rows.iter().map(Facility::from_value));
        }
    }
    if facilities.is_empty() {
        return Err("PeeringDB listed no facilities".to_string());
    }
    Ok(facilities)
}

/// Today's country-level outage detections from IODA.
pub async fn fetch_detections() -> Result<Vec<Detection>, String> {
    let client = client()?;
    let day: i64 = 24 * 60 * 60;
    let from = (now_ms() / 1000 - day) / day * day;
    let until = from + day;
    let answer = json(&client, &format!("{IODA}outages/summary?from={from}&until={until}&entityType=country")).await?;
    let rows: Vec<Detection> = answer["data"]
        .as_array()
        .map(|list| list.iter().filter_map(Detection::from_value).collect())
        .unwrap_or_default();
    if rows.is_empty() {
        return Err("IODA reported no detections".to_string());
    }
    Ok(rows)
}

/// What the socket task needs to reach the world and know when to stop.
pub struct SocketTask {
    pub engine: Arc<Mutex<Option<Engine>>>,
    pub flag: Arc<AtomicU64>,
    pub generation: u64,
}

impl SocketTask {
    fn stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst) != self.generation
    }

    fn with_engine(&self, act: impl FnOnce(&mut Engine)) {
        if let Ok(mut guard) = self.engine.lock() {
            if let Some(engine) = guard.as_mut() {
                act(engine);
            }
        }
    }

    /// Open the real socket, with a backoff and a fall back to the recording.
    /// Returns when the feed is stopped, or once the fallback has taken over.
    pub async fn run(self) {
        let mut attempts: u32 = 0;
        let mut opened = false;
        loop {
            if self.stopped() {
                return;
            }
            match tokio_tungstenite::connect_async(RIS_LIVE).await {
                Ok((mut socket, _)) => {
                    let subscribe = serde_json::json!({
                        "type": "ris_subscribe",
                        "data": {
                            "path": CARRIER_ASN.to_string(),
                            "moreSpecific": true,
                            "type": "UPDATE",
                            "socketOptions": { "includeRaw": false }
                        }
                    });
                    if socket.send(Message::Text(subscribe.to_string().into())).await.is_ok() {
                        opened = true;
                        attempts = 0;
                        self.with_engine(|engine| {
                            engine.set_state(
                                "connected",
                                &format!("Live BGP updates for AS{CARRIER_ASN}, from the RIPE NCC's RIS."),
                            )
                        });
                        while let Some(frame) = socket.next().await {
                            if self.stopped() {
                                let _ = socket.close(None).await;
                                return;
                            }
                            match frame {
                                Ok(Message::Text(text)) => self.with_engine(|engine| engine.on_ris_message(&text)),
                                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) | Ok(Message::Frame(_)) => {}
                                Ok(Message::Close(_)) | Err(_) => break,
                            }
                        }
                    }
                }
                Err(_) => {}
            }
            if self.stopped() {
                return;
            }
            attempts += 1;
            if !opened && attempts >= 2 {
                self.with_engine(|engine| {
                    if let Err(error) = engine.fall_back_to_recording(
                        "The live routing feed could not be reached, so this is the recording, replayed.",
                    ) {
                        engine.set_state("error", &format!("The live feed could not be reached, and {error}"));
                    }
                });
                return;
            }
            self.with_engine(|engine| {
                engine.set_state("reconnecting", &format!("The live feed closed; reconnecting (attempt {attempts})."))
            });
            tokio::time::sleep(Duration::from_millis((800 * u64::from(attempts)).min(10_000))).await;
        }
    }
}
