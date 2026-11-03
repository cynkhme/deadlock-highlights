//! Events on the VOD timeline, grouped into windows and ranked by the streamer's own actions.

use crate::game::events::MatchEvents;
use crate::output::{Event, Highlight};
use crate::timeline::place::{Pause, elapsed};

/// Events at most this far apart form one window, s.
const GAP_S: f64 = 12.0;
const LEAD_IN_S: f64 = 10.0;
const TAIL_S: f64 = 5.0;
/// The streamer's combat seconds move a window's edges (never join windows): the start to
/// this long before the first combat second, at most `MAX_LEAD_S` before the first event; the
/// end past the last combat second, at most `MAX_TAIL_S` after the last event, s.
const COMBAT_LEAD_S: f64 = 3.0;
const MAX_LEAD_S: f64 = 20.0;
const MAX_TAIL_S: f64 = 30.0;
/// A window's points per own kill and per assist of the streamer; other events score 0.
pub(crate) const OWN_KILL_POINTS: u32 = 2;
pub(crate) const ASSIST_POINTS: u32 = 1;

/// The match's scoring events on the VOD, in VOD order: kills near the streamer and
/// objectives, up to the end of the streamer's stream of the match and inside the VOD's
/// `vod_duration`.
pub(crate) fn vod_events(
    ev: &MatchEvents,
    vod_start: f64,
    pauses: &[Pause],
    duration_s: f64,
    vod_duration: f64,
) -> Vec<Event> {
    let stream_end = ev.left_at.unwrap_or(duration_s);
    let at = |game_time: f64| vod_start + elapsed(game_time, pauses);
    let kills = ev.kills.iter().filter(|k| k.scored).map(|k| Event {
        vod_time: at(k.game_time),
        game_time: k.game_time,
        label: format!("{} > {}", k.killer, k.victim),
        my_kill: k.my_kill,
        my_death: k.my_death,
        my_assist: k.my_assist,
        objective: false,
        my_health: k.my_health,
        net_worth_ratio: k.net_worth_ratio,
    });
    let objectives = ev.objectives.iter().map(|o| Event {
        vod_time: at(o.game_time),
        game_time: o.game_time,
        label: format!("{} ({})", o.name, if o.by_us { "us" } else { "them" }),
        my_kill: false,
        my_death: false,
        my_assist: false,
        objective: true,
        my_health: None,
        net_worth_ratio: None,
    });
    let mut events: Vec<Event> = kills
        .chain(objectives)
        .filter(|e| e.game_time <= stream_end && (0.0..=vod_duration).contains(&e.vod_time))
        .collect();
    events.sort_by(|a, b| a.vod_time.total_cmp(&b.vod_time));
    events
}

/// The streamer's combat seconds on the VOD, ascending, up to the end of the streamer's stream
/// of the match and inside the VOD's `vod_duration`.
pub(crate) fn vod_combat(
    ev: &MatchEvents,
    vod_start: f64,
    pauses: &[Pause],
    duration_s: f64,
    vod_duration: f64,
) -> Vec<f64> {
    let stream_end = ev.left_at.unwrap_or(duration_s);
    ev.combat
        .iter()
        .filter(|&&t| t <= stream_end)
        .map(|&t| vod_start + elapsed(t, pauses))
        .filter(|v| (0.0..=vod_duration).contains(v))
        .collect()
}

/// The match's last second on the VOD, when the streamer stayed to the end.
pub(crate) fn match_end(
    ev: &MatchEvents,
    vod_start: f64,
    pauses: &[Pause],
    duration_s: f64,
) -> Option<f64> {
    ev.left_at
        .is_none()
        .then(|| vod_start + elapsed(duration_s, pauses))
}

/// The windows of `events`, which are in VOD order: runs at most `GAP_S` apart, their edges
/// moved by the ascending `combat` seconds and, near it, the `match_end`.
pub(crate) fn windows(
    match_id: u64,
    events: &[Event],
    combat: &[f64],
    match_end: Option<f64>,
    vod_duration: f64,
) -> Vec<Highlight> {
    events
        .chunk_by(|a, b| b.vod_time - a.vod_time <= GAP_S)
        .map(|g| {
            let count = |f: fn(&Event) -> bool| g.iter().filter(|e| f(e)).count() as u32;
            let deaths = count(|e| !e.objective);
            let my_kills = count(|e| e.my_kill);
            let my_assists = count(|e| e.my_assist);
            let objectives = count(|e| e.objective);
            let (first, last) = (g[0].vod_time, g[g.len() - 1].vod_time);
            let mut last_c = last;
            for &c in combat.iter().filter(|&&c| c > last) {
                if c - last_c > GAP_S || c > last + MAX_TAIL_S {
                    break;
                }
                last_c = c;
            }
            let mut first_c = first;
            for &c in combat.iter().rev().filter(|&&c| c < first) {
                if first_c - c > GAP_S || c < first - MAX_LEAD_S {
                    break;
                }
                first_c = c;
            }
            let start = (first - LEAD_IN_S)
                .min(first_c - COMBAT_LEAD_S)
                .max(first - MAX_LEAD_S)
                .max(0.0);
            let mut end = last.max(last_c) + TAIL_S;
            // The fight that ends the match runs to 5 s after its last second (the patron's fall).
            if let Some(m) = match_end.filter(|&m| m >= last && m - end <= GAP_S) {
                end = end.max((m + TAIL_S).min(last + MAX_TAIL_S + TAIL_S));
            }
            let end = end.min(vod_duration);
            let score = OWN_KILL_POINTS * my_kills + ASSIST_POINTS * my_assists;
            Highlight {
                match_id,
                start,
                end,
                score,
                combat_s: combat
                    .iter()
                    .filter(|&&c| (start..=end).contains(&c))
                    .count() as u32,
                deaths,
                my_kills,
                my_assists,
                my_deaths: count(|e| e.my_death),
                objectives,
                events: g.to_vec(),
            }
        })
        .collect()
}

/// Best first: by score, then by combat seconds, then earlier.
pub(crate) fn rank(all: &mut [Highlight]) {
    all.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.combat_s.cmp(&a.combat_s))
            .then(a.start.total_cmp(&b.start))
    });
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "window bounds are sums of whole seconds")]
mod tests {
    use super::*;
    use crate::game::events::{Kill, Objective};

    fn ev(vod_time: f64, my_kill: bool, my_assist: bool) -> Event {
        Event {
            vod_time,
            game_time: vod_time,
            label: String::new(),
            my_kill,
            my_death: false,
            my_assist,
            objective: false,
            my_health: None,
            net_worth_ratio: None,
        }
    }

    fn kill(game_time: f64, scored: bool) -> Kill {
        Kill {
            game_time,
            killer: "a".into(),
            victim: "b".into(),
            my_kill: false,
            my_death: false,
            my_assist: false,
            scored,
            my_health: None,
            net_worth_ratio: None,
        }
    }

    #[test]
    fn score_counts_own_actions_only() {
        let mut own_death = ev(103.0, false, false);
        own_death.my_death = true;
        let mut objective = ev(104.0, false, false);
        objective.objective = true;
        let e = [
            ev(100.0, true, false),
            ev(101.0, false, true),
            ev(102.0, false, false),
            own_death,
            objective,
        ];
        let w = windows(1, &e, &[], None, 10_000.0);
        assert_eq!((w.len(), w[0].score), (1, 3));
    }

    #[test]
    fn chain_of_zero_point_events_scores_0() {
        let mut objective = ev(101.0, false, false);
        objective.objective = true;
        let w = windows(
            1,
            &[ev(100.0, false, false), objective],
            &[],
            None,
            10_000.0,
        );
        assert_eq!((w.len(), w[0].score), (1, 0));
    }

    #[test]
    fn gap_of_12_joins_13_splits() {
        assert_eq!(
            windows(
                1,
                &[ev(100.0, false, false), ev(112.0, false, false)],
                &[],
                None,
                1e4
            )
            .len(),
            1
        );
        assert_eq!(
            windows(
                1,
                &[ev(100.0, false, false), ev(113.0, false, false)],
                &[],
                None,
                1e4
            )
            .len(),
            2
        );
    }

    #[test]
    fn window_is_clamped_to_vod() {
        let w = windows(1, &[ev(2.0, false, false)], &[], None, 1000.0);
        assert_eq!(w[0].start, 0.0);
        let w = windows(1, &[ev(999.0, false, false)], &[], None, 1000.0);
        assert_eq!(w[0].end, 1000.0);
    }

    #[test]
    fn far_death_excluded_objective_kept() {
        let m = MatchEvents {
            kills: vec![kill(10.0, true), kill(20.0, false)],
            objectives: vec![Objective {
                game_time: 30.0,
                name: "objective 7".into(),
                by_us: false,
            }],
            left_at: None,
            has_paths: true,
            combat: Vec::new(),
        };
        let t = vod_events(&m, 1000.0, &[], 2000.0, 1e4);
        let got: Vec<(f64, bool, &str)> = t
            .iter()
            .map(|e| (e.vod_time, e.objective, e.label.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (1010.0, false, "a > b"),
                (1030.0, true, "objective 7 (them)")
            ]
        );
    }

    #[test]
    fn timeline_is_pause_aware_and_drops_before_vod() {
        let m = MatchEvents {
            kills: vec![kill(10.0, true), kill(700.0, true)],
            objectives: vec![],
            left_at: None,
            has_paths: true,
            combat: Vec::new(),
        };
        let pauses = [Pause {
            game_time_s: 600.0,
            duration_s: 30.0,
        }];
        let t = vod_events(&m, -20.0, &pauses, 2000.0, 1e4);
        assert_eq!(
            t.iter()
                .map(|e| (e.vod_time, e.game_time))
                .collect::<Vec<_>>(),
            [(710.0, 700.0)]
        );
    }

    #[test]
    fn events_after_leave_dropped() {
        let m = MatchEvents {
            kills: vec![kill(10.0, true), kill(500.0, true), kill(501.0, true)],
            objectives: vec![Objective {
                game_time: 900.0,
                name: "mid boss".into(),
                by_us: true,
            }],
            left_at: Some(500.0),
            has_paths: true,
            combat: Vec::new(),
        };
        let t = vod_events(&m, 0.0, &[], 2000.0, 1e4);
        assert_eq!(
            t.iter().map(|e| e.game_time).collect::<Vec<_>>(),
            [10.0, 500.0]
        );
    }

    #[test]
    fn events_after_match_end_dropped() {
        let m = MatchEvents {
            kills: vec![kill(10.0, true), kill(2001.0, true)],
            objectives: vec![],
            left_at: None,
            has_paths: true,
            combat: Vec::new(),
        };
        assert_eq!(vod_events(&m, 0.0, &[], 2000.0, 1e4).len(), 1);
    }

    fn hl(score: u32, combat_s: u32, start: f64) -> Highlight {
        Highlight {
            match_id: 1,
            start,
            end: start + 10.0,
            score,
            combat_s,
            deaths: 0,
            my_kills: 0,
            my_assists: 0,
            my_deaths: 0,
            objectives: 0,
            events: Vec::new(),
        }
    }

    #[test]
    fn rank_by_score_then_combat_then_start() {
        let mut all = [
            hl(3, 0, 200.0),
            hl(3, 10, 300.0),
            hl(5, 0, 400.0),
            hl(3, 10, 100.0),
        ];
        rank(&mut all);
        assert_eq!(
            all.iter().map(|h| h.start).collect::<Vec<_>>(),
            [400.0, 100.0, 300.0, 200.0]
        );
    }

    #[test]
    fn vod_events_carry_recorded_fields() {
        let mut k = kill(100.0, true);
        k.my_kill = true;
        k.my_health = Some(40);
        k.net_worth_ratio = Some(1.2);
        let m = MatchEvents {
            kills: vec![k],
            objectives: vec![Objective {
                game_time: 200.0,
                name: "objective 1".into(),
                by_us: true,
            }],
            left_at: None,
            has_paths: true,
            combat: Vec::new(),
        };
        let t = vod_events(&m, 0.0, &[], 2000.0, 1e4);
        assert_eq!(
            (t[0].my_health, t[0].net_worth_ratio),
            (Some(40), Some(1.2))
        );
        assert_eq!((t[1].my_health, t[1].net_worth_ratio), (None, None));
    }

    /// Whole VOD seconds `a..=b`.
    fn secs(a: u32, b: u32) -> Vec<f64> {
        (a..=b).map(f64::from).collect()
    }

    /// The one window of an event at VOD 100 with `combat`.
    fn one(combat: &[f64], match_end: Option<f64>) -> Highlight {
        let w = windows(1, &[ev(100.0, true, false)], combat, match_end, 1e4);
        assert_eq!(w.len(), 1);
        w.into_iter().next().unwrap()
    }

    #[test]
    fn combat_extends_the_tail() {
        assert_eq!(one(&secs(101, 117), None).end, 122.0);
    }

    #[test]
    fn combat_gap_over_12_stops_the_tail() {
        let mut c = secs(101, 105);
        c.push(118.0);
        assert_eq!(one(&c, None).end, 110.0);
    }

    #[test]
    fn max_tail_caps_the_tail() {
        assert_eq!(one(&secs(101, 200), None).end, 135.0);
    }

    #[test]
    fn combat_extends_the_lead() {
        assert_eq!(one(&secs(85, 99), None).start, 82.0);
    }

    #[test]
    fn max_lead_caps_the_lead() {
        assert_eq!(one(&secs(50, 99), None).start, 80.0);
    }

    #[test]
    fn no_combat_keeps_todays_edges() {
        let w = one(&[], None);
        assert_eq!((w.start, w.end, w.combat_s), (90.0, 105.0, 0));
    }

    #[test]
    fn combat_never_joins_chains() {
        let e = [ev(100.0, true, false), ev(113.0, true, false)];
        let w = windows(1, &e, &secs(100, 113), None, 1e4);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].end, w[1].start), (118.0, 97.0));
    }

    #[test]
    fn match_end_extends_to_the_patron() {
        assert_eq!(one(&secs(101, 108), Some(119.0)).end, 124.0);
        assert_eq!(one(&secs(101, 108), Some(150.0)).end, 113.0);
    }

    #[test]
    fn without_a_match_end_the_window_ends_with_the_fight() {
        assert_eq!(one(&secs(101, 108), None).end, 113.0);
    }

    #[test]
    fn match_end_none_after_leave() {
        let mut m = MatchEvents {
            kills: vec![],
            objectives: vec![],
            left_at: Some(500.0),
            has_paths: true,
            combat: Vec::new(),
        };
        assert_eq!(match_end(&m, 0.0, &[], 999.0), None);
        m.left_at = None;
        assert_eq!(match_end(&m, 0.0, &[], 999.0), Some(999.0));
        let pauses = [Pause {
            game_time_s: 600.0,
            duration_s: 30.0,
        }];
        assert_eq!(match_end(&m, 0.0, &pauses, 999.0), Some(1029.0));
    }

    #[test]
    fn combat_lead_clamps_at_vod_zero() {
        let w = windows(1, &[ev(5.0, true, false)], &secs(0, 4), None, 1e4);
        assert_eq!(w[0].start, 0.0);
    }

    #[test]
    fn combat_s_counts_seconds_inside() {
        let mut c = secs(101, 117);
        c.push(300.0);
        assert_eq!(one(&c, None).combat_s, 17);
    }

    #[test]
    fn combat_seconds_map_through_pauses() {
        let m = MatchEvents {
            kills: vec![kill(10.0, true), kill(700.0, true)],
            objectives: vec![],
            left_at: None,
            has_paths: true,
            combat: vec![10.0, 700.0],
        };
        let pauses = [Pause {
            game_time_s: 600.0,
            duration_s: 30.0,
        }];
        let events: Vec<f64> = vod_events(&m, -20.0, &pauses, 2000.0, 1e4)
            .iter()
            .map(|e| e.vod_time)
            .collect();
        assert_eq!(vod_combat(&m, -20.0, &pauses, 2000.0, 1e4), events);
        assert_eq!(events, [710.0]);
    }

    #[test]
    fn events_and_combat_after_the_vod_end_dropped() {
        let m = MatchEvents {
            kills: vec![kill(100.0, true), kill(600.0, true), kill(601.0, true)],
            objectives: vec![],
            left_at: None,
            has_paths: true,
            combat: vec![100.0, 600.0, 601.0],
        };
        let events: Vec<f64> = vod_events(&m, 0.0, &[], 2000.0, 600.0)
            .iter()
            .map(|e| e.vod_time)
            .collect();
        assert_eq!(events, [100.0, 600.0]);
        assert_eq!(vod_combat(&m, 0.0, &[], 2000.0, 600.0), [100.0, 600.0]);
    }
}
