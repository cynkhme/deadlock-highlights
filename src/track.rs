//! A player's drift-corrected path: where they were, and whether alive, at a game time.

use deadlock_api::{MatchInfo, MatchPaths, Player, PlayerPath, Pos};

/// The assist radius and window are calibrated against demo files (README); constants, not options.
pub(crate) const ASSIST_RADIUS: f64 = 2500.0;
pub(crate) const ASSIST_WINDOW_S: i64 = 3;
/// A death keeps health at 0 for at least this many samples; shorter zero runs are blips.
const MIN_DEATH_SAMPLES: usize = 5;

/// A player's position and health over game time.
///
/// Samples sit at index `game_time + 1`, except that lag and reconnects insert extra
/// samples, so the index drifts (by over a minute by the end of some matches). The
/// player's own deaths, where health first hits 0, pin the drift; it is interpolated
/// between them. A path that runs to the end of the match pins it after the last death too.
pub(crate) struct Track<'a> {
    paths: &'a MatchPaths,
    path: &'a PlayerPath,
    /// (game time, index - game time), ascending.
    anchors: Vec<(f64, f64)>,
}

impl<'a> Track<'a> {
    pub(crate) fn new(info: &'a MatchInfo, me: &Player) -> Option<Track<'a>> {
        let paths = info.match_paths.as_ref()?;
        let path = paths
            .player(me.player_slot)
            .filter(|p| !p.x_pos.is_empty())?;
        let h = &path.health;
        // A path that ends while the player is dead cuts that zero run short.
        let zeros: Vec<usize> = (1..h.len())
            .filter(|&i| h[i] == 0 && h[i - 1] > 0)
            .filter(|&i| {
                let run = h[i..].iter().take_while(|&&v| v == 0).count();
                run >= MIN_DEATH_SAMPLES || i + run == h.len()
            })
            .collect();
        let mut deaths: Vec<f64> = me
            .death_details
            .iter()
            .filter_map(|d| d.game_time_s.map(f64::from))
            .collect();
        deaths.sort_by(f64::total_cmp);
        let mut anchors = vec![(0.0, 1.0)];
        for d in deaths {
            let drift = anchors[anchors.len() - 1].1;
            // Drift only grows, and never by more than a minute between two deaths.
            if let Some(&i) = zeros
                .iter()
                .find(|&&i| (d + drift - 2.0..=d + drift + 60.0).contains(&(i as f64)))
            {
                anchors.push((d, i as f64 - d));
            }
        }
        // A full path's last sample is at the match's last second. One that ends while the
        // player is dead, or earlier than the drift so far allows, stopped early.
        let last = anchors[anchors.len() - 1];
        let end = f64::from(info.duration_s) - 1.0;
        let end_drift = (path.x_pos.len() - 1) as f64 - end;
        if h.last().is_some_and(|&v| v > 0) && end > last.0 && end_drift >= last.1 {
            anchors.push((end, end_drift));
        }
        Some(Track {
            paths,
            path,
            anchors,
        })
    }

    fn len(&self) -> usize {
        self.path.x_pos.len()
    }

    /// Game time of the last sample, at the drift of the last anchor.
    pub(crate) fn last_game_time(&self) -> f64 {
        (self.len() - 1) as f64 - self.anchors[self.anchors.len() - 1].1
    }

    pub(crate) fn index(&self, game_time: f64) -> i64 {
        let prev = self
            .anchors
            .iter()
            .rev()
            .find(|a| a.0 <= game_time)
            .unwrap_or(&self.anchors[0]);
        let drift = self
            .anchors
            .iter()
            .find(|a| a.0 > game_time)
            .map_or(prev.1, |next| {
                prev.1 + (next.1 - prev.1) * (game_time - prev.0) / (next.0 - prev.0)
            });
        (game_time + drift).round() as i64
    }

    /// Alive and within the assist radius of `pos` at some second of the assist window.
    pub(crate) fn near(&self, game_time: f64, pos: &Pos) -> bool {
        let i = self.index(game_time);
        (i - ASSIST_WINDOW_S..=i)
            .filter_map(|j| usize::try_from(j).ok())
            .filter(|&j| j < self.len())
            .any(|j| {
                self.path.health.get(j).is_some_and(|&h| h > 0)
                    && self.paths.position(self.path, j).distance(pos) <= ASSIST_RADIUS
            })
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "whole-second path times are exact")]
mod tests {
    use super::*;
    use crate::test_support::{match_info, path, player};
    use serde_json::json;

    /// Me in slot 1 with an own death at `death`, health zero on `zeros`, 300 samples.
    fn info(
        death: Option<u32>,
        zeros: std::ops::Range<usize>,
        len: usize,
        duration: u32,
    ) -> MatchInfo {
        let mut health = vec![100; len];
        for h in &mut health[zeros] {
            *h = 0;
        }
        let deaths: Vec<_> = death.map(|d| (d, Some(2), Some(0.0))).into_iter().collect();
        match_info(
            duration,
            vec![player(1, 1, 0, &deaths), player(2, 2, 1, &[])],
            Some(vec![path(1, &vec![5000; len], &health)]),
            &json!({}),
        )
    }

    #[test]
    fn minimal_match_info_deserialises() {
        let m = info(None, 0..0, 10, 10);
        assert_eq!(m.players.len(), 2);
        assert!(m.match_paths.is_some());
    }

    #[test]
    fn track_pins_drift_at_own_death() {
        let m = info(Some(100), 105..111, 300, 400);
        let t = Track::new(&m, &m.players[0]).unwrap();
        assert_eq!(t.index(100.0), 105);
        assert_eq!(t.index(50.0), 53);
        assert_eq!(t.index(200.0), 205);
    }

    #[test]
    fn blip_does_not_pin() {
        let m = info(Some(100), 105..107, 300, 400);
        let t = Track::new(&m, &m.players[0]).unwrap();
        assert_eq!(t.index(100.0), 101);
    }

    #[test]
    fn zero_run_reaching_path_end_pins() {
        let m = info(Some(100), 105..108, 108, 400);
        let t = Track::new(&m, &m.players[0]).unwrap();
        assert_eq!(t.index(100.0), 105);
    }

    #[test]
    fn full_path_pins_end_drift() {
        // 1801 samples over 1800 s: the end anchor holds drift 1 to the last second.
        let m = info(None, 0..0, 1801, 1800);
        let t = Track::new(&m, &m.players[0]).unwrap();
        assert_eq!(t.last_game_time(), 1799.0);
        assert_eq!(t.index(1799.0), 1800);
    }

    #[test]
    fn short_path_last_game_time() {
        let m = info(None, 0..0, 1000, 1800);
        let t = Track::new(&m, &m.players[0]).unwrap();
        assert_eq!(t.last_game_time(), 998.0);
    }

    #[test]
    fn near_is_alive_and_within_radius_in_window() {
        // No own death: index(t) = t + 1, so at t = 50 the window is samples 48..=51.
        let mut xs = vec![0; 100];
        let mut health = vec![100; 100];
        xs[47] = 7000;
        xs[48] = 2400;
        xs[51] = 9000;
        health[51] = 0;
        let m = match_info(
            100,
            vec![player(1, 1, 0, &[])],
            Some(vec![path(1, &xs, &health)]),
            &json!({}),
        );
        let t = Track::new(&m, &m.players[0]).unwrap();
        let x = |x: f64| Pos { x, y: 0.0, z: 0.0 };
        assert!(t.near(50.0, &x(4800.0)), "sample 48, alive, 2400 away");
        assert!(!t.near(50.0, &x(4901.0)), "2501 from sample 48");
        assert!(
            !t.near(50.0, &x(9000.0)),
            "sample 51 is on the spot but dead"
        );
        assert!(!t.near(50.0, &x(7000.0)), "sample 47 is before the window");
    }

    #[test]
    fn dead_in_whole_window_is_not_near() {
        let xs = vec![0; 100];
        let mut health = vec![100; 100];
        for h in &mut health[40..60] {
            *h = 0;
        }
        let m = match_info(
            100,
            vec![player(1, 1, 0, &[])],
            Some(vec![path(1, &xs, &health)]),
            &json!({}),
        );
        let t = Track::new(&m, &m.players[0]).unwrap();
        assert!(!t.near(
            50.0,
            &Pos {
                x: 0.0,
                y: 0.0,
                z: 0.0
            }
        ));
        assert!(t.near(
            70.0,
            &Pos {
                x: 2500.0,
                y: 0.0,
                z: 0.0
            }
        ));
        assert!(!t.near(
            70.0,
            &Pos {
                x: 2501.0,
                y: 0.0,
                z: 0.0
            }
        ));
    }

    #[test]
    fn no_paths_no_track() {
        let m = match_info(100, vec![player(1, 1, 0, &[])], None, &json!({}));
        assert!(Track::new(&m, &m.players[0]).is_none());
    }
}
