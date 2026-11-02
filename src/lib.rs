//! Highlight windows and clips for Deadlock VODs, from the game clock on screen and
//! deadlock-api.com match data.

mod config;
mod error;
mod game;
mod highlights;
mod hud;
mod output;
mod pipeline;
#[cfg(test)]
mod test_support;
mod timeline;
mod video;

pub use config::Config;
pub use error::Error;
pub use game::history::AnchorSource;
pub use hud::profile::Profile;
pub use output::{
    Alignment, Degraded, Event, Highlight, MatchReport, MatchStatus, Output, Progress,
    UnmatchedStart, UnseenMatch, VodInfo,
};

/// Runs every stage on `cfg.vod` and returns what it found; writes the caches, history and
/// clips under `cfg.out`.
///
/// # Errors
///
/// Any stage failure that leaves nothing to place: a usage error, ffmpeg, the model
/// server, the history, no clock or game start, or an offset the history does not accept.
pub fn run(cfg: &Config) -> Result<Output, Error> {
    // Construction checks only the key policy, so a refused key fails before ffmpeg runs.
    let reader = hud::reader::openai::OpenAiChat::new(
        &cfg.server,
        &cfg.model,
        cfg.server_api_key.as_deref(),
    )?;
    let probe = video::probe(&cfg.vod)?;
    pipeline::run(cfg, &reader, &probe)
}
