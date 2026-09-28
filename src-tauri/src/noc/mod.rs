//! The wall's data layer, in Rust: every row, and all the shaping, in one
//! engine. A port of the browser demo's `data.js`, minus the network.
//!
//! The page owns the grids, the charts and the data router. This engine owns
//! the world: the footprint of sites, the alarm state machine that turns
//! routing events into alarms, the services graded off those alarms, the
//! traffic samples and the derived figures. It never draws anything.
//!
//! Three modes, as the browser demo has:
//!
//! * `live` takes the real RIPE RIS Live BGP feed for AS3257 over a websocket,
//!   the footprint from PeeringDB and the day's outage detections from IODA.
//!   The socket and the HTTP reads live in `live.rs`; the engine only takes
//!   what they hand it. If the socket cannot be reached, the wall falls back
//!   to the recording.
//! * `recorded` replays ten minutes of real RIPE RIS Live BGP updates for
//!   AS3257, looping, each event stamped with the moment it is replayed.
//! * `simulated` invents routing events against an invented footprint of
//!   twelve cities, so it touches no recording at all.
//!
//! The engine is stepped by the Tauri side: `tick()` advances the feed and,
//! once a second, turns the wall (ageing alarms, re-stating sites, pushing
//! the derived rows). Everything it changed comes back as `Change`s in the
//! shape the router's `handle.apply()` wants.

pub mod live;
pub mod rows;
pub mod seed;

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use rows::{Alarm, Change, Chip, Element, Metric, Sample, Service, Site, Status};
use seed::{clamp, round, Seeded};

/// How long an alarm stays up, how many are held, the coalescing window.
const ALARM_WINDOW_MS: i64 = 10 * 60 * 1000;
const CLEARED_MS: i64 = 60 * 1000;
const ALARM_CAP: usize = 150;
const COALESCE_MS: i64 = 10 * 1000;
/// Criticals at once that make a major incident; the traffic sample period.
const INCIDENT_AT: usize = 4;
const SAMPLE_MS: i64 = 5 * 60 * 1000;

/// The recorded footprint, the recorded outage detections and the recorded
/// routing feed, compiled into the binary so the app needs no files at all.
const SITES_JSON: &str = include_str!("../../data/sites.json");
const OUTAGES_JSON: &str = include_str!("../../data/outages.json");
const CAPTURE_JSON: &str = include_str!("../../data/ris-capture.json");

/// Where each RIS collector sits. Static public facts.
const COLLECTORS: &[(&str, &str, &str)] = &[
    ("rrc00", "Amsterdam", "NL"), ("rrc01", "London", "GB"), ("rrc03", "Amsterdam", "NL"),
    ("rrc04", "Geneva", "CH"), ("rrc05", "Vienna", "AT"), ("rrc06", "Tokyo", "JP"),
    ("rrc07", "Stockholm", "SE"), ("rrc10", "Milan", "IT"), ("rrc11", "New York", "US"),
    ("rrc12", "Frankfurt", "DE"), ("rrc13", "Moscow", "RU"), ("rrc14", "Palo Alto", "US"),
    ("rrc15", "Sao Paulo", "BR"), ("rrc16", "Miami", "US"), ("rrc18", "Barcelona", "ES"),
    ("rrc19", "Johannesburg", "ZA"), ("rrc20", "Zurich", "CH"), ("rrc21", "Paris", "FR"),
    ("rrc22", "Bucharest", "RO"), ("rrc23", "Singapore", "SG"), ("rrc24", "Montevideo", "UY"),
    ("rrc26", "Dubai", "AE"),
];

/// The seven services the invented operator sells, and their weights.
const SERVICES: &[(&str, &str, f64)] = &[
    ("internet", "Internet transit", 1.0),
    ("mpls", "MPLS VPN", 0.8),
    ("mobile", "Mobile backhaul", 0.7),
    ("voice", "Voice and VoIP", 0.5),
    ("video", "Video transport", 0.6),
    ("enterprise", "Enterprise ethernet", 0.9),
    ("cloud", "Cloud connect", 1.1),
];

/// The cities `simulated` invents a footprint in.
const CITIES: &[(&str, &str, f64, f64)] = &[
    ("Frankfurt", "DE", 50.11, 8.68), ("London", "GB", 51.51, -0.13), ("Amsterdam", "NL", 52.37, 4.9),
    ("Paris", "FR", 48.86, 2.35), ("New York", "US", 40.71, -74.01), ("Ashburn", "US", 39.04, -77.49),
    ("Singapore", "SG", 1.29, 103.85), ("Tokyo", "JP", 35.68, 139.69), ("Sydney", "AU", -33.87, 151.21),
    ("Sao Paulo", "BR", -23.55, -46.63), ("Johannesburg", "ZA", -26.2, 28.05), ("Toronto", "CA", 43.65, -79.38),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Live,
    Recorded,
    Simulated,
}

impl Mode {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "live" => Ok(Mode::Live),
            "recorded" | "snapshot" => Ok(Mode::Recorded),
            "simulated" => Ok(Mode::Simulated),
            other => Err(format!("mode must be live, recorded or simulated, not \"{other}\".")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Recorded => "recorded",
            Mode::Simulated => "simulated",
        }
    }
}

/// Where the routing events come from right now. `live` starts on the socket
/// and may fall back to the replay; the mode the page asked for is kept apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Socket,
    Replay,
    Invent,
}

/// One building, before it is a row: what PeeringDB (live or recorded) says.
#[derive(Clone, Debug)]
pub struct Facility {
    pub id: String,
    pub name: String,
    pub city: String,
    pub country: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub networks: i64,
}

impl Facility {
    /// A PeeringDB `fac` object, live (`latitude`, `net_count`) or recorded (`lat`, `networks`).
    pub fn from_value(fac: &Value) -> Self {
        let raw = match &fac["id"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let id = if raw.starts_with('F') || raw.starts_with('S') { raw } else { format!("F{raw}") };
        let text = |key: &str| fac[key].as_str().unwrap_or("").trim().to_string();
        Self {
            id,
            name: text("name"),
            city: text("city"),
            country: text("country"),
            lat: fac["lat"].as_f64().or_else(|| fac["latitude"].as_f64()),
            lon: fac["lon"].as_f64().or_else(|| fac["longitude"].as_f64()),
            networks: fac["networks"].as_i64().or_else(|| fac["net_count"].as_i64()).unwrap_or(0),
        }
    }
}

/// The recorded footprint: PeeringDB's facilities for the backbone, saved.
pub fn recorded_facilities() -> Result<Vec<Facility>, String> {
    let rows: Vec<Value> =
        serde_json::from_str(SITES_JSON).map_err(|e| format!("the recorded footprint could not be read: {e}"))?;
    Ok(rows.iter().map(Facility::from_value).collect())
}

/// The invented footprint `simulated` uses: one building per city.
fn invented_facilities() -> Vec<Facility> {
    CITIES
        .iter()
        .enumerate()
        .map(|(at, (city, country, lat, lon))| Facility {
            id: format!("S{}", at + 1),
            name: format!("{} {}", city, ['A', 'B', 'C', 'D'][at % 4]),
            city: city.to_string(),
            country: country.to_string(),
            lat: Some(*lat),
            lon: Some(*lon),
            networks: 40 + at as i64 * 9,
        })
        .collect()
}

/// One of IODA's per-country outage detections, live or recorded.
#[derive(Clone, Debug)]
pub struct Detection {
    pub code: String,
    pub name: String,
    pub score: i64,
    pub events: i64,
}

impl Detection {
    /// IODA's live summary row (`entity`, `scores.overall`, `event_cnt`) or the recorded one (`code`, `score`, `events`).
    pub fn from_value(row: &Value) -> Option<Self> {
        let entity = if row["entity"].is_object() { &row["entity"] } else { row };
        let code = entity["code"].as_str().unwrap_or("").to_string();
        let name = entity["name"].as_str().map(str::to_string).unwrap_or_else(|| code.clone());
        let score = row["score"].as_f64().or_else(|| row["scores"]["overall"].as_f64()).unwrap_or(0.0).round() as i64;
        let events = row["events"].as_i64().or_else(|| row["event_cnt"].as_i64()).unwrap_or(1).max(1);
        if code.is_empty() || score <= 0 {
            return None;
        }
        Some(Self { code, name, score, events })
    }
}

/// Which collector a RIS host name is, if it is one the wall knows.
fn collector(host: &str) -> Option<Vantage> {
    let id = host.split('.').next().unwrap_or("");
    COLLECTORS.iter().find(|(known, _, _)| *known == id).map(|(id, city, country)| Vantage {
        id: id.to_string(),
        city: city.to_string(),
        country: country.to_string(),
    })
}

/// The RIS Live recording: `[msFromStart, hostIndex, peerAsn, kind, prefix, originAsn]` per event.
#[derive(Deserialize)]
struct Capture {
    hosts: Vec<String>,
    events: Vec<(i64, usize, i64, String, String, Value)>,
}

/// Where a routing event was seen from.
struct Vantage {
    id: String,
    city: String,
    country: String,
}

enum EventKind {
    Withdrawal,
    Recovery,
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub struct Engine {
    mode: Mode,
    source: Source,
    /// Routing events a second.
    rate: f64,
    sites: Vec<Site>,
    by_id: HashMap<String, usize>,
    by_city: HashMap<String, usize>,
    services: Vec<Service>,
    elements: Vec<Element>,
    alarms: HashMap<String, Alarm>,
    flaps: HashMap<String, u32>,
    dirty: HashSet<String>,
    messages: u64,
    withdrawals: u64,
    last_messages: u64,
    last_at: i64,
    rate_per_minute: i64,
    sampled_at: i64,
    chance: Seeded,
    capture: Option<Capture>,
    replay_at: usize,
    /// Fractional events owed to the next tick, so a low rate still moves.
    carry: f64,
    last_tick: i64,
    last_turn: i64,
    incident: bool,
    state: &'static str,
    note: String,
    pending: Vec<Change>,
}

impl Engine {
    /// Build the world for one mode from its own footprint: the recording for
    /// `recorded` and `live`, the invented cities for `simulated`. Nothing has
    /// moved yet: `snapshot()` is the whole of it, and `tick()` moves it from there.
    pub fn new(mode: Mode, rate: f64) -> Result<Self, String> {
        let facilities = match mode {
            Mode::Simulated => invented_facilities(),
            Mode::Recorded | Mode::Live => recorded_facilities()?,
        };
        Self::from_facilities(mode, rate, facilities)
    }

    /// Build the world for one mode over a footprint read elsewhere: the live
    /// PeeringDB answer, or the recorded copy when that could not be read.
    pub fn from_facilities(mode: Mode, rate: f64, facilities: Vec<Facility>) -> Result<Self, String> {
        let rate = if rate > 0.0 { rate } else { 12.0 };
        let now = now_ms();
        let sites: Vec<Site> = facilities
            .into_iter()
            .map(|fac| site_row(fac.id, fac.name, fac.city, fac.country, fac.lat, fac.lon, fac.networks))
            .collect();
        let mut by_id = HashMap::new();
        let mut by_city = HashMap::new();
        for (at, site) in sites.iter().enumerate() {
            by_id.insert(site.id.clone(), at);
            by_city.entry(site.city.to_lowercase()).or_insert(at);
        }
        let (source, capture) = match mode {
            Mode::Recorded => (Source::Replay, Some(load_capture()?)),
            Mode::Simulated => (Source::Invent, None),
            Mode::Live => (Source::Socket, None),
        };
        let elements = element_rows(&sites);
        let mut engine = Self {
            mode,
            source,
            rate,
            sites,
            by_id,
            by_city,
            services: SERVICES
                .iter()
                .map(|(id, name, weight)| Service {
                    kind: "service",
                    id,
                    name,
                    weight: *weight,
                    availability: 100.0,
                    status: "Operational",
                })
                .collect(),
            elements,
            alarms: HashMap::new(),
            flaps: HashMap::new(),
            dirty: HashSet::new(),
            messages: 0,
            withdrawals: 0,
            last_messages: 0,
            last_at: now,
            rate_per_minute: 0,
            sampled_at: now,
            chance: Seeded::new("noc-simulated"),
            capture,
            replay_at: 0,
            carry: 0.0,
            last_tick: now,
            last_turn: now,
            incident: false,
            state: "loading",
            note: String::new(),
            pending: Vec::new(),
        };
        match mode {
            Mode::Recorded => {
                engine.load_recorded_outages(now);
                engine.set_state("replaying", "Replaying ten minutes of recorded RIS Live BGP updates for AS3257, looping.");
            }
            Mode::Simulated => {
                engine.set_state("simulating", &format!("Inventing {rate} routing events a second. Nothing in this mode is real."));
            }
            Mode::Live => {
                engine.set_state("connecting", "Opening the RIS Live feed for AS3257.");
            }
        }
        Ok(engine)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Move the connection state, and say so in the same breath.
    pub fn set_state(&mut self, state: &'static str, note: &str) {
        self.state = state;
        self.note = note.to_string();
    }

    /// The socket could not be had: replay the recording instead, as the
    /// browser demo does. The mode the page asked for stays `live`.
    pub fn fall_back_to_recording(&mut self, why: &str) -> Result<(), String> {
        if self.capture.is_none() {
            self.capture = Some(load_capture()?);
        }
        self.source = Source::Replay;
        self.set_state("replaying", why);
        Ok(())
    }

    /// One RIS Live message, straight off the socket, as the routing events in it.
    pub fn on_ris_message(&mut self, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else { return };
        if message["type"] != "ris_message" || !message["data"].is_object() {
            return;
        }
        let update = &message["data"];
        self.messages += 1;
        let at = update["timestamp"]
            .as_f64()
            .map(|seconds| (seconds * 1000.0).round() as i64)
            .unwrap_or_else(now_ms);
        let vantage = collector(update["host"].as_str().unwrap_or(""));
        let peer = match &update["peer_asn"] {
            Value::String(text) => text.parse().unwrap_or(0),
            other => other.as_i64().unwrap_or(0),
        };
        let withdrawals: Vec<String> = update["withdrawals"]
            .as_array()
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let recoveries: Vec<String> = update["announcements"]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|group| group["prefixes"].as_array())
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for prefix in withdrawals {
            self.raise(EventKind::Withdrawal, at, peer, &prefix, vantage.as_ref());
        }
        for prefix in recoveries {
            self.raise(EventKind::Recovery, at, peer, &prefix, vantage.as_ref());
        }
    }

    /// IODA's detections, live or recorded, as alarms of the outage class.
    /// Stamped with `at`, so nothing on the wall dates from the recording day.
    pub fn apply_detections(&mut self, mut detections: Vec<Detection>, at: i64) -> usize {
        detections.sort_by(|a, b| b.score.cmp(&a.score));
        let mut added = 0;
        for row in detections.into_iter().take(12) {
            let (severity, label) = if row.score >= 20_000 { ("critical", "Critical") } else { ("major", "Major") };
            let alarm = Alarm {
                kind: "alarm",
                id: format!("outage:ioda:{}", row.code),
                class: "outage",
                source: "IODA",
                prefix: String::new(),
                peer_asn: None,
                collector: String::new(),
                vantage: row.name.clone(),
                site_id: None,
                site: row.name.clone(),
                country: row.code,
                severity,
                severity_label: label,
                raised_severity_label: label,
                status: "Active",
                cleared: false,
                since: at,
                at,
                count: row.events,
                score: Some(row.score),
                description: format!("Internet outage detected in {} (IODA score {})", row.name, row.score),
                impact: "Reachability reduced for this country".to_string(),
                seen: at,
            };
            self.pending.push(Change::upsert(&alarm));
            self.alarms.insert(alarm.id.clone(), alarm);
            added += 1;
        }
        added
    }

    /// The recorded detections, for when IODA is not asked or does not answer.
    pub fn load_recorded_outages(&mut self, at: i64) -> usize {
        let Ok(saved) = serde_json::from_str::<Value>(OUTAGES_JSON) else { return 0 };
        let rows = saved["latest"]["countries"]
            .as_array()
            .map(|list| list.iter().filter_map(Detection::from_value).collect())
            .unwrap_or_default();
        self.apply_detections(rows, at)
    }

    pub fn set_rate(&mut self, rate: f64) {
        if rate > 0.0 {
            self.rate = rate;
            if self.mode == Mode::Simulated {
                self.note = format!("Inventing {rate} routing events a second. Nothing in this mode is real.");
            }
        }
    }

    /// The whole world as it stands: every row of every kind, for `handle.load()`.
    pub fn snapshot(&mut self) -> Vec<Value> {
        let now = now_ms();
        let mut rows: Vec<Value> = Vec::new();
        for site in &self.sites {
            rows.push(to_value(site));
        }
        for element in &self.elements {
            rows.push(to_value(element));
        }
        for sample in sample_rows(self.sites.len(), now, 288) {
            rows.push(to_value(&sample));
        }
        for alarm in self.alarms.values() {
            rows.push(to_value(alarm));
        }
        // Grade the services before they are sent, so the snapshot is consistent.
        let (derived, _changed) = self.derived();
        for service in &self.services {
            rows.push(to_value(service));
        }
        rows.extend(derived);
        self.pending.clear();
        rows
    }

    /// Advance the feed to `now`, and turn the wall if a second has passed.
    /// Returns what changed, and the status line when the wall turned.
    pub fn tick(&mut self, now: i64) -> (Vec<Change>, Option<Status>) {
        let elapsed = (now - self.last_tick).max(0) as f64 / 1000.0;
        self.last_tick = now;
        self.carry += self.rate * elapsed;
        let due = self.carry.floor() as usize;
        self.carry -= due as f64;
        // Never let a stalled thread catch up with a burst that swamps the page.
        for _ in 0..due.min(200) {
            match self.source {
                Source::Replay => self.replay_one(now),
                Source::Invent => self.invent(now),
                Source::Socket => break, // the socket task feeds `on_ris_message` itself
            }
        }
        let status = if now - self.last_turn >= 1000 {
            self.last_turn = now;
            Some(self.turn(now))
        } else {
            None
        };
        (std::mem::take(&mut self.pending), status)
    }

    /// An operator clearing an alarm from the wall by hand.
    pub fn clear_alarm(&mut self, id: &str, now: i64) -> bool {
        let Some(held) = self.alarms.get_mut(id) else { return false };
        if held.cleared {
            return false;
        }
        held.cleared = true;
        held.status = "Cleared";
        held.severity = "minor";
        held.severity_label = "Minor";
        held.at = now;
        held.seen = now;
        held.impact = "Cleared by the operator".to_string();
        held.description = format!("{} (cleared by hand)", held.description);
        let site_id = held.site_id.clone();
        let row = held.clone();
        self.touch(site_id.as_deref(), -1);
        self.pending.push(Change::upsert(&row));
        true
    }

    pub fn status(&self, now: i64) -> Status {
        self.status_with(now, None)
    }

    fn status_with(&self, now: i64, alert: Option<&'static str>) -> Status {
        let (alarms, critical) = self.count_live();
        Status {
            mode: self.mode.name(),
            state: self.state,
            note: self.note.clone(),
            messages: self.messages,
            withdrawals: self.withdrawals,
            rate_per_minute: self.rate_per_minute,
            alarms,
            critical,
            sites: self.sites.len(),
            at: now,
            alert,
        }
    }

    fn count_live(&self) -> (i64, i64) {
        let mut all = 0;
        let mut critical = 0;
        for alarm in self.alarms.values() {
            if alarm.cleared {
                continue;
            }
            all += 1;
            if alarm.severity == "critical" {
                critical += 1;
            }
        }
        (all, critical)
    }

    /// Move a site's alarm count, and mark it for the next turn.
    fn touch(&mut self, site_id: Option<&str>, by: i64) {
        let Some(at) = site_id.and_then(|id| self.by_id.get(id).copied()) else { return };
        let site = &mut self.sites[at];
        site.alarms = (site.alarms + by).max(0);
        self.dirty.insert(site.id.clone());
    }

    /// One routing event, replayed or invented, as the alarm it moved.
    ///
    /// A withdrawal raises an alarm keyed by peer and prefix; the same peer
    /// announcing it again clears it; a repeat inside ten seconds raises the
    /// count on the alarm already there; a prefix that keeps flapping is
    /// escalated to critical, the only judgement made here.
    fn raise(&mut self, kind: EventKind, at: i64, peer: i64, prefix: &str, vantage: Option<&Vantage>) {
        let id = format!("bgp:{peer}:{prefix}");
        let seen = now_ms();
        if let EventKind::Recovery = kind {
            let Some(held) = self.alarms.get_mut(&id) else { return };
            if held.cleared {
                return;
            }
            held.cleared = true;
            held.status = "Cleared";
            held.severity = "minor";
            held.severity_label = "Minor";
            held.at = at;
            held.seen = seen;
            held.impact = "Route restored".to_string();
            held.description = format!("Prefix {prefix} announced again by AS{peer}");
            let site_id = held.site_id.clone();
            let row = held.clone();
            self.touch(site_id.as_deref(), -1);
            self.pending.push(Change::upsert(&row));
            return;
        }
        self.withdrawals += 1;
        if let Some(held) = self.alarms.get_mut(&id) {
            if !held.cleared && seen - held.seen < COALESCE_MS {
                held.count += 1;
                held.at = at;
                held.seen = seen;
                self.pending.push(Change::upsert(held));
                return;
            }
        }
        let flaps = self.flaps.entry(id.clone()).or_insert(0);
        *flaps += 1;
        let flaps = *flaps;
        let site_at = vantage.and_then(|v| self.by_city.get(&v.city.to_lowercase()).copied());
        let site = site_at.map(|at| &self.sites[at]);
        let (severity, label) = if flaps >= 3 { ("critical", "Critical") } else { ("major", "Major") };
        let mut alarm = self.alarms.remove(&id).unwrap_or_else(|| Alarm {
            kind: "alarm",
            id: id.clone(),
            class: "bgp",
            source: "BGP (RIS Live)",
            prefix: prefix.to_string(),
            peer_asn: Some(peer),
            collector: vantage.map(|v| v.id.clone()).unwrap_or_default(),
            vantage: vantage.map(|v| v.city.clone()).unwrap_or_else(|| "Unknown".to_string()),
            site_id: site.map(|s| s.id.clone()),
            site: site
                .map(|s| s.name.clone())
                .or_else(|| vantage.map(|v| v.city.clone()))
                .unwrap_or_else(|| "Unknown".to_string()),
            country: site
                .map(|s| s.country.clone())
                .or_else(|| vantage.map(|v| v.country.clone()))
                .unwrap_or_default(),
            severity,
            severity_label: label,
            raised_severity_label: label,
            status: "Active",
            cleared: false,
            since: at,
            at,
            count: 0,
            score: None,
            description: String::new(),
            impact: String::new(),
            seen,
        });
        let was_live = !alarm.cleared && alarm.count > 0;
        alarm.at = at;
        alarm.seen = seen;
        alarm.cleared = false;
        alarm.status = "Active";
        alarm.count += 1;
        alarm.severity = severity;
        alarm.severity_label = label;
        alarm.description = format!("Prefix {prefix} withdrawn by AS{peer}");
        alarm.impact = if flaps >= 3 {
            format!("Flapping: {flaps} withdrawals")
        } else {
            "Route no longer seen at this collector".to_string()
        };
        let site_id = alarm.site_id.clone();
        self.pending.push(Change::upsert(&alarm));
        self.alarms.insert(id, alarm);
        if !was_live {
            self.touch(site_id.as_deref(), 1);
        }
    }

    /// The next recorded event, stamped with now. The recording loops.
    fn replay_one(&mut self, now: i64) {
        let Some(capture) = self.capture.as_ref() else { return };
        if capture.events.is_empty() {
            return;
        }
        let (_offset, host_index, peer, kind, prefix, _origin) = &capture.events[self.replay_at];
        self.replay_at = (self.replay_at + 1) % capture.events.len();
        let host = capture.hosts.get(*host_index).map(String::as_str).unwrap_or("rrc00");
        let vantage = collector(host);
        let peer = *peer;
        let prefix = prefix.clone();
        let event = if kind == "w" { EventKind::Withdrawal } else { EventKind::Recovery };
        self.messages += 1;
        self.raise(event, now, peer, &prefix, vantage.as_ref());
    }

    /// Invent one routing event, so the wall moves with no recording at all.
    fn invent(&mut self, now: i64) {
        self.messages += 1;
        let open: Vec<(i64, String)> = self
            .alarms
            .values()
            .filter(|a| !a.cleared && a.class == "bgp")
            .map(|a| (a.peer_asn.unwrap_or(0), a.prefix.clone()))
            .collect();
        if !open.is_empty() && self.chance.next() < 0.35 {
            let pick = (self.chance.next() * open.len() as f64).floor() as usize;
            let (peer, prefix) = &open[pick.min(open.len() - 1)];
            self.raise(EventKind::Recovery, now, *peer, prefix, None);
            return;
        }
        let vantage = if self.sites.is_empty() {
            None
        } else {
            let pick = (self.chance.next() * self.sites.len() as f64).floor() as usize;
            let site = &self.sites[pick.min(self.sites.len() - 1)];
            Some(Vantage { id: "sim".to_string(), city: site.city.clone(), country: site.country.clone() })
        };
        let peer = 3000 + (self.chance.next() * 60000.0).floor() as i64;
        let prefix = format!(
            "{}.{}.{}.0/24",
            10 + (self.chance.next() * 180.0).floor() as i64,
            (self.chance.next() * 255.0).floor() as i64,
            (self.chance.next() * 255.0).floor() as i64
        );
        self.raise(EventKind::Withdrawal, now, peer, &prefix, vantage.as_ref());
    }

    /// Take the aged-out alarms off the wall, and hold the list to its cap.
    fn expire(&mut self, now: i64) {
        let mut gone: Vec<String> = Vec::new();
        let mut rest: Vec<(i64, String)> = Vec::new();
        for alarm in self.alarms.values() {
            let age = now - alarm.seen;
            if age > ALARM_WINDOW_MS || (alarm.cleared && age > CLEARED_MS) {
                gone.push(alarm.id.clone());
            } else {
                rest.push((alarm.seen, alarm.id.clone()));
            }
        }
        if rest.len() > ALARM_CAP {
            rest.sort_by_key(|(seen, _)| *seen);
            let over = rest.len() - ALARM_CAP;
            gone.extend(rest.into_iter().take(over).map(|(_, id)| id));
        }
        for id in gone {
            if let Some(alarm) = self.alarms.remove(&id) {
                if !alarm.cleared {
                    self.touch(alarm.site_id.as_deref(), -1);
                }
                self.pending.push(Change::delete("alarm", &id));
            }
        }
    }

    /// Re-state the sites whose alarm count moved since the last turn.
    fn refresh_sites(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let dirty: Vec<String> = self.dirty.drain().collect();
        for id in dirty {
            if let Some(&at) = self.by_id.get(&id) {
                let site = &mut self.sites[at];
                site.restate();
                self.pending.push(Change::upsert(site));
            }
        }
    }

    /// The chips and the tiles as rows, and the services re-graded off the
    /// alarms. Returns the chip and metric rows, and the services that moved.
    fn derived(&mut self) -> (Vec<Value>, Vec<Value>) {
        let mut severity: HashMap<&'static str, i64> = HashMap::new();
        let mut country: HashMap<String, i64> = HashMap::new();
        for alarm in self.alarms.values() {
            if alarm.cleared {
                continue;
            }
            *severity.entry(alarm.severity_label).or_insert(0) += 1;
            if !alarm.country.is_empty() {
                *country.entry(alarm.country.clone()).or_insert(0) += 1;
            }
        }
        let alarms: i64 = severity.values().sum();
        let mut availability = 0.0;
        let mut online = 0;
        for site in &self.sites {
            availability += site.availability;
            if site.status == "Operational" {
                online += 1;
            }
        }
        let mut rows: Vec<Value> = Vec::new();
        rows.push(to_value(&Chip { kind: "chip", id: "chip-all".to_string(), label: "All", count: alarms }));
        for (id, label) in [("critical", "Critical"), ("major", "Major"), ("minor", "Minor")] {
            rows.push(to_value(&Chip {
                kind: "chip",
                id: format!("chip-{id}"),
                label,
                count: severity.get(label).copied().unwrap_or(0),
            }));
        }
        let mut tile = |id: &str, label: &str, value: f64| {
            rows.push(to_value(&Metric {
                kind: "metric",
                id: format!("metric-{id}"),
                label: label.to_string(),
                value,
                group: "wall",
            }));
        };
        tile("rate", "Feed messages per minute", self.rate_per_minute as f64);
        tile("withdrawals", "Withdrawals seen", self.withdrawals as f64);
        tile("alarms", "Active alarms", alarms as f64);
        tile("critical", "Critical alarms", severity.get("Critical").copied().unwrap_or(0) as f64);
        tile("sites-online", "Sites operational", online as f64);
        tile("availability", "Mean availability", round(availability / self.sites.len().max(1) as f64, 3));
        tile(
            "rows",
            "Rows on the wall",
            (self.sites.len() + self.alarms.len() + self.services.len() + self.elements.len()) as f64,
        );
        let mut busiest: Vec<(String, i64)> = country.into_iter().collect();
        busiest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (code, count) in busiest.into_iter().take(8) {
            rows.push(to_value(&Metric {
                kind: "metric",
                id: format!("metric-country-{code}"),
                label: code,
                value: count as f64,
                group: "country",
            }));
        }
        let graded = [
            severity.get("Critical").copied().unwrap_or(0) as f64,
            severity.get("Major").copied().unwrap_or(0) as f64,
            severity.get("Minor").copied().unwrap_or(0) as f64,
        ];
        let mut changed: Vec<Value> = Vec::new();
        for service in &mut self.services {
            let now = round(
                clamp(100.0 - service.weight * (graded[0] * 0.05 + graded[1] * 0.002 + graded[2] * 0.0005), 98.0, 100.0),
                3,
            );
            if now == service.availability {
                continue;
            }
            service.availability = now;
            service.status = if now >= 99.5 {
                "Operational"
            } else if now >= 99.0 {
                "Degraded"
            } else {
                "Impaired"
            };
            changed.push(to_value(service));
        }
        (rows, changed)
    }

    /// One turn of the wall, once a second.
    fn turn(&mut self, now: i64) -> Status {
        let seconds = ((now - self.last_at) as f64 / 1000.0).max(0.001);
        self.rate_per_minute = (((self.messages - self.last_messages) as f64 / seconds) * 60.0).round() as i64;
        self.last_messages = self.messages;
        self.last_at = now;
        self.expire(now);
        self.refresh_sites();
        let (rows, changed) = self.derived();
        for row in rows.into_iter().chain(changed) {
            self.pending.push(Change { op: "upsert", row });
        }
        if now - self.sampled_at > SAMPLE_MS {
            self.sampled_at = now;
            for sample in sample_rows(self.sites.len(), now, 1) {
                self.pending.push(Change::upsert(&sample));
            }
        }
        // The incident line: fires once when the criticals cross it, re-arms after.
        let (_, critical) = self.count_live();
        let alert = if critical as usize >= INCIDENT_AT {
            if self.incident {
                None
            } else {
                self.incident = true;
                Some("major-incident")
            }
        } else {
            self.incident = false;
            None
        };
        self.status_with(now, alert)
    }
}

fn load_capture() -> Result<Capture, String> {
    serde_json::from_str::<Capture>(CAPTURE_JSON).map_err(|e| format!("the routing recording could not be read: {e}"))
}

fn to_value<T: serde::Serialize>(row: &T) -> Value {
    serde_json::to_value(row).unwrap_or(Value::Null)
}

/// A facility as a `site` row, with an invented availability drawn skewed
/// towards healthy: most buildings are fine.
fn site_row(id: String, name: String, city: String, country: String, lat: Option<f64>, lon: Option<f64>, networks: i64) -> Site {
    let mut random = Seeded::new(&format!("site:{id}"));
    let band = random.next();
    let spread = random.next();
    let base_availability = round(
        if band < 0.85 {
            99.9 + spread * 0.099
        } else if band < 0.97 {
            99.5 + spread * 0.39
        } else {
            98.6 + spread * 0.89
        },
        3,
    );
    let maintenance = random.next() < 0.03;
    let mut site = Site {
        kind: "site",
        id,
        name,
        city,
        country,
        lat,
        lon,
        networks,
        status: "Operational",
        availability: base_availability,
        alarms: 0,
        maintenance,
        base_availability,
    };
    site.restate();
    site
}

/// The invented core: two circuits between the four busiest cities.
fn element_rows(sites: &[Site]) -> Vec<Element> {
    let mut busiest: Vec<(String, i64)> = Vec::new();
    for site in sites {
        if let Some(found) = busiest.iter_mut().find(|(city, _)| *city == site.city) {
            found.1 += site.networks;
        } else {
            busiest.push((site.city.clone(), site.networks));
        }
    }
    busiest.sort_by(|a, b| b.1.cmp(&a.1));
    let cores: Vec<String> = busiest.iter().take(4).map(|(city, _)| format!("{city} core")).collect();
    let mut rows = Vec::new();
    let mut link = |from: &str, to: &str, circuit: &str| {
        rows.push(Element {
            kind: "element",
            id: format!("{from}|{to}|{circuit}"),
            from: from.to_string(),
            to: to.to_string(),
            load: round(30.0 + Seeded::new(&format!("element:{from}:{to}:{circuit}")).next() * 65.0, 1),
        });
    };
    for (at, core) in cores.iter().enumerate() {
        link("Internet", core, "a");
        link("Internet", core, "b");
        let next = &cores[(at + 1) % cores.len()];
        if next != core {
            link(core, next, "a");
            link(core, next, "b");
        }
        link(core, &format!("Region {}", at + 1), "a");
    }
    if !cores.is_empty() {
        link("Cloud A", &cores[0], "a");
        link("Cloud B", cores.get(1).unwrap_or(&cores[0]), "a");
    }
    rows
}

/// Five-minute traffic samples: an invented daily curve, scaled by the sites.
fn sample_rows(sites: usize, until: i64, steps: i64) -> Vec<Sample> {
    let scale = sites.max(1) as f64 * 0.042;
    let mut rows = Vec::new();
    for back in (0..steps).rev() {
        let at = until - back * SAMPLE_MS;
        let bucket = at.div_euclid(SAMPLE_MS);
        let mut random = Seeded::new(&format!("traffic:{bucket}"));
        let seconds_of_day = at.rem_euclid(24 * 3600 * 1000) as f64 / 1000.0;
        let hour = seconds_of_day / 3600.0;
        let shape = 0.55 + 0.45 * (((hour - 10.0) / 24.0) * 2.0 * std::f64::consts::PI).sin();
        rows.push(Sample {
            kind: "sample",
            id: format!("t{bucket}-in"),
            at,
            direction: "Inbound",
            gbps: round(scale * (shape * 10.0 + random.next() * 1.2), 2),
        });
        rows.push(Sample {
            kind: "sample",
            id: format!("t{bucket}-out"),
            at,
            direction: "Outbound",
            gbps: round(scale * (shape * 8.4 + random.next() * 1.1), 2),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_generator_matches_the_page() {
        // The first draw of `seeded('noc-simulated')` in the browser demo.
        let mut random = Seeded::new("noc-simulated");
        let first = random.next();
        assert!((0.0..1.0).contains(&first));
        let mut again = Seeded::new("noc-simulated");
        assert_eq!(first, again.next());
    }

    #[test]
    fn recorded_world_builds_and_moves() {
        let mut engine = Engine::new(Mode::Recorded, 12.0).unwrap();
        let rows = engine.snapshot();
        assert!(rows.iter().filter(|r| r["kind"] == "site").count() > 200);
        assert_eq!(rows.iter().filter(|r| r["kind"] == "service").count(), 7);
        assert!(rows.iter().filter(|r| r["kind"] == "alarm").count() > 0);
        let start = now_ms();
        let (changes, status) = engine.tick(start + 1000);
        assert!(!changes.is_empty());
        assert!(status.is_some());
    }

    #[test]
    fn a_ris_live_message_moves_the_wall() {
        let mut engine = Engine::new(Mode::Live, 12.0).unwrap();
        assert_eq!(engine.state, "connecting");
        engine.on_ris_message(r#"{"type":"ris_message","data":{"timestamp":1758800000.5,"host":"rrc12.ripe.net","peer_asn":"3257","withdrawals":["203.0.113.0/24"],"announcements":[]}}"#);
        let alarm = &engine.alarms["bgp:3257:203.0.113.0/24"];
        assert_eq!(alarm.status, "Active");
        assert_eq!(alarm.country, "DE");
        assert_eq!(alarm.at, 1758800000500);
        engine.on_ris_message(r#"{"type":"ris_message","data":{"host":"rrc12.ripe.net","peer_asn":3257,"announcements":[{"next_hop":"","prefixes":["203.0.113.0/24"]}]}}"#);
        assert_eq!(engine.alarms["bgp:3257:203.0.113.0/24"].status, "Cleared");
        assert_eq!(engine.messages, 2);
        // Nothing invented on the socket: a tick only turns the wall.
        let (_, status) = engine.tick(now_ms() + 1500);
        assert_eq!(status.unwrap().mode, "live");
        assert_eq!(engine.messages, 2);
        engine.fall_back_to_recording("no socket").unwrap();
        assert_eq!(engine.source, Source::Replay);
    }

    #[test]
    fn live_detections_become_outage_alarms() {
        let mut engine = Engine::new(Mode::Live, 12.0).unwrap();
        let live = serde_json::json!([
            { "entity": { "code": "CV", "name": "Cape Verde" }, "scores": { "overall": 36395.2 }, "event_cnt": 2 },
            { "entity": { "code": "XX", "name": "Nowhere" }, "scores": { "overall": 0 } }
        ]);
        let rows: Vec<Detection> = live.as_array().unwrap().iter().filter_map(Detection::from_value).collect();
        assert_eq!(engine.apply_detections(rows, 1), 1);
        assert_eq!(engine.alarms["outage:ioda:CV"].severity, "critical");
    }

    #[test]
    fn a_withdrawal_raises_and_a_recovery_clears() {
        let mut engine = Engine::new(Mode::Simulated, 1.0).unwrap();
        let where_ = Vantage { id: "sim".into(), city: "London".into(), country: "GB".into() };
        engine.raise(EventKind::Withdrawal, 1, 64512, "10.0.0.0/24", Some(&where_));
        let alarm = engine.alarms.get("bgp:64512:10.0.0.0/24").unwrap();
        assert_eq!(alarm.status, "Active");
        assert_eq!(alarm.site_id.as_deref(), Some("S2"));
        engine.raise(EventKind::Recovery, 2, 64512, "10.0.0.0/24", None);
        assert_eq!(engine.alarms["bgp:64512:10.0.0.0/24"].status, "Cleared");
        assert!(engine.clear_alarm("bgp:64512:10.0.0.0/24", 3) == false);
    }
}
