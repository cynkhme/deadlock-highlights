//! Where a match starts on the VOD, from its clock readings and pauses.

use crate::hud::clock::ClockReading;

/// Votes farther than this from the first median are misreads, s.
const START_TOLERANCE_S: f64 = 5.0;
/// Cluster gaps and pause sums this close agree, s.
const PAUSE_TOLERANCE_S: f64 = 5.0;

/// A match pause: the game time the clock stopped at, and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Pause {
    pub game_time_s: f64,
    pub duration_s: f64,
}

/// Seconds since the match started, pauses included: the game clock stops during them.
pub(crate) fn elapsed(game_time: f64, pauses: &[Pause]) -> f64 {
    game_time
        + pauses
            .iter()
            .filter(|p| p.game_time_s <= game_time)
            .map(|p| p.duration_s)
            .sum::<f64>()
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    })
}

/// The VOD time at which the match started.
pub(crate) fn vod_start(readings: &[ClockReading], pauses: &[Pause]) -> Option<f64> {
    let votes: Vec<f64> = readings
        .iter()
        .map(|r| r.vod_time - elapsed(f64::from(r.clock_s), pauses))
        .collect();
    let first = median(votes.clone())?;
    median(
        votes
            .into_iter()
            .filter(|v| (v - first).abs() <= START_TOLERANCE_S)
            .collect(),
    )
}

/// Each later start of a match sits after the earlier ones by the pauses between them, so a
/// gap that no running sum of the metadata pauses explains means the metadata lost a pause.
/// Returns the gaps and the running sums when they disagree.
pub(crate) fn pause_mismatch(
    starts_vod_s: &[i64],
    pauses: &[Pause],
) -> Option<(Vec<i64>, Vec<f64>)> {
    let mut starts = starts_vod_s.to_vec();
    starts.sort_unstable();
    let gaps: Vec<i64> = starts.iter().skip(1).map(|s| s - starts[0]).collect();
    let mut by_time = pauses.to_vec();
    by_time.sort_by(|a, b| a.game_time_s.total_cmp(&b.game_time_s));
    let sums: Vec<f64> = by_time
        .iter()
        .scan(0.0, |acc, p| {
            *acc += p.duration_s;
            Some(*acc)
        })
        .collect();
    let explained = |g: i64| {
        sums.iter()
            .any(|s| (g as f64 - s).abs() <= PAUSE_TOLERANCE_S)
    };
    (!gaps.iter().all(|&g| explained(g))).then_some((gaps, sums))
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "medians of whole and half seconds are exact"
)]
mod tests {
    use super::*;

    fn p(game_time_s: f64, duration_s: f64) -> Pause {
        Pause {
            game_time_s,
            duration_s,
        }
    }

    fn r(vod_time: f64, clock: &str) -> ClockReading {
        ClockReading {
            vod_time,
            clock_s: crate::hud::clock::parse_clock(clock).unwrap(),
        }
    }

    #[test]
    fn vod_start_accounts_for_pauses_and_ignores_misreads() {
        let pauses = [p(600.0, 30.0)];
        let readings = [
            r(1000.0, "8:20"),
            r(1500.0, "16:10"),
            r(1600.0, "17:51"),
            r(1700.0, "2:00"),
        ];
        assert_eq!(vod_start(&readings, &pauses), Some(500.0));
    }

    #[test]
    fn vod_start_even_count_is_midpoint() {
        assert_eq!(
            vod_start(&[r(100.0, "0:10"), r(101.0, "0:10")], &[]),
            Some(90.5)
        );
        assert_eq!(vod_start(&[], &[]), None);
    }

    #[test]
    fn elapsed_counts_pause_at_its_own_time() {
        assert_eq!(elapsed(600.0, &[p(600.0, 30.0)]), 630.0);
        assert_eq!(elapsed(599.0, &[p(600.0, 30.0)]), 599.0);
        assert_eq!(elapsed(900.0, &[p(600.0, 30.0), p(700.0, 15.0)]), 945.0);
    }

    #[test]
    fn pause_mismatch_none_when_equal() {
        assert_eq!(
            pause_mismatch(&[0, 22, 117], &[p(300.0, 22.0), p(900.0, 94.0)]),
            None
        );
        assert_eq!(
            pause_mismatch(&[117, 0, 22], &[p(900.0, 94.0), p(300.0, 22.0)]),
            None
        );
    }

    #[test]
    fn pause_mismatch_flags_dropped_pause() {
        let m = pause_mismatch(&[0, 40], &[]).unwrap();
        assert_eq!(m, (vec![40], vec![]));
        assert!(pause_mismatch(&[0, 40], &[p(300.0, 34.0)]).is_some());
        assert!(pause_mismatch(&[0, 40], &[p(300.0, 35.0)]).is_none());
    }

    #[test]
    fn pause_mismatch_one_start_none() {
        assert_eq!(pause_mismatch(&[10], &[p(300.0, 50.0)]), None);
    }
}
