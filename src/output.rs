//! What a run returns, serialisable as JSON: field names are the Rust names, versioned by
//! `Output::schema`.

use std::path::PathBuf;

use serde::Serialize;

use crate::game::history::AnchorSource;

/// The schema version of a serialised [`Output`].
pub(crate) const SCHEMA: u32 = 1;

/// Everything a run found, best highlights first.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Output {
    pub schema: u32,
    pub vod: VodInfo,
    pub alignment: Alignment,
    /// Every match a game start was assigned to, in VOD order.
    pub matches: Vec<MatchReport>,
    /// Game starts with no history match.
    pub unmatched_starts: Vec<UnmatchedStart>,
    pub unseen: Vec<UnseenMatch>,
    /// Whether a rerun may place more: some match was unavailable or not ingested yet, or
    /// an unmatched start was read after the newest history match started.
    pub retryable: bool,
    pub highlights: Vec<Highlight>,
    /// Clip files of this run's settings, cut now or kept from an earlier run.
    pub clips: Vec<PathBuf>,
}

/// The VOD a run describes.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct VodInfo {
    pub path: PathBuf,
    pub size: u64,
    /// Unix seconds.
    pub mtime: i64,
    pub duration_s: f64,
}

/// The VOD's wall-clock offset, and how well the history agrees with it.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Alignment {
    /// Unix seconds of VOD time 0.
    pub offset: i64,
    pub anchor: Option<AnchorSource>,
    /// Game starts that fall in some history match at this offset.
    pub hits: usize,
    pub start_count: usize,
}

/// One history match a game start was assigned to.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct MatchReport {
    pub match_id: u64,
    /// Unix seconds, from the history.
    pub start_time: i64,
    /// VOD time of the match's clock 0:00, when placed.
    pub vod_start: Option<f64>,
    pub status: MatchStatus,
    pub degraded: Vec<Degraded>,
}

/// Why a placed match's highlights may be incomplete.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Degraded {
    /// The metadata has no path for the streamer: no assists, no proximity, no leave.
    NoPaths,
    /// The gaps between the match's clock clusters disagree with its metadata pauses by
    /// more than 5 s, so a pause may be missing from the metadata.
    PauseMismatch {
        gaps_s: Vec<i64>,
        pauses_s: Vec<f64>,
    },
}

/// A game start with no history match.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct UnmatchedStart {
    /// VOD time of the start's first clock reading.
    pub first_reading: f64,
    /// VOD time of its last clock reading.
    pub last_reading: f64,
    pub readings: usize,
}

/// A history match inside the aligned VOD span that got no game start.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct UnseenMatch {
    pub match_id: u64,
    pub start_time: i64,
}

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

/// What became of one history match.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MatchStatus {
    /// Placed on the VOD; its highlights are listed.
    Placed,
    /// Shorter than 180 s: no request is made.
    Remake,
    /// A mode other than the standard one, such as Street Brawl, whose clock restarts
    /// every round: no request is made.
    UnsupportedMode { game_mode: Option<u32> },
    /// The account is not among the match's players.
    NotYours,
    /// deadlock-api has not ingested the match yet (404).
    NotIngested,
    /// Rate limited, overloaded or unreachable; a rerun may succeed.
    Unavailable { retry_after_s: Option<u64> },
    /// The cached metadata does not decode.
    DecodeFailed { message: String },
}

/// Progress reported while a run goes through its stages.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Progress {
    Probed {
        duration_s: f64,
    },
    History {
        matches: usize,
        anchor: Option<AnchorSource>,
    },
    /// Frames read so far, of `total`.
    Read {
        done: usize,
        total: usize,
    },
    Aligned {
        offset: i64,
        start_count: usize,
    },
    Match(MatchReport),
    Warning(String),
}
