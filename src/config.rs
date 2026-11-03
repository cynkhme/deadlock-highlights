use std::path::PathBuf;
use std::sync::Arc;

use crate::output::Progress;
use crate::{Error, Profile};

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
    /// The key sent to `server`.
    pub server_api_key: Option<String>,
    pub deadlock_api_key: Option<String>,
    /// Concurrent reader requests.
    pub parallel: u16,
    /// Windows scoring at least this are cut as clips.
    pub clips_min_score: u32,
    pub clips_max: Option<usize>,
    pub progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
    pub(crate) deadlock_api_url: String,
}

impl Config {
    /// Concurrent reader requests unless set otherwise.
    pub const DEFAULT_PARALLEL: u16 = 1;
    /// The clip threshold unless set otherwise: for example an own kill and an assist, or three
    /// assists.
    pub const DEFAULT_CLIPS_MIN_SCORE: u32 =
        crate::highlights::OWN_KILL_POINTS + crate::highlights::ASSIST_POINTS;

    pub fn new(
        vod: impl Into<PathBuf>,
        account: u32,
        server: impl Into<String>,
        model: impl Into<String>,
    ) -> Config {
        let vod = vod.into();
        let mut out = vod.clone().into_os_string();
        out.push(".dlhl");
        Config {
            vod,
            out: out.into(),
            account,
            vod_start: None,
            profile: Profile::default(),
            server: server.into(),
            model: model.into(),
            server_api_key: None,
            deadlock_api_key: None,
            parallel: Config::DEFAULT_PARALLEL,
            clips_min_score: Config::DEFAULT_CLIPS_MIN_SCORE,
            clips_max: None,
            progress: None,
            deadlock_api_url: DEADLOCK_API_URL.into(),
        }
    }

    /// Parses a VOD start time: unix seconds, or RFC 3339 with an explicit offset.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] for anything else, including a time without a zone, so a local
    /// time cannot pass as UTC.
    pub fn parse_vod_start(s: &str) -> Result<i64, Error> {
        let usage = || {
            Error::Usage(format!(
                "{s:?}: needs unix seconds or RFC 3339 with an offset, e.g. 2026-11-02T18:00:00+01:00"
            ))
        };
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s.parse().map_err(|_| usage());
        }
        rfc3339(s).ok_or_else(usage)
    }
}

fn rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let p = text.get(r)?;
        p.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| p.parse().ok())?
    };
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &frac[n..];
    }
    let offset = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let oh: i64 = rest[1..3].parse().ok()?;
            let om: i64 = rest[4..6].parse().ok()?;
            if oh > 23 || om > 59 {
                return None;
            }
            let off = oh * 3600 + om * 60;
            if *sign == b'+' { off } else { -off }
        }
        _ => return None,
    };
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vod_start_unix_and_rfc3339() {
        assert_eq!(Config::parse_vod_start("1793000000").unwrap(), 1793000000);
        assert_eq!(
            Config::parse_vod_start("2026-11-02T18:00:00+01:00").unwrap(),
            1793638800
        );
        assert_eq!(
            Config::parse_vod_start("2026-11-02T17:00:00Z").unwrap(),
            1793638800
        );
        assert_eq!(
            Config::parse_vod_start("2026-11-02T17:00:00.5Z").unwrap(),
            1793638800
        );
        assert_eq!(
            Config::parse_vod_start("2026-11-02t17:00:00z").unwrap(),
            1793638800
        );
        assert_eq!(
            Config::parse_vod_start("2026-11-02T12:30:00-04:30").unwrap(),
            1793638800
        );
        assert_eq!(Config::parse_vod_start("1970-01-01T00:00:00Z").unwrap(), 0);
    }

    #[test]
    fn vod_start_without_zone_is_usage_error() {
        for bad in [
            "2026-11-02T18:00:00",
            "yesterday",
            "2026-13-02T18:00:00Z",
            "2026-11-02T25:00:00Z",
            "",
        ] {
            let e = Config::parse_vod_start(bad).unwrap_err();
            assert!(
                matches!(&e, crate::Error::Usage(m) if m.contains("RFC 3339") && !m.contains("--")),
                "{bad:?}: {e}"
            );
        }
    }

    #[test]
    fn config_defaults() {
        let c = Config::new("/v/a.mp4", 7, "http://127.0.0.1:9/v1", "m");
        assert_eq!(c.out, PathBuf::from("/v/a.mp4.dlhl"));
        assert_eq!(c.account, 7);
        assert_eq!(c.parallel, Config::DEFAULT_PARALLEL);
        assert_eq!(c.clips_min_score, Config::DEFAULT_CLIPS_MIN_SCORE);
        // The README states this value as an own kill and an assist, or three assists.
        assert_eq!(Config::DEFAULT_CLIPS_MIN_SCORE, 3);
        assert_eq!(
            (c.server.as_str(), c.model.as_str()),
            ("http://127.0.0.1:9/v1", "m")
        );
        assert_eq!(c.profile, Profile::default());
        assert!(c.vod_start.is_none() && c.clips_max.is_none());
    }
}
