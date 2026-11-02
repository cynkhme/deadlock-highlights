//! Each history match's metadata, or the reason it has none.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use deadlock_api::blocking::Client;
use deadlock_api::{AccountId, Error as ApiError, MatchId, MatchInfo};

use crate::game::events::{MatchEvents, events};
use crate::output::MatchStatus;
use crate::timeline::align::HistoryMatch;
use crate::timeline::place::Pause;

/// A match this short is a remake: nothing happened worth a request.
const MIN_MATCH_S: u32 = 180;
/// The standard mode; others, such as Street Brawl, restart the clock every round.
const STANDARD_MODE: u32 = 1;
/// A rate limit longer than the crate's retry policy waits stops further requests: every
/// one would be refused until it ends.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) enum Fetched {
    Ready(Box<MatchTiming>),
    Status(MatchStatus),
}

#[derive(Debug)]
pub(crate) struct MatchTiming {
    pub duration_s: f64,
    pub pauses: Vec<Pause>,
    pub events: MatchEvents,
}

/// Whether the match's metadata is in the crate's file cache under `out`.
pub(crate) fn is_cached(out: &Path, match_id: u64) -> bool {
    out.join("matches")
        .join(match_id.to_string())
        .join("metadata.json")
        .is_file()
}

pub(crate) struct Fetcher<'a> {
    api: &'a Client,
    account: u32,
    /// Set by a long rate limit.
    stopped: Option<Stopped>,
}

/// The `Retry-After` every uncached match reports once a long rate limit stopped requests.
#[derive(Debug, Clone, Copy)]
struct Stopped {
    retry_after_s: Option<u64>,
}

/// Why `h` is not worth a request, or `None` for a full match of the standard mode.
pub(crate) fn skip_status(h: &HistoryMatch) -> Option<MatchStatus> {
    if h.duration_s < MIN_MATCH_S {
        Some(MatchStatus::Remake)
    } else if h.game_mode != Some(STANDARD_MODE) {
        Some(MatchStatus::UnsupportedMode {
            game_mode: h.game_mode,
        })
    } else {
        None
    }
}

impl<'a> Fetcher<'a> {
    pub(crate) fn new(api: &'a Client, account: u32) -> Fetcher<'a> {
        Fetcher {
            api,
            account,
            stopped: None,
        }
    }

    /// `cached` says whether the metadata is on disk, so a stopped fetcher can still read it.
    pub(crate) fn fetch(&mut self, h: &HistoryMatch, cached: bool) -> Fetched {
        if let Some(status) = skip_status(h) {
            return Fetched::Status(status);
        }
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
        Fetched::Ready(Box::new(MatchTiming {
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
            events: events(&info, me, &names),
        }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
        let paths = paths.then(|| vec![path(1, &[0; 10], &[100; 10])]);
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

    fn setup() -> (
        tempfile::TempDir,
        Arc<FakeApi>,
        deadlock_api::blocking::Client,
    ) {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(d.path().join("v.mp4"), 7, "http://127.0.0.1:9/v1", "m");
        cfg.out = d.path().to_path_buf();
        cfg.deadlock_api_url = "http://fake.test".into();
        let api = FakeApi::with(&[]);
        let client = crate::game::api::client(&cfg, Some(Box::new(api.clone()))).unwrap();
        (d, api, client)
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
    fn skip_status_passes_only_a_full_standard_match() {
        assert_eq!(skip_status(&hist(1, 180, Some(1))), None);
        assert_eq!(
            skip_status(&hist(1, 179, Some(1))),
            Some(MatchStatus::Remake)
        );
        assert_eq!(
            skip_status(&hist(1, 1800, Some(4))),
            Some(MatchStatus::UnsupportedMode { game_mode: Some(4) })
        );
    }

    #[test]
    fn remake_and_unsupported_mode_send_no_request() {
        let (_d, api, client) = setup();
        let mut f = Fetcher::new(&client, 7);
        assert!(matches!(
            f.fetch(&hist(1, 179, Some(1)), false),
            Fetched::Status(MatchStatus::Remake)
        ));
        assert!(matches!(
            f.fetch(&hist(2, 1800, Some(4)), false),
            Fetched::Status(MatchStatus::UnsupportedMode { game_mode: Some(4) })
        ));
        assert!(matches!(
            f.fetch(&hist(3, 1800, None), false),
            Fetched::Status(MatchStatus::UnsupportedMode { game_mode: None })
        ));
        assert_eq!(api.requests(), Vec::<String>::new());
    }

    #[test]
    fn placed_ready() {
        let (d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(7, true), None);
        names_ok(&api);
        let Fetched::Ready(t) = Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), false)
        else {
            panic!("not ready")
        };
        assert!((t.duration_s - 1800.0).abs() < 1e-9);
        assert_eq!(
            t.pauses,
            [Pause {
                game_time_s: 50.0,
                duration_s: 20.0
            }]
        );
        assert!(t.events.has_paths);
        assert_eq!(t.events.kills[0].victim, "Me");
        assert_eq!(t.events.kills[0].killer, "Foe");
        assert!(is_cached(d.path(), 1));
        assert!(!is_cached(d.path(), 2));
    }

    #[test]
    fn not_yours() {
        let (_d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(9, true), None);
        assert!(matches!(
            Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), false),
            Fetched::Status(MatchStatus::NotYours)
        ));
        assert_eq!(api.requests(), ["/v1/matches/1/metadata"]);
    }

    #[test]
    fn not_ingested_on_404() {
        let (_d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 404, "{}", None);
        assert!(matches!(
            Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), false),
            Fetched::Status(MatchStatus::NotIngested)
        ));
    }

    #[test]
    fn decode_failed_names_file() {
        let (d, api, client) = setup();
        std::fs::create_dir_all(d.path().join("matches/1")).unwrap();
        std::fs::write(
            d.path().join("matches/1/metadata.json"),
            r#"{"match_info": {"match_id": "x"}}"#,
        )
        .unwrap();
        let Fetched::Status(MatchStatus::DecodeFailed { message }) =
            Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), true)
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
        let (d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 429, "{}", Some(3600));
        std::fs::create_dir_all(d.path().join("matches/4")).unwrap();
        std::fs::write(d.path().join("matches/4/metadata.json"), metadata(7, true)).unwrap();
        let mut f = Fetcher::new(&client, 7);
        for id in 1..=3 {
            assert!(
                matches!(
                    f.fetch(&hist(id, 1800, Some(1)), is_cached(d.path(), id)),
                    Fetched::Status(MatchStatus::Unavailable {
                        retry_after_s: Some(3600)
                    })
                ),
                "match {id}"
            );
        }
        assert!(matches!(
            f.fetch(&hist(4, 1800, Some(1)), is_cached(d.path(), 4)),
            Fetched::Ready(_)
        ));
        assert_eq!(api.requests(), ["/v1/matches/1/metadata"]);
    }

    #[test]
    fn short_429_does_not_stop() {
        let (_d, api, client) = setup();
        // Three 429s of 1 s: the crate retries, then gives up; the next match is still tried.
        for _ in 0..3 {
            api.push("/v1/matches/1/metadata", 429, "{}", Some(0));
        }
        api.push("/v1/matches/2/metadata", 404, "{}", None);
        let mut f = Fetcher::new(&client, 7);
        assert!(matches!(
            f.fetch(&hist(1, 1800, Some(1)), false),
            Fetched::Status(MatchStatus::Unavailable {
                retry_after_s: Some(0)
            })
        ));
        assert!(matches!(
            f.fetch(&hist(2, 1800, Some(1)), false),
            Fetched::Status(MatchStatus::NotIngested)
        ));
    }

    #[test]
    fn server_error_is_unavailable() {
        let (_d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 500, "{}", None);
        assert!(matches!(
            Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), false),
            Fetched::Status(MatchStatus::Unavailable {
                retry_after_s: None
            })
        ));
    }

    #[test]
    fn names_failure_falls_back_to_ids() {
        let (_d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(7, true), None);
        api.push("/v1/players/steam", 500, "{}", None);
        let Fetched::Ready(t) = Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), false)
        else {
            panic!("not ready")
        };
        assert_eq!(t.events.kills[0].victim, "7");
        assert_eq!(t.events.kills[0].killer, "8");
    }

    #[test]
    fn no_paths_is_ready_with_has_paths_false() {
        let (_d, api, client) = setup();
        api.push("/v1/matches/1/metadata", 200, &metadata(7, false), None);
        names_ok(&api);
        let Fetched::Ready(t) = Fetcher::new(&client, 7).fetch(&hist(1, 1800, Some(1)), false)
        else {
            panic!("not ready")
        };
        assert!(!t.events.has_paths);
    }
}
