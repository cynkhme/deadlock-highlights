//! The stages in order: the history, the read of the VOD, then everything placed from the
//! readings.

use std::path::PathBuf;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::game::history::{History, load_history};
use crate::game::source::api::ApiSource;
use crate::game::source::{Fetched, MatchSource, skip_status};
use crate::highlights::{match_end, rank, vod_combat, vod_events, windows};
use crate::hud::cache;
use crate::hud::clock::ClockReading;
use crate::hud::gate::Gate;
use crate::hud::reader::{
    FrameReading, HudReader, RealTime, clock_readings, read_frames, require_clock,
};
use crate::output::{
    Alignment, Degraded, Highlight, MatchReport, MatchStatus, Output, Progress, SCHEMA,
    UnmatchedStart, UnseenMatch, VodInfo,
};
use crate::timeline::align::{GameStart, HistoryMatch, align_readings};
use crate::timeline::place::{pause_mismatch, vod_start};
use crate::video::{GRID_S, Probe, cut_clip, keyframes};

fn notify(cfg: &Config, p: Progress) {
    if let Some(f) = &cfg.progress {
        f(p);
    }
}

/// Every stage after the probe.
pub(crate) fn run(cfg: &Config, reader: &dyn HudReader, probe: &Probe) -> Result<Output> {
    begin(cfg, probe)?;
    let mut source = ApiSource::new(cfg, None)?;
    let history = prepare(cfg, probe, &mut source)?;
    let frames = read_vod(cfg, probe, reader)?;
    place(cfg, &mut source, probe, history, &frames)
}

/// Reports the probe and creates the output directory.
fn begin(cfg: &Config, probe: &Probe) -> Result<()> {
    notify(
        cfg,
        Progress::Probed {
            duration_s: probe.duration_s,
        },
    );
    std::fs::create_dir_all(&cfg.out).map_err(|source| Error::Io {
        path: cfg.out.clone(),
        source,
    })
}

/// The match history from `source`, reported once loaded.
fn prepare(cfg: &Config, probe: &Probe, source: &mut dyn MatchSource) -> Result<History> {
    let history = load_history(source, cfg, probe, &|w| notify(cfg, Progress::Warning(w)))?;
    notify(
        cfg,
        Progress::History {
            matches: history.matches.len(),
            anchor: history.anchor.map(|a| a.1),
        },
    );
    Ok(history)
}

/// Everything placed from the readings: alignment, each match's place and windows, and
/// the clips.
fn place(
    cfg: &Config,
    source: &mut dyn MatchSource,
    probe: &Probe,
    history: History,
    frames: &[FrameReading],
) -> Result<Output> {
    require_clock(frames, &cfg.profile)?;
    let mut hist = history.matches;
    // Assignment takes the latest match by start time.
    hist.sort_by_key(|m| m.start_time);
    let aligned = align_readings(&clock_readings(frames), &hist, history.anchor.map(|a| a.0))?;
    let offset = aligned.offset;
    notify(
        cfg,
        Progress::Aligned {
            offset,
            start_count: aligned.starts.len(),
        },
    );

    // Starts come in VOD order, so matches keep the order of their first start.
    let mut groups: Vec<(u64, Vec<&GameStart>)> = Vec::new();
    let mut unmatched_starts = Vec::new();
    for (start, m) in &aligned.starts {
        match m {
            Some(id) => match groups.iter_mut().find(|(g, _)| g == id) {
                Some((_, starts)) => starts.push(start),
                None => groups.push((*id, vec![start])),
            },
            None => unmatched_starts.push(unmatched(start)),
        }
    }

    let (matches, mut highlights) = place_matches(cfg, source, &hist, &groups, probe.duration_s);
    rank(&mut highlights);

    let unseen = unseen(&hist, &groups, offset, probe.duration_s);
    let newest = hist.last().map_or(i64::MIN, |h| h.start_time);
    let retryable = matches
        .iter()
        .any(|m| matches!(m.status, MatchStatus::Unavailable { .. } | MatchStatus::NotIngested))
        // The history may not list a match that started after its newest entry yet.
        || unmatched_starts.iter().any(|s| s.last_reading + offset as f64 > newest as f64);
    let clips = clips(cfg, &highlights)?;
    Ok(Output {
        schema: SCHEMA,
        vod: VodInfo {
            path: cfg.vod.clone(),
            size: probe.size,
            mtime: probe.mtime,
            duration_s: probe.duration_s,
        },
        alignment: Alignment {
            offset,
            anchor: history.anchor.map(|a| a.1),
            hits: aligned.hits,
            start_count: aligned.starts.len(),
        },
        matches,
        unmatched_starts,
        unseen,
        retryable,
        highlights,
        clips,
    })
}

/// Places each assigned match once, in VOD order, fetching only those [`skip_status`] passes;
/// returns its reports and windows.
fn place_matches(
    cfg: &Config,
    source: &mut dyn MatchSource,
    hist: &[HistoryMatch],
    groups: &[(u64, Vec<&GameStart>)],
    vod_duration: f64,
) -> (Vec<MatchReport>, Vec<Highlight>) {
    let mut matches = Vec::new();
    let mut highlights = Vec::new();
    for (id, starts) in groups {
        let h = hist
            .iter()
            .find(|h| h.match_id == *id)
            .expect("assigned from the history");
        let mut m = MatchReport {
            match_id: *id,
            start_time: h.start_time,
            vod_start: None,
            status: MatchStatus::Placed,
            degraded: Vec::new(),
        };
        if let Some(s) = skip_status(h) {
            m.status = s;
        } else {
            match source.fetch(h) {
                Fetched::Status(s) => m.status = s,
                Fetched::Ready { timing, events } => {
                    let readings: Vec<ClockReading> = starts
                        .iter()
                        .flat_map(|s| s.readings.iter().copied())
                        .collect();
                    m.vod_start = vod_start(&readings, &timing.pauses);
                    if !events.has_paths {
                        m.degraded.push(Degraded::NoPaths);
                    }
                    let vod_s: Vec<i64> = starts.iter().map(|s| s.vod_s).collect();
                    if let Some((gaps_s, pauses_s)) = pause_mismatch(&vod_s, &timing.pauses) {
                        m.degraded
                            .push(Degraded::PauseMismatch { gaps_s, pauses_s });
                    }
                    if let Some(v) = m.vod_start {
                        let (p, d) = (&timing.pauses, timing.duration_s);
                        let on_vod = vod_events(&events, v, p, d, vod_duration);
                        let combat = vod_combat(&events, v, p, d, vod_duration);
                        let end = match_end(&events, v, p, d);
                        highlights.extend(windows(*id, &on_vod, &combat, end, vod_duration));
                    }
                }
            }
        }
        notify(cfg, Progress::Match(m.clone()));
        matches.push(m);
    }
    (matches, highlights)
}

/// History matches the VOD should show, inside its aligned span, that got no game start.
fn unseen(
    hist: &[HistoryMatch],
    groups: &[(u64, Vec<&GameStart>)],
    offset: i64,
    vod_duration: f64,
) -> Vec<UnseenMatch> {
    let span_end = offset + vod_duration.ceil() as i64;
    hist.iter()
        .filter(|h| skip_status(h).is_none())
        .filter(|h| h.start_time <= span_end && h.start_time + i64::from(h.duration_s) >= offset)
        .filter(|h| !groups.iter().any(|(id, _)| *id == h.match_id))
        .map(|h| UnseenMatch {
            match_id: h.match_id,
            start_time: h.start_time,
        })
        .collect()
}

fn unmatched(start: &GameStart) -> UnmatchedStart {
    let times = start.readings.iter().map(|r| r.vod_time);
    UnmatchedStart {
        first_reading: times.clone().fold(f64::INFINITY, f64::min),
        last_reading: times.fold(f64::NEG_INFINITY, f64::max),
        readings: start.readings.len(),
    }
}

/// The read-cache key of `cfg` on this VOD.
fn cache_key(cfg: &Config, probe: &Probe, reader: &dyn HudReader) -> cache::CacheKey {
    cache::CacheKey {
        band: cfg.profile.band.clone(),
        t: cfg.profile.t,
        reader: reader.identity(),
        grid_s: GRID_S,
        vod_size: probe.size,
        vod_mtime: probe.mtime,
        read_format: cache::READ_FORMAT,
    }
}

/// The cached readings of this VOD and settings, or a fresh read that is then cached.
fn read_vod(cfg: &Config, probe: &Probe, reader: &dyn HudReader) -> Result<Vec<FrameReading>> {
    let key = cache_key(cfg, probe, reader);
    if let Some(r) = cache::load(&cfg.out, &key) {
        return Ok(r);
    }
    reader.preflight()?;
    let frames = keyframes(&cfg.vod, probe, &cfg.profile, GRID_S)?;
    let gate = Gate::new();
    let r = read_frames(
        &frames,
        &|gray| gate.score(gray),
        cfg.profile.t,
        reader,
        usize::from(cfg.parallel),
        &RealTime::new(),
        &|done, total| notify(cfg, Progress::Read { done, total }),
    )?;
    cache::store(&cfg.out, &key, &cfg.profile, &r)?;
    Ok(r)
}

/// Cuts the windows at or above the score threshold, best first; an existing clip is kept.
fn clips(cfg: &Config, ranked: &[Highlight]) -> Result<Vec<PathBuf>> {
    let ext = cfg
        .vod
        .extension()
        .map_or("mp4".into(), |e| e.to_string_lossy().into_owned());
    let dir = cfg.out.join("clips");
    let mut out = Vec::new();
    for h in ranked
        .iter()
        .filter(|h| h.score >= cfg.clips_min_score)
        .take(cfg.clips_max.unwrap_or(usize::MAX))
    {
        // Named by match and VOD time, so runs with other settings do not collide.
        let dest = dir.join(format!("{}-{}.{ext}", h.match_id, h.start.floor() as i64));
        if !dest.exists() {
            std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
                path: dir.clone(),
                source,
            })?;
            cut_clip(&cfg.vod, h.start, h.end, &dest)?;
        }
        out.push(dest);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hud::reader::{Outcome, ReadError, ReaderIdentity};
    use crate::test_support::{FakeApi, match_info_json, path, player};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const A: i64 = 1_793_000_000;

    fn reading(vod_time: f64, clock: &str) -> FrameReading {
        FrameReading {
            pts_ms: (vod_time * 1000.0).round() as u64,
            gate: 1.0,
            outcome: Outcome::Read {
                raw: json!({ "clock": clock }).to_string(),
            },
        }
    }

    /// `n` readings 30 s apart of a match whose clock 0:00 is at VOD `start`.
    fn cluster(start: f64, from_s: u32, n: u32) -> Vec<FrameReading> {
        (0..n)
            .map(|i| {
                let e = from_s + 30 * i;
                reading(start + f64::from(e), &format!("{}:{:02}", e / 60, e % 60))
            })
            .collect()
    }

    fn hist(id: u64, start: i64, dur: u32, mode: u32) -> Value {
        json!({"account_id": 7, "match_id": id, "start_time": start, "match_duration_s": dur, "game_mode": mode})
    }

    fn probe(duration_s: f64) -> Probe {
        Probe {
            duration_s,
            width: 1920,
            height: 1080,
            size: 1,
            mtime: 0,
        }
    }

    struct Fixture {
        _d: tempfile::TempDir,
        cfg: Config,
        api: Arc<FakeApi>,
    }

    /// Account 7 anchored at `A`; `history` served, names refused, no clips.
    fn fixture(history: &Value) -> Fixture {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(d.path().join("v.mp4"), 7, "http://127.0.0.1:9/v1", "m");
        cfg.out = d.path().join("out");
        cfg.deadlock_api_url = "http://127.0.0.1:9".into();
        cfg.vod_start = Some(A);
        cfg.clips_max = Some(0);
        let api = FakeApi::with(&[]);
        api.push(
            "/v1/players/7/match-history",
            200,
            &history.to_string(),
            None,
        );
        for _ in 0..30 {
            api.push("/v1/players/steam", 500, "{}", None);
        }
        Fixture { _d: d, cfg, api }
    }

    /// Me (7, slot 1) kills 8 at game time `kill_at`; a path for me unless `paths` is false.
    fn cache_match(f: &Fixture, id: u64, me: u32, paths: bool, kill_at: u32) {
        cache_match_combat(f, id, me, paths, &[kill_at], &[]);
    }

    /// [`cache_match`] with my kills of 8 at the game times `kills` and my path's combat types.
    fn cache_match_combat(
        f: &Fixture,
        id: u64,
        me: u32,
        paths: bool,
        kills: &[u32],
        combat: &[u32],
    ) {
        let paths = paths.then(|| vec![path(1, &[1000; 1801], &[100; 1801], combat)]);
        let info = match_info_json(
            1800,
            vec![
                player(me, 1, 0, &[]),
                player(
                    8,
                    2,
                    1,
                    &kills
                        .iter()
                        .map(|&t| (t, Some(1), Some(1000.0)))
                        .collect::<Vec<_>>(),
                ),
            ],
            paths,
            &json!({}),
        );
        let dir = f.cfg.out.join("matches").join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            json!({ "match_info": info }).to_string(),
        )
        .unwrap();
    }

    fn run(f: &Fixture, duration_s: f64, readings: &[FrameReading]) -> Result<Output> {
        run_on(f, &probe(duration_s), readings)
    }

    /// The run after the read, on `readings` in place of the VOD's.
    fn run_on(f: &Fixture, probe: &Probe, readings: &[FrameReading]) -> Result<Output> {
        begin(&f.cfg, probe)?;
        let mut source = ApiSource::new(&f.cfg, Some(Box::new(f.api.clone())))?;
        let history = prepare(&f.cfg, probe, &mut source)?;
        place(&f.cfg, &mut source, probe, history, readings)
    }

    #[test]
    fn place_widens_windows_with_combat() {
        // Kill at game 300 = VOD 350; combat on samples 302..=311 = game 301..=310.
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        let mut combat = vec![0; 1801];
        combat[302..=311].fill(1);
        cache_match_combat(&f, 1, 7, true, &[300], &combat);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        let h = &o.highlights[0];
        assert!((h.end - 365.0).abs() < 1e-9, "end {}", h.end);
        assert_eq!(h.combat_s, 10);
    }

    #[test]
    fn place_extends_the_window_to_the_match_end() {
        // Kill at game 1790 = VOD 1840; the match ends at game 1800 = VOD 1850.
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 1790);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert!(
            (o.highlights[0].end - 1855.0).abs() < 1e-9,
            "end {}",
            o.highlights[0].end
        );
    }

    #[test]
    fn events_after_the_vod_end_make_no_window() {
        // The VOD ends at 400, mid-match: the kill at game 300 = VOD 350 is on it; the kills
        // at game 500 and 505 = VOD 550 and 555, and the combat after them, are not.
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        let mut combat = vec![0; 1801];
        combat[500..=510].fill(1);
        cache_match_combat(&f, 1, 7, true, &[300, 500, 505], &combat);
        let o = run(&f, 400.0, &cluster(50.0, 10, 3)).unwrap();
        let spans: Vec<(f64, f64)> = o.highlights.iter().map(|h| (h.start, h.end)).collect();
        assert_eq!(spans, [(340.0, 355.0)]);
    }

    #[test]
    fn placed_with_paths_is_not_degraded() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches.len(), 1);
        assert_eq!(o.matches[0].status, MatchStatus::Placed);
        assert_eq!(o.matches[0].degraded, []);
        assert_eq!(o.matches[0].vod_start, Some(50.0));
        assert_eq!(o.alignment.offset, A);
        assert_eq!(o.highlights.len(), 1);
        assert!((o.highlights[0].events[0].vod_time - 350.0).abs() < 1e-9);
        assert!(!o.retryable);
    }

    #[test]
    fn degraded_no_paths() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, false, 300);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::Placed);
        assert_eq!(o.matches[0].degraded, [Degraded::NoPaths]);
    }

    #[test]
    fn pause_mismatch_marked() {
        // A 100 s stop of the clock that the metadata does not list.
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        let mut r = cluster(50.0, 10, 5);
        r.extend(cluster(150.0, 400, 3));
        let o = run(&f, 3600.0, &r).unwrap();
        assert_eq!(
            o.matches.len(),
            1,
            "both clusters are match 1: {:?}",
            o.matches
        );
        assert!(
            matches!(&o.matches[0].degraded[..], [Degraded::PauseMismatch { gaps_s, .. }] if gaps_s.contains(&100)),
            "{:?}",
            o.matches[0].degraded
        );
    }

    #[test]
    fn retryable_when_unavailable_or_late_unmatched_start() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        f.api.push("/v1/matches/1/metadata", 404, "{}", None);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::NotIngested);
        assert!(o.retryable);

        // An unmatched start 50 min after the newest history match started.
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        let mut r = cluster(50.0, 10, 3);
        r.extend(cluster(3000.0, 10, 3));
        let o = run(&f, 3600.0, &r).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::Placed);
        assert_eq!(o.unmatched_starts.len(), 1);
        let s = &o.unmatched_starts[0];
        assert_eq!(
            (s.first_reading, s.last_reading, s.readings),
            (3010.0, 3070.0, 3)
        );
        assert!(o.retryable);
        let json = serde_json::to_value(&o).unwrap();
        let start = &json["unmatched_starts"][0];
        assert_eq!(
            (&start["first_reading"], &start["last_reading"]),
            (&json!(3010.0), &json!(3070.0))
        );
        let alignment = &json["alignment"];
        assert_eq!(
            (&alignment["hits"], &alignment["start_count"]),
            (&json!(1), &json!(2))
        );
    }

    #[test]
    fn unseen_lists_overlapping_unassigned_matches() {
        let f = fixture(&json!([
            hist(1, A + 50, 1800, 1),
            hist(2, A + 2000, 600, 1),
            hist(3, A + 2700, 100, 1),
            hist(4, A + 3000, 600, 4),
            hist(5, A + 10_000, 1800, 1),
            hist(6, A - 1900, 1800, 1),
        ]));
        cache_match(&f, 1, 7, true, 300);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        let unseen: Vec<u64> = o.unseen.iter().map(|u| u.match_id).collect();
        assert_eq!(unseen, [2]);
    }

    /// A reader whose preflight is refused, counting its calls; it never reads.
    #[derive(Default)]
    struct Refused(AtomicUsize);

    impl HudReader for Refused {
        fn identity(&self) -> ReaderIdentity {
            ReaderIdentity {
                protocol: "fake",
                server: "s".into(),
                model: "m".into(),
                prompt: "p",
            }
        }
        fn preflight(&self) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(Error::Reader("refused".into()))
        }
        fn read(&self, _: &[u8]) -> std::result::Result<String, ReadError> {
            unreachable!("a refused reader is never read")
        }
    }

    #[test]
    fn cached_readings_need_no_server() {
        let f = fixture(&json!([]));
        let (probe, reader) = (probe(3600.0), Refused::default());
        let frames = cluster(50.0, 10, 3);
        let key = cache_key(&f.cfg, &probe, &reader);
        cache::store(&f.cfg.out, &key, &f.cfg.profile, &frames).unwrap();
        assert_eq!(read_vod(&f.cfg, &probe, &reader).unwrap(), frames);
        assert_eq!(reader.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_refused_preflight_stops_before_the_keyframe_pass() {
        let f = fixture(&json!([]));
        let reader = Refused::default();
        let e = read_vod(&f.cfg, &probe(3600.0), &reader).unwrap_err();
        assert!(matches!(&e, Error::Reader(m) if m == "refused"), "{e}");
        assert_eq!(reader.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_match_with_two_starts_is_fetched_once() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        f.api.push("/v1/matches/1/metadata", 404, "{}", None);
        // Two starts 100 s apart, both in match 1.
        let mut r = cluster(50.0, 10, 5);
        r.extend(cluster(150.0, 900, 3));
        let o = run(&f, 3600.0, &r).unwrap();
        assert_eq!(o.alignment.start_count, 2);
        assert_eq!(o.matches.len(), 1);
        assert_eq!(
            f.api
                .requests()
                .iter()
                .filter(|p| p.starts_with("/v1/matches/"))
                .count(),
            1
        );
    }

    #[test]
    fn match_without_the_account_is_not_yours() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 9, true, 300);
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::NotYours);
    }

    #[test]
    fn assigned_remake_sends_no_metadata_request() {
        let f = fixture(&json!([hist(1, A + 50, 179, 1)]));
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::Remake);
        assert!(
            !f.api
                .requests()
                .iter()
                .any(|p| p.starts_with("/v1/matches/"))
        );
    }

    #[test]
    fn assigned_other_mode_sends_no_metadata_request() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 4)]));
        let o = run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        assert_eq!(
            o.matches[0].status,
            MatchStatus::UnsupportedMode { game_mode: Some(4) }
        );
        assert!(
            !f.api
                .requests()
                .iter()
                .any(|p| p.starts_with("/v1/matches/"))
        );
    }

    #[test]
    fn bad_api_key_fails_after_probed() {
        let mut f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        f.cfg.deadlock_api_key = Some("bad\nkey".into());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        f.cfg.progress = Some(Arc::new(move |p| s.lock().unwrap().push(format!("{p:?}"))));
        let reader = Refused::default();
        let e = super::run(&f.cfg, &reader, &probe(3600.0)).unwrap_err();
        assert!(matches!(e, Error::Usage(_)), "{e:?}");
        let kinds: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.split([' ', '(']).next().unwrap().to_owned())
            .collect();
        assert_eq!(kinds, ["Probed"]);
    }

    #[test]
    fn history_failure_contacts_no_model_server() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(d.path().join("v.mp4"), 7, "http://127.0.0.1:9/v1", "m");
        cfg.out = d.path().join("out");
        // Nothing listens on port 9, so the history request fails after the crate's two
        // connect retries (1 s + 2 s).
        cfg.deadlock_api_url = "http://127.0.0.1:9".into();
        let reader = Refused::default();
        let e = super::run(&cfg, &reader, &probe(3600.0)).unwrap_err();
        assert!(matches!(e, Error::History(_)), "{e:?}");
        assert_eq!(reader.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn output_serialises_field_names() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, false, 300);
        let v = serde_json::to_value(run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap()).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["alignment"]["offset"], A);
        assert_eq!(v["alignment"]["anchor"], "flag");
        assert_eq!(v["matches"][0]["status"]["kind"], "placed");
        assert_eq!(v["matches"][0]["degraded"][0]["kind"], "no_paths");
        assert_eq!(v["vod"]["duration_s"], 3600.0);
    }

    #[test]
    fn progress_reports_stages_and_matches() {
        let mut f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        f.cfg.progress = Some(Arc::new(move |p| s.lock().unwrap().push(format!("{p:?}"))));
        run(&f, 3600.0, &cluster(50.0, 10, 3)).unwrap();
        let seen = seen.lock().unwrap();
        let kinds: Vec<&str> = seen
            .iter()
            .map(|p| p.split([' ', '(']).next().unwrap())
            .collect();
        assert_eq!(kinds, ["Probed", "History", "Aligned", "Match"], "{seen:?}");
    }

    fn have_ffmpeg() -> bool {
        let ok = std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("skipping: ffmpeg not on PATH");
        }
        ok
    }

    #[test]
    fn clips_stream_copy_named_by_match_and_time() {
        if !have_ffmpeg() {
            return;
        }
        let mut f = fixture(&json!([hist(1, A, 1800, 1)]));
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10",
                "-t",
                "12",
            ])
            .args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p"])
            .arg(&f.cfg.vod)
            .status()
            .unwrap();
        assert!(st.success());
        f.cfg.clips_max = None;
        // Two own kills at game times 10 and 11: score 4, over the default threshold.
        cache_match_combat(&f, 1, 7, false, &[10, 11], &[]);
        let readings = vec![
            reading(1.0, "0:01"),
            reading(2.0, "0:02"),
            reading(3.0, "0:03"),
        ];
        let p = crate::video::probe(&f.cfg.vod).unwrap();
        let o = run_on(&f, &p, &readings).unwrap();
        assert_eq!(o.highlights.len(), 1);
        let h = &o.highlights[0];
        assert!(
            h.start.abs() < 1e-9 && (h.end - p.duration_s).abs() < 1e-9,
            "clamped: {h:?}"
        );
        let clip = f.cfg.out.join("clips/1-0.mp4");
        assert_eq!(o.clips, std::slice::from_ref(&clip));
        assert!(std::fs::metadata(&clip).unwrap().len() > 0);
        std::fs::write(&clip, b"kept").unwrap();
        f.api.push(
            "/v1/players/7/match-history",
            200,
            &json!([hist(1, A, 1800, 1)]).to_string(),
            None,
        );
        let o = run_on(&f, &p, &readings).unwrap();
        assert_eq!(o.clips, std::slice::from_ref(&clip));
        assert_eq!(std::fs::read(&clip).unwrap(), b"kept");
        f.cfg.clips_min_score = 5;
        f.api.push(
            "/v1/players/7/match-history",
            200,
            &json!([hist(1, A, 1800, 1)]).to_string(),
            None,
        );
        let o = run_on(&f, &p, &readings).unwrap();
        assert_eq!(o.clips, Vec::<PathBuf>::new());
        assert_eq!(std::fs::read(&clip).unwrap(), b"kept");
    }

    /// A recorded run on one VOD: clock readings, the account's match history, the
    /// metadata of its matches and the highlights found then. Not in git.
    #[test]
    fn recorded_run_places_12() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/recorded-run");
        if !src.is_dir() {
            eprintln!("skipping: {} is absent", src.display());
            return;
        }
        let read = |name: &str| -> Value {
            serde_json::from_str(&std::fs::read_to_string(src.join(name)).unwrap()).unwrap()
        };
        let readings: Vec<FrameReading> = read("hud.json")["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                reading(
                    r["vod_time"].as_f64().unwrap(),
                    r["clock"].as_str().unwrap(),
                )
            })
            .collect();
        let duration_s = readings.last().unwrap().pts_ms as f64 / 1000.0 + 30.0;
        let history = read("history.json");
        let account = u32::try_from(history[0]["account_id"].as_u64().unwrap()).unwrap();
        let mut f = fixture(&json!([]));
        f.cfg.account = account;
        f.cfg.vod_start = None;
        let api = FakeApi::with(&[]);
        api.push(
            &format!("/v1/players/{account}/match-history"),
            200,
            &history.to_string(),
            None,
        );
        for _ in 0..30 {
            api.push("/v1/players/steam", 500, "{}", None);
        }
        f.api = api;
        for e in std::fs::read_dir(src.join("matches")).unwrap() {
            let e = e.unwrap();
            let dir = f.cfg.out.join("matches").join(e.file_name());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(e.path().join("metadata.json"), dir.join("metadata.json")).unwrap();
        }
        let o = run(&f, duration_s, &readings).unwrap();
        let placed: Vec<&MatchReport> = o
            .matches
            .iter()
            .filter(|m| m.status == MatchStatus::Placed)
            .collect();
        assert_eq!(placed.len(), 12, "{:?}", o.matches);
        assert_eq!(o.matches.len(), 12);
        assert_eq!(o.unmatched_starts.len(), 1, "{:?}", o.unmatched_starts);
        // The first reading lands within a minute of the start's clock 0:00.
        assert!(
            (o.unmatched_starts[0].first_reading - 6660.0).abs() <= 60.0,
            "{:?}",
            o.unmatched_starts
        );
        assert!(
            !f.api
                .requests()
                .iter()
                .any(|p| p.starts_with("/v1/matches/")),
            "every match is cached"
        );

        let expected = read("expected-highlights.json");
        let expected_times = |id: u64| -> Vec<f64> {
            expected
                .as_array()
                .unwrap()
                .iter()
                .filter(|h| h["match_id"].as_u64() == Some(id))
                .flat_map(|h| {
                    h["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|e| e["vod_time"].as_f64().unwrap())
                })
                .collect()
        };
        let mut checked = 0;
        for h in &o.highlights {
            let theirs = expected_times(h.match_id);
            for e in &h.events {
                assert!(
                    theirs.iter().any(|t| (t - e.vod_time).abs() <= 2.0),
                    "match {} event at {} ({}) is not in the recorded highlights",
                    h.match_id,
                    e.vod_time,
                    e.label
                );
                checked += 1;
            }
        }
        eprintln!(
            "recorded run: {} highlights, {checked} events within 2 s of the recorded ones",
            o.highlights.len()
        );
        assert!(checked > 0);
    }
}
