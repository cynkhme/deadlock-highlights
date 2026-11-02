//! Builders shared by the unit tests.

use deadlock_api::MatchInfo;
use serde_json::{Value, json};

/// One path sample per second: x position (y = 0), health and combat type. An empty `combat`
/// leaves the key out, as some matches do.
pub(crate) fn path(slot: u32, xs: &[u32], health: &[u32], combat: &[u32]) -> Value {
    assert_eq!(xs.len(), health.len());
    let mut p = json!({
        "player_slot": slot, "x_min": 0.0, "y_min": 0.0, "x_max": 10000.0, "y_max": 10000.0,
        "x_pos": xs, "y_pos": vec![0; xs.len()], "health": health,
    });
    if !combat.is_empty() {
        assert_eq!(combat.len(), xs.len());
        p["combat_type"] = json!(combat);
    }
    p
}

/// A player; `deaths` are (game time, killer slot, death x).
pub(crate) fn player(
    account: u32,
    slot: u32,
    team: u32,
    deaths: &[(u32, Option<u32>, Option<f64>)],
) -> Value {
    let d: Vec<Value> = deaths
        .iter()
        .map(|&(t, killer, x)| {
            json!({
                "game_time_s": t,
                "killer_player_slot": killer,
                "death_pos": x.map(|x| json!({"x": x, "y": 0.0, "z": 0.0})),
            })
        })
        .collect();
    json!({"account_id": account, "player_slot": slot, "team": team, "death_details": d})
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "callers build the lists inline"
)]
pub(crate) fn match_info(
    duration_s: u32,
    players: Vec<Value>,
    paths: Option<Vec<Value>>,
    extra: &Value,
) -> MatchInfo {
    serde_json::from_value(match_info_json(duration_s, players, paths, extra))
        .expect("a valid MatchInfo")
}

/// The JSON of [`match_info`], for responses of the fake API.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn match_info_json(
    duration_s: u32,
    players: Vec<Value>,
    paths: Option<Vec<Value>>,
    extra: &Value,
) -> Value {
    let mut v = json!({"match_id": 42, "duration_s": duration_s, "players": players});
    if let Some(p) = paths {
        v["match_paths"] = json!({"x_resolution": 10000.0, "y_resolution": 10000.0, "paths": p});
    }
    for (k, x) in extra.as_object().into_iter().flatten() {
        v[k] = x.clone();
    }
    v
}

/// A scripted deadlock-api transport: responses are taken in order per path prefix, every
/// request is logged, and an unscripted or exhausted path panics.
/// Per path prefix, the (status, body, Retry-After) responses still to give.
type Script = Vec<(
    String,
    std::collections::VecDeque<(u16, String, Option<u64>)>,
)>;

#[derive(Default)]
pub(crate) struct FakeApi {
    script: std::sync::Mutex<Script>,
    pub log: std::sync::Mutex<Vec<String>>,
}

impl FakeApi {
    pub(crate) fn with(script: &[(&str, &[(u16, &str)])]) -> std::sync::Arc<FakeApi> {
        let script = script
            .iter()
            .map(|(p, rs)| {
                let rs = rs
                    .iter()
                    .map(|(s, b)| (*s, (*b).to_owned(), None))
                    .collect();
                ((*p).to_owned(), rs)
            })
            .collect();
        std::sync::Arc::new(FakeApi {
            script: std::sync::Mutex::new(script),
            log: std::sync::Mutex::default(),
        })
    }

    /// Appends a response for `prefix`, after those already scripted for it.
    pub(crate) fn push(&self, prefix: &str, status: u16, body: &str, retry_after: Option<u64>) {
        let mut script = self.script.lock().unwrap();
        let r = (status, body.to_owned(), retry_after);
        match script.iter_mut().find(|(p, _)| p == prefix) {
            Some((_, rs)) => rs.push_back(r),
            None => script.push((prefix.to_owned(), [r].into())),
        }
    }

    pub(crate) fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl deadlock_api::blocking::Transport for FakeApi {
    fn send(
        &self,
        request: http::Request<()>,
    ) -> Result<http::Response<bytes::Bytes>, deadlock_api::BoxError> {
        let path = request.uri().path().to_owned();
        self.log.lock().unwrap().push(path.clone());
        let mut script = self.script.lock().unwrap();
        let (status, body, retry_after) = script
            .iter_mut()
            .find(|(p, _)| path.starts_with(p.as_str()))
            .and_then(|(_, rs)| rs.pop_front())
            .unwrap_or_else(|| panic!("unscripted request {path}"));
        let mut b = http::Response::builder()
            .status(status)
            .header("content-type", "application/json");
        if let Some(s) = retry_after {
            b = b.header("retry-after", s.to_string());
        }
        Ok(b.body(bytes::Bytes::from(body)).unwrap())
    }
}
