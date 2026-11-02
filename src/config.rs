use std::path::PathBuf;
use std::sync::Arc;

use crate::Profile;

const DEADLOCK_API_URL: &str = "https://api.deadlock-api.com";

/// What to run on, and how.
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
    pub vod: PathBuf,
    /// Output directory, `<vod>.dlhl` by default.
    pub out: PathBuf,
    /// The 32-bit Steam account id of the streamer.
    pub account: u32,
    /// The VOD's wall-clock start, unix seconds.
    pub vod_start: Option<i64>,
    pub profile: Profile,
    /// Base URL of an OpenAI-compatible chat server.
    pub server: String,
    pub model: String,
    pub openai_api_key: Option<String>,
    pub deadlock_api_key: Option<String>,
    /// Concurrent reader requests.
    pub parallel: usize,
    /// Windows at or above this density are cut as clips.
    pub clips_min_density: f64,
    pub clips_max: Option<usize>,
    pub progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
    pub(crate) api_base_url: String,
}

impl Config {
    pub fn new(vod: impl Into<PathBuf>, account: u32) -> Config {
        let vod = vod.into();
        let mut out = vod.clone().into_os_string();
        out.push(".dlhl");
        Config {
            vod,
            out: out.into(),
            account,
            vod_start: None,
            profile: Profile::default(),
            server: "http://localhost:11434/v1".into(),
            model: "gemma4:e4b".into(),
            openai_api_key: None,
            deadlock_api_key: None,
            parallel: 1,
            clips_min_density: 0.30,
            clips_max: None,
            progress: None,
            api_base_url: DEADLOCK_API_URL.into(),
        }
    }
}

/// Progress reported while a run goes through its stages.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Progress {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp, reason = "an exact default value")]
    fn config_defaults() {
        let c = Config::new("/v/a.mp4", 7);
        assert_eq!(c.out, PathBuf::from("/v/a.mp4.dlhl"));
        assert_eq!(c.account, 7);
        assert_eq!(c.parallel, 1);
        assert_eq!(c.clips_min_density, 0.30);
        assert_eq!(c.server, "http://localhost:11434/v1");
        assert_eq!(c.model, "gemma4:e4b");
        assert_eq!(c.profile, Profile::default());
        assert!(c.vod_start.is_none() && c.clips_max.is_none());
    }
}
