//! What a run returns. The binary writes it as `highlights.json`; field names are the
//! Rust names, versioned by `Output::schema`.

use serde::Serialize;

/// One scored moment on the VOD.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one event"
)]
pub struct Event {
    pub vod_time: f64,
    pub game_time: f64,
    pub label: String,
    pub my_kill: bool,
    pub my_death: bool,
    pub my_assist: bool,
    pub objective: bool,
}

/// Events at most 12 s apart, padded, with their score and density.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Highlight {
    pub match_id: u64,
    pub start: f64,
    pub end: f64,
    pub score: u32,
    /// Score per second of the events' span plus 10 s.
    pub density: f64,
    pub deaths: u32,
    pub my_kills: u32,
    pub my_assists: u32,
    pub my_deaths: u32,
    pub objectives: u32,
    pub events: Vec<Event>,
}
