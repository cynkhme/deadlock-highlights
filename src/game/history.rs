//! The account's match history from a source, and the VOD's wall-clock anchor if one is known.

use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::game::source::MatchSource;
use crate::timeline::align::{ANCHOR_WINDOW_S, HistoryMatch};
use crate::video::{self, Probe};

/// Where the VOD's wall-clock start came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AnchorSource {
    /// [`Config::vod_start`](crate::Config::vod_start).
    Flag,
    /// yt-dlp's `<vod stem>.info.json` beside the VOD.
    InfoJson,
}

#[derive(Debug)]
pub(crate) struct History {
    pub matches: Vec<HistoryMatch>,
    /// The VOD's start as unix seconds.
    pub anchor: Option<(i64, AnchorSource)>,
}

/// The account's matches from `source`, and the VOD's anchor if one is known; an anchor
/// no match overlaps is an error.
pub(crate) fn load_history(
    source: &mut dyn MatchSource,
    cfg: &Config,
    probe: &Probe,
    warn: &dyn Fn(String),
) -> Result<History> {
    let n = cfg.account;
    let matches = source.history()?;
    let anchor = cfg.vod_start.map(|v| (v, AnchorSource::Flag)).or_else(|| {
        video::info_json_start(&cfg.vod, probe, warn).map(|v| (v, AnchorSource::InfoJson))
    });
    if let Some((a, _)) = anchor {
        let span = probe.duration_s.round() as i64;
        let overlaps = matches.iter().any(|m| {
            m.start_time <= a + span + ANCHOR_WINDOW_S
                && m.start_time + i64::from(m.duration_s) >= a - ANCHOR_WINDOW_S
        });
        if !overlaps {
            return Err(Error::History(format!(
                "no match of account {n} overlaps the VOD's span around {a} (±{ANCHOR_WINDOW_S} s)"
            )));
        }
    }
    Ok(History { matches, anchor })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::source::api::ApiSource;
    use crate::test_support::FakeApi;
    use std::cell::RefCell;

    const A: i64 = 1_793_000_000;

    fn entry(id: u64, start: Option<i64>, dur: Option<u32>) -> serde_json::Value {
        serde_json::json!({"account_id": 7, "match_id": id, "start_time": start,
            "match_duration_s": dur, "game_mode": 1})
    }

    fn setup(
        body: &serde_json::Value,
        vod_start: Option<i64>,
    ) -> (tempfile::TempDir, Config, Probe, std::sync::Arc<FakeApi>) {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(d.path().join("stream.mp4"), 7, "http://127.0.0.1:9/v1", "m");
        cfg.deadlock_api_url = "http://fake.test".into();
        cfg.vod_start = vod_start;
        let api = FakeApi::with(&[(
            "/v1/players/7/match-history",
            &[(200, body.to_string().leak())],
        )]);
        let probe = Probe {
            duration_s: 3600.0,
            width: 1920,
            height: 1080,
            size: 1,
            mtime: 0,
        };
        (d, cfg, probe, api)
    }

    fn run(
        cfg: &Config,
        probe: &Probe,
        api: &std::sync::Arc<FakeApi>,
    ) -> (Result<History>, Vec<String>) {
        let mut src = ApiSource::new(cfg, Some(Box::new(api.clone()))).unwrap();
        let warnings = RefCell::new(Vec::new());
        let r = load_history(&mut src, cfg, probe, &|w| warnings.borrow_mut().push(w));
        (r, warnings.into_inner())
    }

    fn info_json(cfg: &Config, timestamp: i64, duration: f64) {
        std::fs::write(
            cfg.vod.with_file_name("stream.info.json"),
            serde_json::json!({"timestamp": timestamp, "duration": duration}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn anchor_flag_wins_over_info_json() {
        let (_d, cfg, probe, api) = setup(
            &serde_json::json!([entry(2, Some(A), Some(1800))]),
            Some(A + 5),
        );
        info_json(&cfg, A, 3600.0);
        assert_eq!(
            run(&cfg, &probe, &api).0.unwrap().anchor,
            Some((A + 5, AnchorSource::Flag))
        );
    }

    #[test]
    fn info_json_used_when_duration_matches() {
        let (_d, cfg, probe, api) =
            setup(&serde_json::json!([entry(2, Some(A), Some(1800))]), None);
        info_json(&cfg, A, 3609.5);
        let (h, w) = run(&cfg, &probe, &api);
        assert_eq!(h.unwrap().anchor, Some((A, AnchorSource::InfoJson)));
        assert_eq!(w, Vec::<String>::new());
    }

    #[test]
    fn info_json_ignored_with_warning_when_duration_differs() {
        let (_d, cfg, probe, api) =
            setup(&serde_json::json!([entry(2, Some(A), Some(1800))]), None);
        info_json(&cfg, A, 20940.0);
        let (h, w) = run(&cfg, &probe, &api);
        assert_eq!(h.unwrap().anchor, None);
        let f = cfg.vod.with_file_name("stream.info.json");
        assert_eq!(
            w,
            [format!(
                "ignoring {}: its duration 20940 s differs from the VOD's 3600 s (a section download?)",
                f.display()
            )]
        );
    }

    #[test]
    fn unparsable_info_json_is_silent() {
        let (_d, cfg, probe, api) =
            setup(&serde_json::json!([entry(2, Some(A), Some(1800))]), None);
        std::fs::write(cfg.vod.with_file_name("stream.info.json"), "{").unwrap();
        let (h, w) = run(&cfg, &probe, &api);
        assert_eq!(h.unwrap().anchor, None);
        assert_eq!(w, Vec::<String>::new());
    }

    #[test]
    fn anchor_without_overlap_is_error() {
        // The VOD spans [A, A + 3600]; a match ending 601 s before A misses the window.
        let (_d, cfg, probe, api) = setup(
            &serde_json::json!([
                entry(2, Some(A - 2401), Some(1800)),
                entry(3, Some(A + 4201), Some(60))
            ]),
            Some(A),
        );
        let e = run(&cfg, &probe, &api).0.unwrap_err();
        assert_eq!(
            e.to_string(),
            Error::History(format!(
                "no match of account 7 overlaps the VOD's span around {A} (±600 s)"
            ))
            .to_string()
        );
    }

    #[test]
    fn vod_starting_inside_a_match_passes_overlap() {
        let (_d, cfg, probe, api) = setup(
            &serde_json::json!([entry(2, Some(A - 900), Some(1800))]),
            Some(A),
        );
        assert!(run(&cfg, &probe, &api).0.is_ok());
    }

    #[test]
    fn overlap_edges_are_inclusive() {
        for start in [A - 2400, A + 4200] {
            let (_d, cfg, probe, api) = setup(
                &serde_json::json!([entry(2, Some(start), Some(1800))]),
                Some(A),
            );
            assert!(run(&cfg, &probe, &api).0.is_ok(), "start {start}");
        }
    }

    #[test]
    fn load_history_without_anchor_is_silent() {
        let (_d, cfg, probe, api) =
            setup(&serde_json::json!([entry(2, Some(A), Some(1800))]), None);
        let (h, w) = run(&cfg, &probe, &api);
        assert_eq!(h.unwrap().anchor, None);
        assert_eq!(w, Vec::<String>::new());
    }
}
