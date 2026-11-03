//! The account's match history, and the VOD's wall-clock anchor if one is known.

use std::path::Path;

use deadlock_api::AccountId;
use deadlock_api::blocking::Client;
use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::timeline::align::{ANCHOR_WINDOW_S, HistoryMatch};
use crate::video::Probe;

/// An info.json whose duration differs from the VOD's by more than this describes
/// another file, such as the whole VOD of a section download.
const INFO_JSON_DURATION_TOLERANCE_S: f64 = 10.0;

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

/// Requests the account's match history once and writes it to `<out>/history.json`.
pub(crate) fn load_history(
    api: &Client,
    cfg: &Config,
    probe: &Probe,
    warn: &dyn Fn(String),
) -> Result<History> {
    let n = cfg.account;
    let entries = api
        .match_history(AccountId(n))
        .map_err(|e| Error::History(format!("match history for account {n}: {e}")))?;
    let file = cfg.out.join("history.json");
    let io = |source| Error::Io {
        path: file.clone(),
        source,
    };
    std::fs::create_dir_all(&cfg.out).map_err(|source| Error::Io {
        path: cfg.out.clone(),
        source,
    })?;
    std::fs::write(
        &file,
        serde_json::to_vec(&entries).expect("history entries serialise"),
    )
    .map_err(io)?;
    if entries.is_empty() {
        return Err(Error::History(format!("account {n} has no match history")));
    }
    // An entry without a start cannot be placed on the wall clock.
    let matches: Vec<HistoryMatch> = entries
        .iter()
        .filter_map(|e| {
            Some(HistoryMatch {
                match_id: e.match_id.0,
                start_time: i64::try_from(e.start_time?).ok()?,
                duration_s: e.match_duration_s.unwrap_or(0),
                game_mode: e.game_mode,
            })
        })
        .collect();
    let anchor = cfg
        .vod_start
        .map(|v| (v, AnchorSource::Flag))
        .or_else(|| info_json_start(&cfg.vod, probe, warn).map(|v| (v, AnchorSource::InfoJson)));
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

/// The `timestamp` of `<vod stem>.info.json`; a missing or unparsable file is no anchor.
fn info_json_start(vod: &Path, probe: &Probe, warn: &dyn Fn(String)) -> Option<i64> {
    let file = vod.with_file_name(format!("{}.info.json", vod.file_stem()?.to_string_lossy()));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let timestamp = v.get("timestamp")?.as_i64()?;
    let duration = v.get("duration")?.as_f64()?;
    if (duration - probe.duration_s).abs() > INFO_JSON_DURATION_TOLERANCE_S {
        warn(format!(
            "ignoring {}: its duration {duration} s differs from the VOD's {} s (a section download?)",
            file.display(),
            probe.duration_s
        ));
        return None;
    }
    Some(timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let client = crate::game::api::client(cfg, Some(Box::new(api.clone()))).unwrap();
        let warnings = RefCell::new(Vec::new());
        let r = load_history(&client, cfg, probe, &|w| warnings.borrow_mut().push(w));
        (r, warnings.into_inner())
    }

    #[test]
    fn history_written_and_parsed() {
        let body = serde_json::json!([
            entry(2, Some(A + 100), Some(1800)),
            entry(3, None, Some(5)),
            entry(4, Some(A), None)
        ]);
        let (d, cfg, probe, api) = setup(&body, None);
        let (h, w) = run(&cfg, &probe, &api);
        let h = h.unwrap();
        assert_eq!(w, Vec::<String>::new());
        assert_eq!(h.anchor, None);
        let got: Vec<(u64, i64, u32)> = h
            .matches
            .iter()
            .map(|m| (m.match_id, m.start_time, m.duration_s))
            .collect();
        assert_eq!(got, [(2, A + 100, 1800), (4, A, 0)]);
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join("stream.mp4.dlhl/history.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written.as_array().unwrap().len(), 3);
        assert_eq!(api.requests(), ["/v1/players/7/match-history"]);
    }

    #[test]
    fn history_empty_is_error() {
        let (_d, cfg, probe, api) = setup(&serde_json::json!([]), None);
        let e = run(&cfg, &probe, &api).0.unwrap_err();
        assert_eq!(
            e.to_string(),
            Error::History("account 7 has no match history".into()).to_string()
        );
    }

    #[test]
    fn history_500_is_error() {
        let (_d, cfg, probe, _) = setup(&serde_json::json!([]), None);
        let api = FakeApi::with(&[("/v1/players/7/match-history", &[(500, "{}")])]);
        let e = run(&cfg, &probe, &api).0.unwrap_err();
        assert!(
            matches!(&e, Error::History(m) if m.starts_with("match history for account 7: ")),
            "{e}"
        );
        // The crate retries a 503 or a 429, not a 500.
        assert_eq!(api.requests().len(), 1);
    }

    #[test]
    fn history_503_retried_three_times_then_error() {
        let (_d, cfg, probe, _) = setup(&serde_json::json!([]), None);
        let api = FakeApi::with(&[(
            "/v1/players/7/match-history",
            &[(503, "{}"), (503, "{}"), (503, "{}")],
        )]);
        let e = run(&cfg, &probe, &api).0.unwrap_err();
        assert!(
            matches!(&e, Error::History(m) if m.starts_with("match history for account 7: ")),
            "{e}"
        );
        assert_eq!(api.requests().len(), 3);
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
    fn history_of_startless_entries_is_not_empty() {
        let (_d, cfg, probe, api) = setup(&serde_json::json!([entry(3, None, Some(5))]), None);
        let h = run(&cfg, &probe, &api).0.unwrap();
        assert_eq!(h.matches, Vec::new());
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
