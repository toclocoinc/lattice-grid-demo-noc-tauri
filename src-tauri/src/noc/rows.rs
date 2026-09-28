//! What one row of each kind carries. Every row goes to the page as JSON and
//! reaches a viewer through the data router, keyed by `kind` then `id`.

use serde::Serialize;

/// One building: real (recorded from PeeringDB) or invented when simulated.
/// Status and availability are derived from its alarms.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    pub city: String,
    pub country: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub networks: i64,
    pub status: &'static str,
    pub availability: f64,
    pub alarms: i64,
    #[serde(skip)]
    pub maintenance: bool,
    #[serde(skip)]
    pub base_availability: f64,
}

impl Site {
    /// Read a site's status off its one availability figure.
    pub fn restate(&mut self) {
        self.availability = super::seed::round(
            super::seed::clamp(self.base_availability - self.alarms as f64 * 0.15, 95.0, 100.0),
            3,
        );
        self.status = if self.maintenance {
            "Maintenance"
        } else if self.availability < 99.5 {
            "Offline"
        } else if self.availability < 99.9 {
            "Degraded"
        } else {
            "Operational"
        };
    }
}

/// A routing or outage alarm on the wall.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alarm {
    pub kind: &'static str,
    pub id: String,
    pub class: &'static str,
    pub source: &'static str,
    pub prefix: String,
    pub peer_asn: Option<i64>,
    pub collector: String,
    pub vantage: String,
    pub site_id: Option<String>,
    pub site: String,
    pub country: String,
    pub severity: &'static str,
    pub severity_label: &'static str,
    /// The severity it was FIRST raised with, never restated: a trend counts this.
    pub raised_severity_label: &'static str,
    pub status: &'static str,
    pub cleared: bool,
    pub since: i64,
    pub at: i64,
    pub count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i64>,
    pub description: String,
    pub impact: String,
    /// When this process last touched it; the ageing clock, never shown.
    #[serde(skip)]
    pub seen: i64,
}

/// An invented service, graded by the alarms live on the wall.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub kind: &'static str,
    pub id: &'static str,
    pub name: &'static str,
    pub weight: f64,
    pub availability: f64,
    pub status: &'static str,
}

/// One five-minute traffic sample; invented, scaled by the number of sites.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub kind: &'static str,
    pub id: String,
    pub at: i64,
    pub direction: &'static str,
    pub gbps: f64,
}

/// One core circuit; from/to are node names, so a network chart needs no node list.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    pub kind: &'static str,
    pub id: String,
    pub from: String,
    pub to: String,
    pub load: f64,
}

/// The severity rollup, as rows.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chip {
    pub kind: &'static str,
    pub id: String,
    pub label: &'static str,
    pub count: i64,
}

/// One figure for a tile: the feed rate, the counts.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metric {
    pub kind: &'static str,
    pub id: String,
    pub label: String,
    pub value: f64,
    pub group: &'static str,
}

/// One change for the router's `handle.apply()`: an upsert or a delete.
#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub op: &'static str,
    pub row: serde_json::Value,
}

impl Change {
    pub fn upsert<T: Serialize>(row: &T) -> Self {
        Self { op: "upsert", row: serde_json::to_value(row).unwrap_or(serde_json::Value::Null) }
    }

    pub fn delete(kind: &str, id: &str) -> Self {
        Self { op: "delete", row: serde_json::json!({ "kind": kind, "id": id }) }
    }
}

/// The status line: where the data comes from, and how much of it there is.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub mode: &'static str,
    pub state: &'static str,
    pub note: String,
    pub messages: u64,
    pub withdrawals: u64,
    pub rate_per_minute: i64,
    pub alarms: i64,
    pub critical: i64,
    pub sites: usize,
    pub at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert: Option<&'static str>,
}
