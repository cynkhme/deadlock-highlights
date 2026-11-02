//! What happened in a match, from the streamer's point of view.

use std::collections::HashMap;

use deadlock_api::{AccountId, MatchInfo, Player};

use crate::game::track::Track;

/// A player death, from the streamer's point of view.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one death"
)]
pub(crate) struct Kill {
    pub game_time: f64,
    pub killer: String,
    pub victim: String,
    #[expect(clippy::struct_field_names, reason = "my kill is the domain term")]
    pub my_kill: bool,
    pub my_death: bool,
    pub my_assist: bool,
    /// Counts for the score: the streamer's own kill or death, or a death near the streamer or
    /// at a distance that cannot be judged.
    pub scored: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Objective {
    pub game_time: f64,
    pub name: String,
    pub by_us: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MatchEvents {
    pub kills: Vec<Kill>,
    pub objectives: Vec<Objective>,
    /// Set when the streamer left well before the end: later events are not on the stream.
    pub left_at: Option<f64>,
    pub has_paths: bool,
}

/// What happened in the match on `me`'s side; `me` is one of `info.players`.
pub(crate) fn events(
    info: &MatchInfo,
    me: &Player,
    names: &HashMap<AccountId, String>,
) -> MatchEvents {
    let my_team = me.team;
    let by_slot: HashMap<u32, &Player> = info.players.iter().map(|p| (p.player_slot, p)).collect();
    let name = |p: &Player| {
        names
            .get(&p.account_id)
            .cloned()
            .unwrap_or_else(|| p.account_id.to_string())
    };
    let track = Track::new(info, me);

    let mut kills = Vec::new();
    for victim in &info.players {
        for d in &victim.death_details {
            // A death without a time cannot be placed on the VOD.
            let Some(t) = d.game_time_s else { continue };
            let game_time = f64::from(t);
            let killer = d.killer_player_slot.and_then(|s| by_slot.get(&s).copied());
            let my_kill = killer.is_some_and(|k| k.account_id == me.account_id);
            let my_death = victim.account_id == me.account_id;
            let ally_kill = killer.is_some_and(|k| {
                my_team.is_some() && k.team == my_team && k.account_id != me.account_id
            });
            let close = track
                .as_ref()
                .zip(d.death_pos.as_ref())
                .map(|(tr, pos)| tr.near(game_time, pos));
            kills.push(Kill {
                game_time,
                killer: killer.map_or_else(|| "(non-player)".into(), name),
                victim: name(victim),
                my_kill,
                my_death,
                my_assist: ally_kill && close == Some(true),
                // Without a path or a death position proximity cannot be judged, so the death counts.
                scored: my_kill || my_death || close != Some(false),
            });
        }
    }
    kills.sort_by(|a, b| a.game_time.total_cmp(&b.game_time));

    let mut objectives: Vec<Objective> = info
        .objectives
        .iter()
        .filter_map(|o| {
            let t = o.destroyed_time_s.filter(|&t| t > 0)?;
            Some(Objective {
                game_time: f64::from(t),
                name: o
                    .team_objective_id
                    .map_or_else(|| "objective".into(), |id| format!("objective {id}")),
                by_us: my_team.is_some() && o.team.is_some() && o.team != my_team,
            })
        })
        .collect();
    objectives.extend(info.mid_boss.iter().filter_map(|b| {
        let t = b.destroyed_time_s.filter(|&t| t > 0)?;
        Some(Objective {
            game_time: f64::from(t),
            name: "mid boss".into(),
            by_us: my_team.is_some() && b.team_killed == my_team,
        })
    }));
    objectives.sort_by(|a, b| a.game_time.total_cmp(&b.game_time));

    let duration = f64::from(info.duration_s);
    // The path has one sample per second while the player is connected, and the abandon
    // field stays 0 when they leave, so a short path is the only sign of leaving.
    let left_at = track
        .as_ref()
        .map(Track::last_game_time)
        .filter(|&t| t < duration - 60.0);
    MatchEvents {
        kills,
        objectives,
        left_at,
        has_paths: track.is_some(),
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "whole-second game times are exact")]
mod tests {
    use super::*;
    use crate::test_support::{match_info, path, player};
    use serde_json::json;

    /// Me: account 1, slot 1, team 0, standing at x = 1000 the whole match. Mate: slot 2,
    /// team 0. Enemies: slots 3 and 4, team 1.
    fn info(
        deaths_of_enemy: &[(u32, Option<u32>, Option<f64>)],
        my_deaths: &[(u32, Option<u32>, Option<f64>)],
    ) -> MatchInfo {
        let n = 1000;
        match_info(
            999,
            vec![
                player(1, 1, 0, my_deaths),
                player(2, 2, 0, &[]),
                player(3, 3, 1, deaths_of_enemy),
                player(4, 4, 1, &[]),
            ],
            Some(vec![path(1, &vec![1000; n], &vec![100; n])]),
            &json!({
                "objectives": [
                    {"team_objective_id": 7, "team": 1, "destroyed_time_s": 300},
                    {"team_objective_id": 8, "team": 0, "destroyed_time_s": 400},
                    {"team_objective_id": 9, "team": 1, "destroyed_time_s": 0}
                ],
                "mid_boss": [{"team_killed": 0, "destroyed_time_s": 500}]
            }),
        )
    }

    fn names() -> HashMap<AccountId, String> {
        HashMap::from([
            (AccountId(1), "me".to_owned()),
            (AccountId(3), "foe".to_owned()),
        ])
    }

    #[test]
    fn assist_needs_teammate_killer_and_near() {
        let m = info(
            &[
                (100, Some(2), Some(1500.0)),
                (200, Some(4), Some(1500.0)),
                (300, Some(1), Some(1500.0)),
            ],
            &[],
        );
        let e = events(&m, &m.players[0], &names());
        let k: Vec<(f64, bool, bool)> = e
            .kills
            .iter()
            .map(|k| (k.game_time, k.my_kill, k.my_assist))
            .collect();
        // 200: killed by its own teammate (slot 4): not an ally kill.
        assert_eq!(
            k,
            [
                (100.0, false, true),
                (200.0, false, false),
                (300.0, true, false)
            ]
        );
        assert_eq!(e.kills[0].killer, "2");
        assert_eq!(e.kills[0].victim, "foe");
        assert_eq!(e.kills[2].killer, "me");
    }

    #[test]
    fn distant_death_not_near() {
        let m = info(&[(100, Some(2), Some(9000.0))], &[]);
        let e = events(&m, &m.players[0], &names());
        assert!(!e.kills[0].scored && !e.kills[0].my_assist);
    }

    #[test]
    fn death_without_pos_kept() {
        let m = info(&[(100, Some(2), None), (120, None, None)], &[]);
        let e = events(&m, &m.players[0], &names());
        assert!(e.kills.iter().all(|k| k.scored && !k.my_assist));
        assert_eq!(e.kills[1].killer, "(non-player)");
    }

    #[test]
    fn own_death_is_near_and_mine() {
        let m = info(&[], &[(150, Some(3), Some(8000.0))]);
        let e = events(&m, &m.players[0], &names());
        assert!(e.kills[0].my_death && e.kills[0].scored);
    }

    #[test]
    fn no_paths_keeps_every_death() {
        let mut m = info(&[(100, Some(2), Some(9000.0))], &[]);
        m.match_paths = None;
        let e = events(&m, &m.players[0], &names());
        assert!(!e.has_paths);
        assert!(e.kills[0].scored && !e.kills[0].my_assist);
        assert_eq!(e.left_at, None);
    }

    #[test]
    fn left_at_set_only_when_path_ends_early() {
        let m = info(&[], &[]);
        // 1000 samples in a 999 s match: a full path.
        assert_eq!(events(&m, &m.players[0], &names()).left_at, None);
        let mut short = m.clone();
        if let Some(p) = short.match_paths.as_mut() {
            let path = &mut p.paths[0];
            path.x_pos.truncate(500);
            path.y_pos.truncate(500);
            path.health.truncate(500);
        }
        assert_eq!(
            events(&short, &short.players[0], &names()).left_at,
            Some(498.0)
        );
    }

    #[test]
    fn objective_by_us_and_mid_boss() {
        let m = info(&[], &[]);
        let o: Vec<(f64, String, bool)> = events(&m, &m.players[0], &names())
            .objectives
            .into_iter()
            .map(|o| (o.game_time, o.name, o.by_us))
            .collect();
        assert_eq!(
            o,
            [
                (300.0, "objective 7".to_owned(), true),
                (400.0, "objective 8".to_owned(), false),
                (500.0, "mid boss".to_owned(), true),
            ]
        );
    }
}
