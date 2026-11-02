use std::path::PathBuf;

/// Why a run stopped before it could place any match.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A bad flag value or combination; the binary exits with 2.
    #[error("{0}")]
    Usage(String),
    /// ffmpeg or ffprobe is missing, or the VOD cannot be read.
    #[error("{0}")]
    Ffmpeg(String),
    /// The match history is unavailable, empty, or does not reach the VOD.
    #[error("{0}")]
    History(String),
    #[error(
        "no frame shows the clock in the band {band} (best gate score {best_gate:.2}); pass --profile for another HUD layout"
    )]
    NoClock { band: String, best_gate: f32 },
    /// The model server is down, stalled, refused the request, or failed too many reads.
    #[error("{0}")]
    Reader(String),
    #[error("no game start: {valid_clocks} valid clocks, none 3 within 5 s")]
    NoGameStart { valid_clocks: usize },
    #[error(
        "offset not accepted: {} scores {score} of {starts} game starts; pass --vod-start with the VOD's start time",
        offset.map_or_else(|| "no offset".to_owned(), |o| format!("best offset {o}"))
    )]
    OffsetRejected {
        offset: Option<i64>,
        score: usize,
        starts: usize,
        anchored: bool,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
