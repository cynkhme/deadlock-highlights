//! Events on the VOD timeline, grouped into windows and ranked by density.

use crate::game::events::MatchEvents;
use crate::output::{Event, Highlight};
use crate::timeline::place::{Pause, elapsed};

/// Events at most this far apart form one window, s.
const GAP_S: f64 = 12.0;
const LEAD_IN_S: f64 = 10.0;
const TAIL_S: f64 = 5.0;
/// Added to a window's span in the density, so a lone event does not outrank a
/// fight, s.
const DENSITY_PAD_S: f64 = 10.0;
/// Points of a window, per event of each kind; the streamer's own kill also counts as a
/// death.
const DEATH_POINTS: u32 = 1;
const OWN_KILL_POINTS: u32 = 2;
const ASSIST_POINTS: u32 = 1;
const OBJECTIVE_POINTS: u32 = 1;
/// The density of a window holding only one kill by the streamer.
pub(crate) const LONE_OWN_KILL_DENSITY: f64 =
    (DEATH_POINTS + OWN_KILL_POINTS) as f64 / DENSITY_PAD_S;

/// The match's scoring events on the VOD, in VOD order: kills near the streamer and
/// objectives, up to the end of the streamer's stream of the match.
pub(crate) fn vod_events(
    ev: &MatchEvents,
    vod_start: f64,
    pauses: &[Pause],
    duration_s: f64,
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
    });
    let objectives = ev.objectives.iter().map(|o| Event {
        vod_time: at(o.game_time),
        game_time: o.game_time,
        label: format!("{} ({})", o.name, if o.by_us { "us" } else { "them" }),
        my_kill: false,
        my_death: false,
        my_assist: false,
        objective: true,
    });
    let mut events: Vec<Event> = kills
        .chain(objectives)
        .filter(|e| e.game_time <= stream_end && e.vod_time >= 0.0)
        .collect();
    events.sort_by(|a, b| a.vod_time.total_cmp(&b.vod_time));
    events
}

/// The windows of `events`, which are in VOD order: runs at most `GAP_S` apart.
pub(crate) fn windows(match_id: u64, events: &[Event], vod_duration: f64) -> Vec<Highlight> {
    events
        .chunk_by(|a, b| b.vod_time - a.vod_time <= GAP_S)
        .map(|g| {
            let count = |f: fn(&Event) -> bool| g.iter().filter(|e| f(e)).count() as u32;
            let deaths = count(|e| !e.objective);
            let my_kills = count(|e| e.my_kill);
            let my_assists = count(|e| e.my_assist);
            let objectives = count(|e| e.objective);
            let (first, last) = (g[0].vod_time, g[g.len() - 1].vod_time);
            let score = DEATH_POINTS * deaths
                + OWN_KILL_POINTS * my_kills
                + ASSIST_POINTS * my_assists
                + OBJECTIVE_POINTS * objectives;
            Highlight {
                match_id,
                start: (first - LEAD_IN_S).max(0.0),
                end: (last + TAIL_S).min(vod_duration),
                score,
                density: f64::from(score) / (last - first + DENSITY_PAD_S),
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

/// Best first: by density, then by score, then earlier.
pub(crate) fn rank(all: &mut [Highlight]) {
    all.sort_by(|a, b| {
        b.density
            .total_cmp(&a.density)
            .then(b.score.cmp(&a.score))
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
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn lone_own_kill_density_030() {
        let w = windows(1, &[ev(100.0, true, false)], 10_000.0);
        assert_eq!((w[0].score, w[0].deaths, w[0].my_kills), (3, 1, 1));
        assert!(close(w[0].density, 0.30));
        assert_eq!((w[0].start, w[0].end), (90.0, 105.0));
    }

    #[test]
    fn three_own_kills_in_5s_060() {
        let e = [
            ev(100.0, true, false),
            ev(102.5, true, false),
            ev(105.0, true, false),
        ];
        let w = windows(1, &e, 10_000.0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].score, 9);
        assert!(close(w[0].density, 0.60));
    }

    #[test]
    fn brawl_60s_one_own_kill_014() {
        let mut e: Vec<Event> = (0..8)
            .map(|i| ev(100.0 + f64::from(i) * 60.0 / 7.0, false, false))
            .collect();
        e[3].my_kill = true;
        let w = windows(1, &e, 10_000.0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].score, 10);
        assert!(close(w[0].density, 10.0 / 70.0));
    }

    #[test]
    #[allow(clippy::float_cmp, reason = "the same arithmetic on the same values")]
    fn lone_own_kill_has_the_named_density() {
        let w = windows(1, &[ev(100.0, true, false)], 3600.0);
        assert_eq!(w[0].density, LONE_OWN_KILL_DENSITY);
    }

    #[test]
    fn assist_and_objective_weights() {
        let mut o = ev(101.0, false, false);
        o.objective = true;
        let w = windows(1, &[ev(100.0, false, true), o], 10_000.0);
        assert_eq!(
            (w[0].score, w[0].my_assists, w[0].objectives, w[0].deaths),
            (3, 1, 1, 1)
        );
    }

    #[test]
    fn gap_of_12_joins_13_splits() {
        assert_eq!(
            windows(1, &[ev(100.0, false, false), ev(112.0, false, false)], 1e4).len(),
            1
        );
        assert_eq!(
            windows(1, &[ev(100.0, false, false), ev(113.0, false, false)], 1e4).len(),
            2
        );
    }

    #[test]
    fn window_is_clamped_to_vod() {
        let w = windows(1, &[ev(2.0, false, false)], 1000.0);
        assert_eq!(w[0].start, 0.0);
        let w = windows(1, &[ev(999.0, false, false)], 1000.0);
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
        };
        let t = vod_events(&m, 1000.0, &[], 2000.0);
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
        };
        let pauses = [Pause {
            game_time_s: 600.0,
            duration_s: 30.0,
        }];
        let t = vod_events(&m, -20.0, &pauses, 2000.0);
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
        };
        let t = vod_events(&m, 0.0, &[], 2000.0);
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
        };
        assert_eq!(vod_events(&m, 0.0, &[], 2000.0).len(), 1);
    }

    #[test]
    fn rank_by_density_then_score() {
        let mut all = windows(1, &[ev(100.0, true, false)], 1e4); // 0.30, score 3
        all.extend(windows(
            2,
            &[
                ev(500.0, true, false),
                ev(502.5, true, false),
                ev(505.0, true, false),
            ],
            1e4,
        )); // 0.60
        all.extend(windows(
            3,
            &[ev(900.0, false, false), ev(910.0, true, false)],
            1e4,
        )); // 4 / 20 = 0.20
        all.extend(windows(
            4,
            &[ev(1300.0, true, false), ev(1300.0, true, false)],
            1e4,
        )); // 6 / 10 = 0.60, score 6
        rank(&mut all);
        assert_eq!(
            all.iter().map(|h| h.match_id).collect::<Vec<_>>(),
            [2, 4, 1, 3]
        );
    }
}
