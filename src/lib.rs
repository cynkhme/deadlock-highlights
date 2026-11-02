//! Highlight windows and clips for Deadlock VODs, from the game clock on screen and
//! deadlock-api.com match data.

mod align;
mod api;
mod clock;
mod config;
mod error;
mod events;
mod fetch;
mod gate;
mod history;
mod output;
mod pipeline;
mod place;
mod profile;
mod read;
mod score;
#[cfg(test)]
mod test_support;
mod track;
mod video;

pub use config::{Config, Progress};
pub use error::Error;
pub use history::AnchorSource;
pub use output::{
    Alignment, ClockSegment, Degraded, Event, Highlight, MatchReport, MatchStatus, Output,
    UnseenMatch, VodInfo,
};
pub use profile::Profile;

/// Runs every stage on `cfg.vod` and returns what it found; writes the caches, history and
/// clips under `cfg.out`, but not `highlights.json` (the binary does).
///
/// # Errors
///
/// Any stage failure that leaves nothing to place: a usage error, ffmpeg, the model
/// server, the history, no clock or game start, or an offset the history does not accept.
pub fn run(cfg: &Config) -> Result<Output, Error> {
    // The key policy is pure, so it fails before ffmpeg runs.
    read::openai::check_key_policy(&cfg.server, cfg.openai_api_key.as_deref())?;
    let probe = video::probe(&cfg.vod)?;
    // The model list is checked before the slow keyframe pass.
    let reader =
        read::openai::OpenAiChat::new(&cfg.server, &cfg.model, cfg.openai_api_key.as_deref())?;
    pipeline::run_with(cfg, &reader, pipeline::FrameSource::Vod(probe), None)
}

/// The read-cache key of `cfg` on this VOD.
fn cache_key(
    cfg: &Config,
    probe: &video::Probe,
    reader: &dyn read::HudReader,
) -> read::cache::CacheKey {
    read::cache::CacheKey {
        band: cfg.profile.band.clone(),
        t: cfg.profile.t,
        reader: reader.identity(),
        grid_s: video::GRID_S,
        vod_size: probe.size,
        vod_mtime: probe.mtime,
        read_format: read::cache::READ_FORMAT,
    }
}
