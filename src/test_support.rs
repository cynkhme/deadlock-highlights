//! Builders shared by the unit tests.

use deadlock_api::MatchInfo;
use serde_json::{Value, json};

/// One path sample per second: x position (y = 0) and health.
pub(crate) fn path(slot: u32, xs: &[u32], health: &[u32]) -> Value {
    assert_eq!(xs.len(), health.len());
    json!({
        "player_slot": slot, "x_min": 0.0, "y_min": 0.0, "x_max": 10000.0, "y_max": 10000.0,
        "x_pos": xs, "y_pos": vec![0; xs.len()], "health": health,
    })
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
    let mut v = json!({"match_id": 42, "duration_s": duration_s, "players": players});
    if let Some(p) = paths {
        v["match_paths"] = json!({"x_resolution": 10000.0, "y_resolution": 10000.0, "paths": p});
    }
    for (k, x) in extra.as_object().into_iter().flatten() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).expect("a valid MatchInfo")
}
