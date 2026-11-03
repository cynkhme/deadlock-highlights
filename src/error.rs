use std::path::PathBuf;

/// Why a run stopped before it could place any match.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A bad setting value or combination.
    #[error("{0}")]
    Usage(String),
    /// An API key would cross the network in clear text: the server is neither https nor
    /// loopback.
    #[error("an API key is only sent over https or to a loopback server, not to {server}")]
    InsecureKey { server: String },
    /// ffmpeg or ffprobe is missing, or the VOD cannot be read.
    #[error("{0}")]
    Ffmpeg(String),
    /// The match history is unavailable, empty, or does not reach the VOD.
    #[error("{0}")]
    History(String),
    #[error("no frame shows the clock in the band {band} (best gate score {best_gate:.2})")]
    NoClock { band: String, best_gate: f32 },
    /// Frames passed the clock gate, but no answer of the model held a valid clock: the model
    /// or its server failed them, as with empty answers from a model that spends its token
    /// budget on hidden thinking.
    #[error(
        "the model answered {answered} frames that passed the clock gate, none with a valid clock (first answer: {first:?})"
    )]
    NoClockAnswer { answered: usize, first: String },
    /// The model server is down, stalled, refused the request, or failed too many reads.
    #[error("{0}")]
    Reader(String),
    #[error("no game start: {valid_clocks} valid clocks, none 3 within 5 s")]
    NoGameStart { valid_clocks: usize },
    #[error(
        "offset not accepted: {} hits {hits} of {start_count} game starts",
        offset.map_or_else(|| "no offset".to_owned(), |o| format!("best offset {o}"))
    )]
    OffsetRejected {
        offset: Option<i64>,
        hits: usize,
        start_count: usize,
        anchored: bool,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
