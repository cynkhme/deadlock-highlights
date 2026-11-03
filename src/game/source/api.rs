//! deadlock-api.com as a match source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use deadlock_api::blocking::{Client, Transport};
use deadlock_api::{AccountId, Error as ApiError, FileCache, MatchId, MatchInfo, RetryPolicy};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::game::events::events;
use crate::game::source::{Fetched, MatchSource, MatchTiming, skip_status};
use crate::output::MatchStatus;
use crate::timeline::align::HistoryMatch;
use crate::timeline::place::Pause;

/// A rate limit longer than the crate's retry policy waits stops further requests: every
/// one would be refused until it ends.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

pub(crate) struct ApiSource {
    api: Client,
    account: u32,
    out: PathBuf,
    /// Set by a long rate limit.
    stopped: Option<Stopped>,
}

/// The `Retry-After` every uncached match reports once a long rate limit stopped requests.
#[derive(Debug, Clone, Copy)]
struct Stopped {
    retry_after_s: Option<u64>,
}

/// Whether the match's metadata is in the crate's file cache under `out`.
fn is_cached(out: &Path, match_id: u64) -> bool {
    out.join("matches")
        .join(match_id.to_string())
        .join("metadata.json")
        .is_file()
}

impl ApiSource {
    /// A source whose client caches under the output directory: the crate stores match
    /// metadata at `matches/{id}/metadata.json` there, so a rerun requests only what it lacks.
    ///
    /// `transport` replaces the network in tests.
    pub(crate) fn new(cfg: &Config, transport: Option<Box<dyn Transport>>) -> Result<ApiSource> {
        let mut b = Client::builder()
            .base_url(cfg.deadlock_api_url.clone())
            .api_key(cfg.deadlock_api_key.clone())
            .cache(FileCache::new(cfg.out.clone()))
            .retry(RetryPolicy::new(3));
        if let Some(t) = transport {
            b = b.transport(t);
        }
        // The base URL is fixed, so a build error means a key that is not a header value.
        let api = b
            .build()
            .map_err(|e| Error::Usage(format!("deadlock-api client: {e}")))?;
        Ok(ApiSource {
            api,
            account: cfg.account,
            out: cfg.out.clone(),
            stopped: None,
        })
    }

    fn failed(&mut self, id: u64, e: &ApiError) -> MatchStatus {
        let retry_after_s = e.retry_after().map(|d| d.as_secs());
        match e {
            ApiError::NotFound { .. } => MatchStatus::NotIngested,
            ApiError::Decode { .. } => MatchStatus::DecodeFailed {
                message: format!(
                    "match {id}: matches/{id}/metadata.json does not decode; \
                     update dlhl's deadlock-api dependency or delete the file"
                ),
            },
            ApiError::RateLimited { retry_after, .. } => {
                // The crate returns a rate limit only after its policy gave up on it.
                if retry_after.is_none_or(|d| d > MAX_RATE_LIMIT_WAIT) {
                    self.stopped = Some(Stopped { retry_after_s });
                }
                MatchStatus::Unavailable { retry_after_s }
            }
            _ => MatchStatus::Unavailable { retry_after_s },
        }
    }

    /// Steam names of the match's players; without them the events name account ids.
    fn names(&self, info: &MatchInfo) -> HashMap<AccountId, String> {
        // After a long rate limit the run sends nothing more.
        if self.stopped.is_some() {
            return HashMap::new();
        }
        let ids: Vec<AccountId> = info.players.iter().map(|p| p.account_id).collect();
        self.api
            .steam_profiles(&ids)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| Some((p.account_id, p.personaname?)))
            .collect()
    }
}

impl MatchSource for ApiSource {
    /// Requests the account's match history once and writes it to `<out>/history.json`.
    fn history(&mut self) -> Result<Vec<HistoryMatch>> {
        let n = self.account;
        let entries = self
            .api
            .match_history(AccountId(n))
            .map_err(|e| Error::History(format!("match history for account {n}: {e}")))?;
        let file = self.out.join("history.json");
        let io = |source| Error::Io {
            path: file.clone(),
            source,
        };
        std::fs::create_dir_all(&self.out).map_err(|source| Error::Io {
            path: self.out.clone(),
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
        Ok(entries
            .iter()
            .filter_map(|e| {
                Some(HistoryMatch {
                    match_id: e.match_id.0,
                    start_time: i64::try_from(e.start_time?).ok()?,
                    duration_s: e.match_duration_s.unwrap_or(0),
                    game_mode: e.game_mode,
                })
            })
            .collect())
    }

    fn fetch(&mut self, h: &HistoryMatch) -> Fetched {
        debug_assert!(
            skip_status(h).is_none(),
            "fetch of a match skip_status rejects"
        );
        // The crate's file cache answers a cached match without a request, so a stopped
        // source still reads it.
        let cached = is_cached(&self.out, h.match_id);
        if let (Some(Stopped { retry_after_s }), false) = (self.stopped, cached) {
            return Fetched::Status(MatchStatus::Unavailable { retry_after_s });
        }
        let id = h.match_id;
        let info = match self.api.match_metadata(MatchId(id)) {
            Ok(m) => m.match_info,
            Err(e) => return Fetched::Status(self.failed(id, &e)),
        };
        let Some(me) = info
            .players
            .iter()
            .find(|p| p.account_id == AccountId(self.account))
        else {
            return Fetched::Status(MatchStatus::NotYours);
        };
        let names = self.names(&info);
        Fetched::Ready {
            timing: MatchTiming {
                duration_s: f64::from(info.duration_s),
                pauses: info
                    .match_pauses
                    .iter()
                    .filter_map(|p| {
                        Some(Pause {
                            game_time_s: f64::from(p.game_time_s?),
                            duration_s: f64::from(p.pause_duration_s?),
                        })
                    })
                    .collect(),
            },
            events: events(&info, me, &names),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeApi, match_info_json, path, player};
    use serde_json::json;
    use std::sync::Arc;

    fn hist(id: u64, duration_s: u32, game_mode: Option<u32>) -> HistoryMatch {
        HistoryMatch {
            match_id: id,
            start_time: 1_793_000_000,
            duration_s,
            game_mode,
        }
    }

    /// Me (account 7) and account 8; a path for me unless `paths` is false.
    fn metadata(me: u32, paths: bool) -> String {
        let paths = paths.then(|| vec![path(1, &[0; 10], &[100; 10], &[])]);
        let info = match_info_json(
            1800,
            vec![
                player(me, 1, 0, &[(100, Some(2), None)]),
                player(8, 2, 1, &[]),
            ],
            paths,
            &json!({"match_pauses": [{"game_time_s": 50, "pause_duration_s": 20}]}),
        );
        json!({"match_info": info}).to_string()
    }

    fn setup() -> (tempfile::TempDir, Arc<FakeApi>, ApiSource) {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(d.path().join("v.mp4"), 7, "http://127.0.0.1:9/v1", "m");
        cfg.out = d.path().to_path_buf();
        cfg.deadlock_api_url = "http://fake.test".into();
        let api = FakeApi::with(&[]);
        let src = ApiSource::new(&cfg, Some(Box::new(api.clone()))).unwrap();
        (d, api, src)
    }

    fn names_ok(api: &FakeApi) {
        api.push(
            "/v1/players/steam",
            200,
            &json!([{"account_id": 7, "personaname": "Me"}, {"account_id": 8, "personaname": "Foe"}]).to_string(),
            None,
        );
    }

    #[test]
    fn placed_ready() {
        let (d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(7, true), None);
        names_ok(&api);
        let Fetched::Ready { timing, events } = src.fetch(&hist(1, 1800, Some(1))) else {
            panic!("not ready")
        };
        assert!((timing.duration_s - 1800.0).abs() < 1e-9);
        assert_eq!(
            timing.pauses,
            [Pause {
                game_time_s: 50.0,
                duration_s: 20.0
            }]
        );
        assert!(events.has_paths);
        assert_eq!(events.kills[0].victim, "Me");
        assert_eq!(events.kills[0].killer, "Foe");
        assert!(is_cached(d.path(), 1));
        assert!(!is_cached(d.path(), 2));
    }

    #[test]
    fn not_yours() {
        let (_d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(9, true), None);
        assert!(matches!(
            src.fetch(&hist(1, 1800, Some(1))),
            Fetched::Status(MatchStatus::NotYours)
        ));
        assert_eq!(api.requests(), ["/v1/matches/1/metadata"]);
    }

    #[test]
    fn not_ingested_on_404() {
        let (_d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 404, "{}", None);
        assert!(matches!(
            src.fetch(&hist(1, 1800, Some(1))),
            Fetched::Status(MatchStatus::NotIngested)
        ));
    }

    #[test]
    fn decode_failed_names_file() {
        let (d, api, mut src) = setup();
        std::fs::create_dir_all(d.path().join("matches/1")).unwrap();
        std::fs::write(
            d.path().join("matches/1/metadata.json"),
            r#"{"match_info": {"match_id": "x"}}"#,
        )
        .unwrap();
        let Fetched::Status(MatchStatus::DecodeFailed { message }) =
            src.fetch(&hist(1, 1800, Some(1)))
        else {
            panic!("not decode failed")
        };
        assert_eq!(
            message,
            "match 1: matches/1/metadata.json does not decode; update dlhl's deadlock-api dependency or delete the file"
        );
        assert_eq!(api.requests(), Vec::<String>::new());
    }

    #[test]
    fn long_429_stops_further_requests() {
        let (d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 429, "{}", Some(3600));
        std::fs::create_dir_all(d.path().join("matches/4")).unwrap();
        std::fs::write(d.path().join("matches/4/metadata.json"), metadata(7, true)).unwrap();
        for id in 1..=3 {
            assert!(
                matches!(
                    src.fetch(&hist(id, 1800, Some(1))),
                    Fetched::Status(MatchStatus::Unavailable {
                        retry_after_s: Some(3600)
                    })
                ),
                "match {id}"
            );
        }
        assert!(matches!(
            src.fetch(&hist(4, 1800, Some(1))),
            Fetched::Ready { .. }
        ));
        assert_eq!(api.requests(), ["/v1/matches/1/metadata"]);
    }

    #[test]
    fn short_429_does_not_stop() {
        let (_d, api, mut src) = setup();
        // Three 429s of 1 s: the crate retries, then gives up; the next match is still tried.
        for _ in 0..3 {
            api.push("/v1/matches/1/metadata", 429, "{}", Some(0));
        }
        api.push("/v1/matches/2/metadata", 404, "{}", None);
        assert!(matches!(
            src.fetch(&hist(1, 1800, Some(1))),
            Fetched::Status(MatchStatus::Unavailable {
                retry_after_s: Some(0)
            })
        ));
        assert!(matches!(
            src.fetch(&hist(2, 1800, Some(1))),
            Fetched::Status(MatchStatus::NotIngested)
        ));
    }

    #[test]
    fn server_error_is_unavailable() {
        let (_d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 500, "{}", None);
        assert!(matches!(
            src.fetch(&hist(1, 1800, Some(1))),
            Fetched::Status(MatchStatus::Unavailable {
                retry_after_s: None
            })
        ));
    }

    #[test]
    fn names_failure_falls_back_to_ids() {
        let (_d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(7, true), None);
        api.push("/v1/players/steam", 500, "{}", None);
        let Fetched::Ready { events, .. } = src.fetch(&hist(1, 1800, Some(1))) else {
            panic!("not ready")
        };
        assert_eq!(events.kills[0].victim, "7");
        assert_eq!(events.kills[0].killer, "8");
    }

    #[test]
    fn no_paths_is_ready_with_has_paths_false() {
        let (_d, api, mut src) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(7, false), None);
        names_ok(&api);
        let Fetched::Ready { events, .. } = src.fetch(&hist(1, 1800, Some(1))) else {
            panic!("not ready")
        };
        assert!(!events.has_paths);
    }

    fn entry(id: u64, start: Option<i64>, dur: Option<u32>) -> serde_json::Value {
        json!({"account_id": 7, "match_id": id, "start_time": start,
            "match_duration_s": dur, "game_mode": 1})
    }

    #[test]
    fn history_written_and_parsed() {
        const A: i64 = 1_793_000_000;
        let body = json!([
            entry(2, Some(A + 100), Some(1800)),
            entry(3, None, Some(5)),
            entry(4, Some(A), None)
        ]);
        let (d, api, mut src) = setup();
        api.push("/v1/players/7/match-history", 200, &body.to_string(), None);
        let got: Vec<(u64, i64, u32)> = src
            .history()
            .unwrap()
            .iter()
            .map(|m| (m.match_id, m.start_time, m.duration_s))
            .collect();
        assert_eq!(got, [(2, A + 100, 1800), (4, A, 0)]);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("history.json")).unwrap())
                .unwrap();
        assert_eq!(written.as_array().unwrap().len(), 3);
        assert_eq!(api.requests(), ["/v1/players/7/match-history"]);
    }

    #[test]
    fn history_empty_is_error() {
        let (_d, api, mut src) = setup();
        api.push("/v1/players/7/match-history", 200, "[]", None);
        let e = src.history().unwrap_err();
        assert_eq!(
            e.to_string(),
            Error::History("account 7 has no match history".into()).to_string()
        );
    }

    #[test]
    fn history_of_startless_entries_is_not_empty() {
        let (_d, api, mut src) = setup();
        let body = json!([entry(3, None, Some(5))]);
        api.push("/v1/players/7/match-history", 200, &body.to_string(), None);
        assert_eq!(src.history().unwrap(), Vec::new());
    }

    #[test]
    fn history_500_is_error() {
        let (_d, api, mut src) = setup();
        api.push("/v1/players/7/match-history", 500, "{}", None);
        let e = src.history().unwrap_err();
        assert!(
            matches!(&e, Error::History(m) if m.starts_with("match history for account 7: ")),
            "{e}"
        );
        // The crate retries a 503 or a 429, not a 500.
        assert_eq!(api.requests().len(), 1);
    }

    #[test]
    fn history_503_retried_three_times_then_error() {
        let (_d, api, mut src) = setup();
        for _ in 0..3 {
            api.push("/v1/players/7/match-history", 503, "{}", None);
        }
        let e = src.history().unwrap_err();
        assert!(
            matches!(&e, Error::History(m) if m.starts_with("match history for account 7: ")),
            "{e}"
        );
        assert_eq!(api.requests().len(), 3);
    }
}
