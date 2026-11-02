//! The stages in order (§1), behind a seam that lets tests supply readings and the API.

use std::path::PathBuf;

use deadlock_api::blocking::Transport;

use crate::align::{HistMatch, Start, align_readings};
use crate::cache_key;
use crate::clock::ClockReading;
use crate::config::{Config, Progress};
use crate::error::{Error, Result};
use crate::fetch::{Fetched, Fetcher, is_cached};
use crate::gate::Gate;
use crate::history::load_history;
use crate::output::{
    Alignment, ClockSegment, Degraded, Highlight, MatchReport, MatchStatus, Output, SCHEMA,
    UnseenMatch, VodInfo,
};
use crate::place::{pause_mismatch, vod_start};
use crate::read::cache;
use crate::read::{
    FrameReading, HudReader, SystemClock, clock_readings, read_frames, require_clock,
};
use crate::score::{rank, timeline, windows};
use crate::video::{GRID_S, Probe, cut_clip, keyframes};

/// The note of every clock segment (§4 Align, no match).
const SEGMENT_NOTE: &str =
    "no history match: free map, hideout, or a match missing from the history";
/// History matches shorter than this are remakes, and not expected on the VOD.
const MIN_MATCH_S: u32 = 180;

/// Where the readings come from: the VOD's keyframes, or readings a test already has.
pub(crate) enum FrameSource {
    Vod(Probe),
    Readings(Probe, Vec<FrameReading>),
}

fn report(cfg: &Config, p: Progress) {
    if let Some(f) = &cfg.progress {
        f(p);
    }
}

/// Stages 2-9 after the probe; `transport` replaces the network in tests.
pub(crate) fn run_with(
    cfg: &Config,
    reader: &dyn HudReader,
    source: FrameSource,
    transport: Option<Box<dyn Transport>>,
) -> Result<Output> {
    let probe = match &source {
        FrameSource::Vod(p) | FrameSource::Readings(p, _) => p.clone(),
    };
    report(
        cfg,
        Progress::Probed {
            duration_s: probe.duration_s,
        },
    );
    std::fs::create_dir_all(&cfg.out).map_err(|source| Error::Io {
        path: cfg.out.clone(),
        source,
    })?;
    let api = crate::api::client(cfg, transport)?;
    let history = load_history(&api, cfg, &probe, &|w| report(cfg, Progress::Warning(w)))?;
    report(
        cfg,
        Progress::History {
            matches: history.matches.len(),
            anchor: history.anchor.map(|a| a.1),
        },
    );

    let frames = match source {
        FrameSource::Readings(_, r) => r,
        FrameSource::Vod(p) => read_vod(cfg, &p, reader)?,
    };
    require_clock(&frames, &cfg.profile)?;
    let mut hist = history.matches;
    // Assignment takes the latest match by start time.
    hist.sort_by_key(|m| m.start_time);
    let aligned = align_readings(&clock_readings(&frames), &hist, history.anchor.map(|a| a.0))?;
    let offset = aligned.offset;
    report(
        cfg,
        Progress::Aligned {
            offset,
            starts: aligned.starts.len(),
        },
    );

    // Starts come in VOD order, so matches keep the order of their first start.
    let mut groups: Vec<(u64, Vec<&Start>)> = Vec::new();
    let mut segments = Vec::new();
    for (start, m) in &aligned.starts {
        match m {
            Some(id) => match groups.iter_mut().find(|(g, _)| g == id) {
                Some((_, starts)) => starts.push(start),
                None => groups.push((*id, vec![start])),
            },
            None => segments.push(segment(start)),
        }
    }

    let (matches, mut highlights) = place_matches(cfg, &api, &hist, &groups, probe.duration_s);
    rank(&mut highlights);

    let unseen = unseen(&hist, &groups, offset, probe.duration_s);
    let newest = hist.last().map_or(i64::MIN, |h| h.start_time);
    let retryable = matches
        .iter()
        .any(|m| matches!(m.status, MatchStatus::Unavailable { .. } | MatchStatus::NotIngested))
        // The history may not list a match that started after its newest entry yet.
        || segments.iter().any(|s| s.vod_end + offset as f64 > newest as f64);
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
            score: aligned.score,
            starts: aligned.starts.len(),
        },
        matches,
        segments,
        unseen,
        retryable,
        highlights,
        clips,
    })
}

/// Fetches and places each assigned match once, in VOD order; returns its reports and windows.
fn place_matches(
    cfg: &Config,
    api: &deadlock_api::blocking::Client,
    hist: &[HistMatch],
    groups: &[(u64, Vec<&Start>)],
    vod_duration: f64,
) -> (Vec<MatchReport>, Vec<Highlight>) {
    let mut fetcher = Fetcher::new(api, cfg.account);
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
        match fetcher.fetch(h, is_cached(&cfg.out, *id)) {
            Fetched::Status(s) => m.status = s,
            Fetched::Ready(t) => {
                let readings: Vec<ClockReading> = starts
                    .iter()
                    .flat_map(|s| s.readings.iter().copied())
                    .collect();
                m.vod_start = vod_start(&readings, &t.pauses);
                if !t.events.has_paths {
                    m.degraded.push(Degraded::NoPaths);
                }
                let vod_s: Vec<i64> = starts.iter().map(|s| s.vod_s).collect();
                if let Some((gaps_s, pauses_s)) = pause_mismatch(&vod_s, &t.pauses) {
                    m.degraded
                        .push(Degraded::PauseMismatch { gaps_s, pauses_s });
                }
                if let Some(v) = m.vod_start {
                    let events = timeline(&t.events, v, &t.pauses, t.duration_s);
                    highlights.extend(windows(*id, &events, vod_duration));
                }
            }
        }
        report(cfg, Progress::Match(m.clone()));
        matches.push(m);
    }
    (matches, highlights)
}

/// History matches the VOD should show, inside its aligned span, that got no game start.
fn unseen(
    hist: &[HistMatch],
    groups: &[(u64, Vec<&Start>)],
    offset: i64,
    vod_duration: f64,
) -> Vec<UnseenMatch> {
    let span_end = offset + vod_duration.ceil() as i64;
    hist.iter()
        .filter(|h| h.duration_s >= MIN_MATCH_S && h.game_mode == Some(1))
        .filter(|h| h.start_time <= span_end && h.start_time + i64::from(h.duration_s) >= offset)
        .filter(|h| !groups.iter().any(|(id, _)| *id == h.match_id))
        .map(|h| UnseenMatch {
            match_id: h.match_id,
            start_time: h.start_time,
        })
        .collect()
}

fn segment(start: &Start) -> ClockSegment {
    let times = start.readings.iter().map(|r| r.vod_time);
    ClockSegment {
        vod_start: times.clone().fold(f64::INFINITY, f64::min),
        vod_end: times.fold(f64::NEG_INFINITY, f64::max),
        readings: start.readings.len(),
        note: SEGMENT_NOTE,
    }
}

/// The cached readings of this VOD and settings, or a fresh read that is then cached.
fn read_vod(cfg: &Config, probe: &Probe, reader: &dyn HudReader) -> Result<Vec<FrameReading>> {
    let key = cache_key(cfg, probe, reader);
    if let Some(r) = cache::load(&cfg.out, &key) {
        return Ok(r);
    }
    let frames = keyframes(&cfg.vod, probe, &cfg.profile, GRID_S)?;
    let gate = Gate::new();
    let r = read_frames(
        &frames,
        &|gray| gate.score(gray),
        cfg.profile.t,
        reader,
        cfg.parallel,
        &SystemClock::new(),
        &|done, total| report(cfg, Progress::Read { done, total }),
    )?;
    cache::store(&cfg.out, &key, &cfg.profile, &r)?;
    Ok(r)
}

/// Cuts the windows at or above the density threshold, best first; an existing clip is kept.
fn clips(cfg: &Config, ranked: &[Highlight]) -> Result<Vec<PathBuf>> {
    let ext = cfg
        .vod
        .extension()
        .map_or("mp4".into(), |e| e.to_string_lossy().into_owned());
    let dir = cfg.out.join("clips");
    let mut out = Vec::new();
    for h in ranked
        .iter()
        .filter(|h| h.density >= cfg.clips_min_density)
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
    use crate::read::{Outcome, ReadError, ReaderIdentity};
    use crate::test_support::{FakeApi, match_info_json, path, player};
    use serde_json::{Value, json};
    use std::sync::Arc;

    const A: i64 = 1_793_000_000;

    struct NoReader;

    impl HudReader for NoReader {
        fn identity(&self) -> ReaderIdentity {
            ReaderIdentity {
                protocol: "none",
                server: String::new(),
                model: String::new(),
                prompt: "",
            }
        }
        fn read(&self, _: &[u8]) -> std::result::Result<String, ReadError> {
            panic!("the reader is not used with readings")
        }
    }

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
        let mut cfg = Config::new(d.path().join("v.mp4"), 7);
        cfg.out = d.path().join("out");
        cfg.api_base_url = "http://127.0.0.1:9".into();
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
        let paths = paths.then(|| vec![path(1, &[1000; 1801], &[100; 1801])]);
        let info = match_info_json(
            1800,
            vec![
                player(me, 1, 0, &[]),
                player(8, 2, 1, &[(kill_at, Some(1), Some(1000.0))]),
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

    fn run(f: &Fixture, duration_s: f64, readings: Vec<FrameReading>) -> Result<Output> {
        run_with(
            &f.cfg,
            &NoReader,
            FrameSource::Readings(probe(duration_s), readings),
            Some(Box::new(f.api.clone())),
        )
    }

    #[test]
    fn placed_with_paths_is_not_degraded() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        let o = run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap();
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
        let o = run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap();
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
        let o = run(&f, 3600.0, r).unwrap();
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
    fn retryable_when_unavailable_or_late_segment() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        f.api.push("/v1/matches/1/metadata", 404, "{}", None);
        let o = run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::NotIngested);
        assert!(o.retryable);

        // A clock segment 50 min after the newest history match started.
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        let mut r = cluster(50.0, 10, 3);
        r.extend(cluster(3000.0, 10, 3));
        let o = run(&f, 3600.0, r).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::Placed);
        assert_eq!(o.segments.len(), 1);
        let s = &o.segments[0];
        assert_eq!((s.vod_start, s.vod_end, s.readings), (3010.0, 3070.0, 3));
        assert_eq!(
            s.note,
            "no history match: free map, hideout, or a match missing from the history"
        );
        assert!(o.retryable);
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
        let o = run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap();
        let unseen: Vec<u64> = o.unseen.iter().map(|u| u.match_id).collect();
        assert_eq!(unseen, [2]);
    }

    #[test]
    fn a_match_with_two_starts_is_fetched_once() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        f.api.push("/v1/matches/1/metadata", 404, "{}", None);
        // Two starts 100 s apart, both in match 1.
        let mut r = cluster(50.0, 10, 5);
        r.extend(cluster(150.0, 900, 3));
        let o = run(&f, 3600.0, r).unwrap();
        assert_eq!(o.alignment.starts, 2);
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
    fn exit_code_0_placed_3_none() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, true, 300);
        assert_eq!(
            run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap().exit_code(),
            0
        );
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 9, true, 300);
        let o = run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap();
        assert_eq!(o.matches[0].status, MatchStatus::NotYours);
        assert_eq!(o.exit_code(), 3);
    }

    #[test]
    fn output_serialises_field_names() {
        let f = fixture(&json!([hist(1, A + 50, 1800, 1)]));
        cache_match(&f, 1, 7, false, 300);
        let v = serde_json::to_value(run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap()).unwrap();
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
        run(&f, 3600.0, cluster(50.0, 10, 3)).unwrap();
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
        // A lone own kill at game time 10: density 3 / 10 = 0.30, the default threshold.
        cache_match(&f, 1, 7, false, 10);
        let readings = vec![
            reading(1.0, "0:01"),
            reading(2.0, "0:02"),
            reading(3.0, "0:03"),
        ];
        let p = crate::video::probe(&f.cfg.vod).unwrap();
        let src = || FrameSource::Readings(p.clone(), readings.clone());
        let o = run_with(&f.cfg, &NoReader, src(), Some(Box::new(f.api.clone()))).unwrap();
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
        let o = run_with(&f.cfg, &NoReader, src(), Some(Box::new(f.api.clone()))).unwrap();
        assert_eq!(o.clips, std::slice::from_ref(&clip));
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
        let o = run(&f, duration_s, readings).unwrap();
        let placed: Vec<&MatchReport> = o
            .matches
            .iter()
            .filter(|m| m.status == MatchStatus::Placed)
            .collect();
        assert_eq!(placed.len(), 12, "{:?}", o.matches);
        assert_eq!(o.matches.len(), 12);
        assert_eq!(o.segments.len(), 1, "{:?}", o.segments);
        assert!(
            (o.segments[0].vod_start - 6660.0).abs() <= 60.0,
            "{:?}",
            o.segments
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
