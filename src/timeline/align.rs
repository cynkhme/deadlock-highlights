//! Game starts from clock readings, aligned to the match history by one wall-clock offset.

use crate::error::{Error, Result};
use crate::hud::clock::ClockReading;

/// How far a game start may land from a history `start_time` and still hit it, s.
const HIT_TOLERANCE_S: i64 = 60;
/// Offsets considered around a wall-clock anchor, s.
pub(crate) const ANCHOR_WINDOW_S: i64 = 600;
/// Clock-0:00 values at most this far apart belong to one start, s.
const CLUSTER_GAP_S: f64 = 5.0;
const MIN_CLUSTER_READINGS: usize = 3;
/// Without an anchor, the fewest starts that must hit.
const MIN_UNANCHORED_HITS: usize = 6;
/// Without an anchor, a second top-score run farther than this rejects the offset, s.
const MAX_RUN_SPREAD_S: i64 = 2 * HIT_TOLERANCE_S;

/// A match of the account's history, as align needs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryMatch {
    pub match_id: u64,
    pub start_time: i64,
    pub duration_s: u32,
    pub game_mode: Option<u32>,
}

/// A game start: the VOD time of clock 0:00 and the readings that put it there.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GameStart {
    pub vod_s: i64,
    pub readings: Vec<ClockReading>,
}

/// An offset that was not accepted: the best one found, if any, and its hits.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlignReject {
    pub offset: Option<i64>,
    pub hits: usize,
}

/// The VOD's wall-clock offset and every start with the history match it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct Aligned {
    pub offset: i64,
    pub hits: usize,
    pub starts: Vec<(GameStart, Option<u64>)>,
}

pub(crate) fn starts(readings: &[ClockReading]) -> Vec<GameStart> {
    let mut zero: Vec<(f64, ClockReading)> = readings
        .iter()
        .map(|r| (r.vod_time - f64::from(r.clock_s), *r))
        .collect();
    zero.sort_by(|a, b| a.0.total_cmp(&b.0));
    zero.chunk_by(|a, b| b.0 - a.0 <= CLUSTER_GAP_S)
        .filter(|c| c.len() >= MIN_CLUSTER_READINGS)
        .map(|c| {
            let n = c.len();
            let median = if n % 2 == 1 {
                c[n / 2].0
            } else {
                f64::midpoint(c[n / 2 - 1].0, c[n / 2].0)
            };
            GameStart {
                vod_s: median.round() as i64,
                readings: c.iter().map(|(_, r)| *r).collect(),
            }
        })
        .collect()
}

/// Starts (of `start_vod_s`) with a history `start_time` within `HIT_TOLERANCE_S` of
/// `start + offset`.
fn hits(start_vod_s: &[i64], times: &[i64], offset: i64) -> usize {
    start_vod_s
        .iter()
        .filter(|&&s| {
            let i = times.partition_point(|&t| t < s + offset - HIT_TOLERANCE_S);
            i < times.len() && times[i] <= s + offset + HIT_TOLERANCE_S
        })
        .count()
}

/// The top score and the maximal runs of consecutive offsets that reach it, within
/// `window` when given. The score is constant between the breakpoints of the hit
/// intervals `[t - s - HIT_TOLERANCE_S, t - s + HIT_TOLERANCE_S]`.
fn runs(
    start_vod_s: &[i64],
    times: &[i64],
    window: Option<(i64, i64)>,
) -> (usize, Vec<(i64, i64)>) {
    let mut bps: Vec<i64> = Vec::with_capacity(start_vod_s.len() * times.len() * 2);
    for &s in start_vod_s {
        for &t in times {
            let (mut a, mut b) = (t - s - HIT_TOLERANCE_S, t - s + HIT_TOLERANCE_S);
            if let Some((lo, hi)) = window {
                if b < lo || a > hi {
                    continue;
                }
                (a, b) = (a.max(lo), b.min(hi));
            }
            bps.push(a);
            bps.push(b + 1);
        }
    }
    if let Some((lo, hi)) = window {
        bps.push(lo);
        bps.retain(|&p| p <= hi);
    }
    bps.sort_unstable();
    bps.dedup();
    let hits_at: Vec<(i64, usize)> = bps
        .iter()
        .map(|&p| (p, hits(start_vod_s, times, p)))
        .collect();
    let top = hits_at.iter().map(|&(_, v)| v).max().unwrap_or(0);
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (i, &(p, v)) in hits_at.iter().enumerate() {
        if v != top {
            continue;
        }
        let end = match (hits_at.get(i + 1), window) {
            (Some(&(next, _)), _) => next - 1,
            (None, Some((_, hi))) => hi,
            (None, None) => p,
        };
        match out.last_mut() {
            Some(last) if last.1 == p - 1 => last.1 = end,
            _ => out.push((p, end)),
        }
    }
    (top, out)
}

/// The wall-clock offset (history time minus VOD time) and its score.
pub(crate) fn align(
    start_vod_s: &[i64],
    history: &[HistoryMatch],
    anchor: Option<i64>,
) -> std::result::Result<(i64, usize), AlignReject> {
    let mut times: Vec<i64> = history.iter().map(|h| h.start_time).collect();
    times.sort_unstable();
    match anchor {
        Some(anchor) => align_anchored(start_vod_s, &times, anchor),
        None => align_unanchored(start_vod_s, &times),
    }
}

fn centre((a, b): (i64, i64)) -> i64 {
    (a + b).div_euclid(2)
}

fn align_anchored(
    start_vod_s: &[i64],
    times: &[i64],
    anchor: i64,
) -> std::result::Result<(i64, usize), AlignReject> {
    let window = (anchor - ANCHOR_WINDOW_S, anchor + ANCHOR_WINDOW_S);
    let (top, found) = runs(start_vod_s, times, Some(window));
    let distance = |&(a, b): &(i64, i64)| {
        if a <= anchor && anchor <= b {
            0
        } else {
            (a - anchor).abs().min((b - anchor).abs())
        }
    };
    // min_by_key keeps the first of equal keys, so a tie goes to the earlier run.
    let offset = found
        .iter()
        .min_by_key(|r| distance(r))
        .copied()
        .map(centre);
    match offset {
        Some(offset) if top >= 1 && 2 * top >= start_vod_s.len() => Ok((offset, top)),
        _ => Err(AlignReject { offset, hits: top }),
    }
}

fn align_unanchored(
    start_vod_s: &[i64],
    times: &[i64],
) -> std::result::Result<(i64, usize), AlignReject> {
    let (top, found) = runs(start_vod_s, times, None);
    let Some(&first) = found.first() else {
        return Err(AlignReject {
            offset: None,
            hits: 0,
        });
    };
    let offset = centre(first);
    let spread = found
        .iter()
        .skip(1)
        .any(|r| r.0 - first.1 > MAX_RUN_SPREAD_S);
    if top >= MIN_UNANCHORED_HITS && !spread {
        Ok((offset, top))
    } else {
        Err(AlignReject {
            offset: Some(offset),
            hits: top,
        })
    }
}

/// The history match a start belongs to: the latest one starting no later than
/// `HIT_TOLERANCE_S` after it, if that match lasts until the start. `history` is sorted by
/// `start_time`.
pub(crate) fn assign(start: i64, offset: i64, history: &[HistoryMatch]) -> Option<u64> {
    let wall = start + offset;
    let i = history.partition_point(|h| h.start_time <= wall + HIT_TOLERANCE_S);
    let h = history.get(i.checked_sub(1)?)?;
    (wall - h.start_time <= i64::from(h.duration_s)).then_some(h.match_id)
}

/// The game starts in `readings`, the offset that fits them to `history`, and the match
/// of each start; `history` is sorted by `start_time`.
pub(crate) fn align_readings(
    readings: &[ClockReading],
    history: &[HistoryMatch],
    anchor: Option<i64>,
) -> Result<Aligned> {
    let found = starts(readings);
    if found.is_empty() {
        return Err(Error::NoGameStart {
            valid_clocks: readings.len(),
        });
    }
    let vod_s: Vec<i64> = found.iter().map(|s| s.vod_s).collect();
    let (offset, hits) = align(&vod_s, history, anchor).map_err(|r| Error::OffsetRejected {
        offset: r.offset,
        hits: r.hits,
        start_count: vod_s.len(),
        anchored: anchor.is_some(),
    })?;
    let starts = found
        .into_iter()
        .map(|s| {
            let m = assign(s.vod_s, offset, history);
            (s, m)
        })
        .collect();
    Ok(Aligned {
        offset,
        hits,
        starts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn reading(vod_time: f64, clock_s: u32) -> ClockReading {
        ClockReading { vod_time, clock_s }
    }

    /// Readings whose clock-0:00 values are exactly `values`.
    fn at(values: &[f64]) -> Vec<ClockReading> {
        values.iter().map(|&v| reading(v + 600.0, 600)).collect()
    }

    fn hist(times: &[i64], duration_s: u32) -> Vec<HistoryMatch> {
        times
            .iter()
            .enumerate()
            .map(|(i, &t)| HistoryMatch {
                match_id: 100 + i as u64,
                start_time: t,
                duration_s,
                game_mode: Some(1),
            })
            .collect()
    }

    fn fixture(name: &str) -> Option<Value> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/align")
            .join(name);
        let Ok(text) = std::fs::read_to_string(&p) else {
            eprintln!("skipping: {} absent", p.display());
            return None;
        };
        Some(serde_json::from_str(&text).unwrap())
    }

    fn history_of(v: &Value) -> Vec<HistoryMatch> {
        let mut h: Vec<HistoryMatch> = v["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| HistoryMatch {
                match_id: e["match_id"].as_u64().unwrap(),
                start_time: e["start_time"].as_i64().unwrap(),
                duration_s: e["duration_s"].as_u64().unwrap() as u32,
                game_mode: e["game_mode"].as_u64().map(|m| m as u32),
            })
            .collect();
        h.sort_by_key(|e| e.start_time);
        h
    }

    fn ints(v: &Value) -> Vec<i64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_i64().unwrap())
            .collect()
    }

    fn ids(v: &Value) -> Vec<Option<u64>> {
        v.as_array().unwrap().iter().map(Value::as_u64).collect()
    }

    fn assign_all(starts: &[i64], offset: i64, h: &[HistoryMatch]) -> Vec<Option<u64>> {
        starts.iter().map(|&s| assign(s, offset, h)).collect()
    }

    #[test]
    fn starts_cluster_by_single_linkage() {
        let s = starts(&at(&[100.0, 103.0, 107.0, 200.0, 201.0]));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].vod_s, 103);
        assert_eq!(s[0].readings.len(), 3);
    }

    #[test]
    fn starts_round_median() {
        let s = starts(&at(&[10.4, 10.6, 11.0, 11.2]));
        assert_eq!(s.iter().map(|s| s.vod_s).collect::<Vec<_>>(), [11]);
    }

    #[test]
    fn sweep_finds_offset_pair_rule_misses() {
        let Some(v) = fixture("short-vod.json") else {
            return;
        };
        let h = history_of(&v);
        let s = ints(&v["starts"]);
        let (off, hits) = align(&s, &h, v["anchor"].as_i64()).unwrap();
        assert_eq!(hits, 3);
        assert_eq!(assign_all(&s, off, &h), ids(&v["assign"]));
    }

    #[test]
    fn full_vod_aligns() {
        let (Some(fx), Some(exp)) = (fixture("full-vod.json"), fixture("expected.json")) else {
            return;
        };
        let h = history_of(&fx);
        let readings: Vec<ClockReading> = fx["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                reading(
                    r["vod_time"].as_f64().unwrap(),
                    r["clock_s"].as_u64().unwrap() as u32,
                )
            })
            .collect();
        let full = &exp["full"];
        let a = align_readings(&readings, &h, None).unwrap();
        let vod_s: Vec<i64> = a.starts.iter().map(|(s, _)| s.vod_s).collect();
        assert_eq!(vod_s, ints(&full["starts"]));
        assert!(full["single_run"].as_bool().unwrap());
        assert_eq!(a.offset, full["offset"].as_i64().unwrap());
        let got: Vec<Option<u64>> = a.starts.iter().map(|(_, m)| *m).collect();
        assert_eq!(got, ids(&full["assign"]));
        let distinct: std::collections::BTreeSet<u64> = got.iter().flatten().copied().collect();
        assert_eq!(distinct.len(), 12);
        let unassigned: Vec<i64> = a
            .starts
            .iter()
            .filter(|(_, m)| m.is_none())
            .map(|(s, _)| s.vod_s)
            .collect();
        assert_eq!(unassigned, [6660]);
    }

    #[test]
    fn full_vod_windows() {
        let (Some(fx), Some(exp)) = (fixture("full-vod.json"), fixture("expected.json")) else {
            return;
        };
        let h = history_of(&fx);
        let windows = exp["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 189);
        for w in windows {
            let s = ints(&w["starts"]);
            let truth = ids(&w["assign"]);
            let outcome = match align(&s, &h, w["anchor"].as_i64()) {
                Err(_) => "rejected",
                Ok((off, _)) if assign_all(&s, off, &h) == truth => "right",
                Ok(_) => "wrong",
            };
            assert_eq!(outcome, w["outcome"].as_str().unwrap(), "window {w}");
        }
    }

    #[test]
    fn wrong_anchor_rejected() {
        let (Some(fx), Some(exp)) = (fixture("full-vod.json"), fixture("expected.json")) else {
            return;
        };
        let h = history_of(&fx);
        let s = ints(&exp["full"]["starts"]);
        let cases = exp["wrong_anchors"].as_array().unwrap();
        assert_eq!(cases.len(), 4);
        for c in cases {
            assert!(!c["accepted"].as_bool().unwrap());
            assert!(align(&s, &h, c["anchor"].as_i64()).is_err(), "anchor {c}");
        }
    }

    #[test]
    fn align_cut_first_match_is_unmatched_not_wrong() {
        let (Some(fx), Some(exp)) = (fixture("full-vod.json"), fixture("expected.json")) else {
            return;
        };
        let h = history_of(&fx);
        let mut readings: Vec<ClockReading> = fx["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                reading(
                    r["vod_time"].as_f64().unwrap(),
                    r["clock_s"].as_u64().unwrap() as u32,
                )
            })
            .collect();
        let first = starts(&readings).remove(0);
        let cut: Vec<ClockReading> = first.readings[2..].to_vec();
        readings.retain(|r| !cut.contains(r));
        let full_starts = ints(&exp["full"]["starts"]);
        let full_assign = ids(&exp["full"]["assign"]);
        let a = align_readings(&readings, &h, None).unwrap();
        assert_eq!(a.starts.len(), full_starts.len() - 1);
        assert!(a.starts.iter().all(|(s, _)| s.vod_s != first.vod_s));
        for (s, m) in &a.starts {
            let i = full_starts.iter().position(|&v| v == s.vod_s).unwrap();
            assert_eq!(*m, full_assign[i], "start {}", s.vod_s);
        }
    }

    #[test]
    fn no_start_error() {
        let e = align_readings(&at(&[0.0, 1.0]), &hist(&[0], 1800), None).unwrap_err();
        assert!(matches!(e, Error::NoGameStart { valid_clocks: 2 }));
        assert_eq!(
            e.to_string(),
            "no game start: 2 valid clocks, none 3 within 5 s"
        );
    }

    #[test]
    fn anchor_too_few_hits() {
        let h = hist(&[10_000], 1800);
        let r = align(&[0, 1000, 2000, 3000], &h, Some(10_000)).unwrap_err();
        assert_eq!(r.hits, 1);
    }

    #[test]
    fn no_anchor_needs_six() {
        let s = [0, 1000, 2000, 3000, 4000];
        let h = hist(&s.map(|v| v + 10_000), 600);
        assert_eq!(align(&s, &h, None).unwrap_err().hits, 5);
    }

    #[test]
    fn no_anchor_six_accepted() {
        let s = [0, 1000, 2000, 3000, 4000, 5000];
        let h = hist(&s.map(|v| v + 10_000), 600);
        assert_eq!(align(&s, &h, None).unwrap(), (10_000, 6));
    }

    #[test]
    fn no_anchor_second_run_far_rejects() {
        let s = [0, 1000, 2000, 3000, 4000, 5000];
        let mut times: Vec<i64> = s.iter().map(|v| v + 10_000).collect();
        times.extend(s.iter().map(|v| v + 10_300));
        times.sort_unstable();
        let r = align(&s, &hist(&times, 100), None).unwrap_err();
        assert_eq!(r.hits, 6);
    }

    #[test]
    fn rejection_message_names_vod_start() {
        let h = hist(&[10_000], 1800);
        let readings = at(&[
            0.0, 1.0, 2.0, 1000.0, 1001.0, 1002.0, 2000.0, 2001.0, 2002.0, 3000.0, 3001.0, 3002.0,
        ]);
        let e = align_readings(&readings, &h, Some(10_000)).unwrap_err();
        let m = e.to_string();
        assert!(
            m.contains("of 4 game starts") && m.contains("best offset") && !m.contains("--"),
            "{m}"
        );
    }

    #[test]
    fn anchor_tie_goes_earlier() {
        let h = hist(&[1000, 1400], 100);
        assert_eq!(align(&[0], &h, Some(1200)).unwrap(), (1000, 1));
    }

    #[test]
    fn anchor_inside_run_wins() {
        let h = hist(&[1000, 1400], 100);
        assert_eq!(align(&[0], &h, Some(1390)).unwrap(), (1400, 1));
    }

    #[test]
    fn assign_requires_duration_cover() {
        let h = hist(&[10_000], 1800);
        assert_eq!(assign(12_000, 0, &h), None);
        assert_eq!(assign(9_970, 0, &h), Some(100));
        assert_eq!(assign(11_800, 0, &h), Some(100));
        assert_eq!(assign(9_900, 0, &h), None);
    }
}
