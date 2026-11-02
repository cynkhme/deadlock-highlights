//! The clock read on every keyframe that passes the gate.

pub(crate) mod cache;
pub(crate) mod openai;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::{ClockReading, parse_answer};
use crate::error::{Error, Result};
use crate::profile::Profile;
use crate::video::Frame;

pub(crate) const MAX_FAILED_READ_SHARE: f64 = 0.1;
/// With no read succeeding for this long, the server is stuck or too slow to ever finish.
pub(crate) const MAX_READ_STALL: Duration = Duration::from_secs(300);
const RETRY_PAUSE: Duration = Duration::from_secs(1);
const MAX_ATTEMPTS: u32 = 3;

/// What a reader is, for the cache key: a change of any field means other readings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ReaderIdentity {
    pub protocol: &'static str,
    pub server: String,
    pub model: String,
    pub prompt: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReadError {
    /// The server did not take the request (connection error, 500, 502-504).
    Retryable(String),
    /// 429, with the server's `retry-after` when it sent one.
    RateLimited(Option<Duration>),
    /// Any other refusal; the request will not succeed as it is.
    Fatal(String),
}

/// Reads the clock off one band. Values are not validated here.
pub(crate) trait HudReader: Send + Sync {
    fn identity(&self) -> ReaderIdentity;
    /// The raw answer text.
    fn read(&self, png: &[u8]) -> std::result::Result<String, ReadError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The raw answer; a clock is taken from it by `parse_answer`, so validation can change
    /// without a new read.
    Read {
        raw: String,
    },
    NoHud,
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FrameReading {
    pub pts_ms: u64,
    pub gate: f32,
    pub outcome: Outcome,
}

/// Time and sleep, replaceable in tests.
pub(crate) trait Clock: Sync {
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

pub(crate) struct SystemClock(std::time::Instant);

impl SystemClock {
    pub(crate) fn new() -> SystemClock {
        SystemClock(std::time::Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

struct Shared<'a> {
    reader: &'a dyn HudReader,
    clock: &'a dyn Clock,
    last_success: std::sync::Mutex<Duration>,
    any_success: std::sync::atomic::AtomicBool,
    abort: std::sync::Mutex<Option<Error>>,
}

impl Shared<'_> {
    fn aborted(&self) -> bool {
        self.abort.lock().expect("abort lock").is_some()
    }

    fn fail(&self, e: Error) {
        self.abort.lock().expect("abort lock").get_or_insert(e);
    }

    fn stalled(&self) -> bool {
        let last = *self.last_success.lock().expect("stall lock");
        self.clock.now().saturating_sub(last) > MAX_READ_STALL
    }

    /// `None` when the run was aborted.
    fn read_one(&self, png: &[u8]) -> Option<Outcome> {
        use std::sync::atomic::Ordering;
        let mut attempts = 0;
        loop {
            if self.aborted() {
                return None;
            }
            if self.stalled() {
                self.fail(Error::Reader(format!(
                    "model server stalled: no read succeeded for {} min",
                    MAX_READ_STALL.as_secs() / 60
                )));
                return None;
            }
            match self.reader.read(png) {
                Ok(raw) => {
                    self.any_success.store(true, Ordering::SeqCst);
                    *self.last_success.lock().expect("stall lock") = self.clock.now();
                    return Some(Outcome::Read { raw });
                }
                Err(ReadError::RateLimited(wait)) => self.clock.sleep(wait.unwrap_or(RETRY_PAUSE)),
                Err(ReadError::Retryable(e)) => {
                    attempts += 1;
                    if attempts >= MAX_ATTEMPTS {
                        return Some(Outcome::Failed { error: e });
                    }
                    self.clock.sleep(RETRY_PAUSE);
                }
                Err(ReadError::Fatal(e)) => {
                    if !self.any_success.load(Ordering::SeqCst) {
                        self.fail(Error::Reader(format!(
                            "the model server refused the first request: {e}"
                        )));
                        return None;
                    }
                    return Some(Outcome::Failed { error: e });
                }
            }
        }
    }
}

/// Gates and reads every frame, `parallel` requests at a time; readings in pts order.
pub(crate) fn read_frames(
    frames: &[Frame],
    gate: &(dyn Fn(&[u8]) -> f32 + Sync),
    t: f32,
    reader: &dyn HudReader,
    parallel: usize,
    clock: &dyn Clock,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<Vec<FrameReading>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let shared = Shared {
        reader,
        clock,
        last_success: std::sync::Mutex::new(clock.now()),
        any_success: false.into(),
        abort: std::sync::Mutex::new(None),
    };
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<FrameReading>> =
        std::sync::Mutex::new(Vec::with_capacity(frames.len()));
    std::thread::scope(|s| {
        for _ in 0..parallel.max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(f) = frames.get(i) else { break };
                    let score = gate(&f.gray);
                    let outcome = if score < t {
                        Outcome::NoHud
                    } else {
                        match shared.read_one(&f.png) {
                            Some(o) => o,
                            None => break,
                        }
                    };
                    let r = FrameReading {
                        pts_ms: f.pts_ms,
                        gate: score,
                        outcome,
                    };
                    results.lock().expect("results lock").push(r);
                    progress(done.fetch_add(1, Ordering::SeqCst) + 1, frames.len());
                }
            });
        }
    });
    if let Some(e) = shared.abort.into_inner().expect("abort lock") {
        return Err(e);
    }
    let mut r = results.into_inner().expect("results lock");
    r.sort_by_key(|f| f.pts_ms);
    if let Some(failed) = too_many_failed(&r) {
        return Err(Error::Reader(format!(
            "{failed} of {} reads failed",
            r.len()
        )));
    }
    Ok(r)
}

/// The failed count, when it is above [`MAX_FAILED_READ_SHARE`] of the reads.
pub(crate) fn too_many_failed(r: &[FrameReading]) -> Option<usize> {
    let failed = r
        .iter()
        .filter(|f| matches!(f.outcome, Outcome::Failed { .. }))
        .count();
    (failed as f64 > MAX_FAILED_READ_SHARE * r.len() as f64).then_some(failed)
}

/// The readings with a valid clock.
pub(crate) fn clock_readings(r: &[FrameReading]) -> Vec<ClockReading> {
    r.iter()
        .filter_map(|f| match &f.outcome {
            Outcome::Read { raw } => Some(ClockReading {
                vod_time: f.pts_ms as f64 / 1000.0,
                clock_s: parse_answer(raw)?,
            }),
            _ => None,
        })
        .collect()
}

/// Fails when no frame showed a clock, naming the band and the best gate score.
pub(crate) fn require_clock(r: &[FrameReading], profile: &Profile) -> Result<()> {
    if clock_readings(r).is_empty() {
        let best_gate = r.iter().map(|f| f.gate).fold(-1.0_f32, f32::max);
        return Err(Error::NoClock {
            band: format!(
                "360x64 at center_x={}, top={}",
                profile.band.center_x, profile.band.top
            ),
            best_gate,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Virtual time: `sleep` advances it, nothing else does unless a test says so.
    #[derive(Default)]
    struct FakeClock(Mutex<Duration>);

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap()
        }
        fn sleep(&self, d: Duration) {
            *self.0.lock().unwrap() += d;
        }
    }

    type Answer = std::result::Result<String, ReadError>;

    /// Scripted answers per frame (the PNG is the pts); unscripted frames read 1:00.
    #[derive(Default)]
    struct Fake {
        script: Mutex<HashMap<u64, VecDeque<Answer>>>,
        calls: Mutex<Vec<u64>>,
        jitter: bool,
        advance: Option<(&'static FakeClock, Duration)>,
    }

    impl Fake {
        fn with(script: &[(u64, Vec<Answer>)]) -> Fake {
            Fake {
                script: Mutex::new(
                    script
                        .iter()
                        .map(|(k, v)| (*k, v.iter().cloned().collect()))
                        .collect(),
                ),
                ..Fake::default()
            }
        }
        fn calls_for(&self, pts: u64) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|&&p| p == pts)
                .count()
        }
    }

    impl HudReader for Fake {
        fn identity(&self) -> ReaderIdentity {
            ReaderIdentity {
                protocol: "fake",
                server: String::new(),
                model: String::new(),
                prompt: "",
            }
        }
        fn read(&self, png: &[u8]) -> Answer {
            let pts = u64::from_le_bytes(png.try_into().unwrap());
            self.calls.lock().unwrap().push(pts);
            if self.jitter {
                std::thread::sleep(Duration::from_millis(pts / 30_000 * 7 % 5));
            }
            if let Some((clock, d)) = self.advance {
                clock.sleep(d);
            }
            let next = self
                .script
                .lock()
                .unwrap()
                .get_mut(&pts)
                .and_then(VecDeque::pop_front);
            next.unwrap_or_else(|| Ok(r#"{"clock":"1:00"}"#.into()))
        }
    }

    /// Frames at 0, 30 000, …; the gray band's first byte / 100 is its gate score.
    fn frames(n: u64, failing_gate: &[u64]) -> Vec<Frame> {
        (0..n)
            .map(|i| {
                let pts_ms = i * 30_000;
                let g = if failing_gate.contains(&pts_ms) {
                    10
                } else {
                    90
                };
                Frame {
                    pts_ms,
                    gray: vec![g],
                    png: pts_ms.to_le_bytes().to_vec(),
                }
            })
            .collect()
    }

    fn gate(gray: &[u8]) -> f32 {
        f32::from(gray[0]) / 100.0
    }

    fn read(
        fs: &[Frame],
        r: &dyn HudReader,
        parallel: usize,
        clock: &dyn Clock,
    ) -> Result<Vec<FrameReading>> {
        read_frames(fs, &gate, 0.40, r, parallel, clock, &|_, _| {})
    }

    fn retryable() -> Answer {
        Err(ReadError::Retryable("503".into()))
    }

    #[test]
    fn gate_fail_is_no_hud_without_call() {
        let f = Fake::default();
        let r = read(&frames(3, &[30_000]), &f, 1, &FakeClock::default()).unwrap();
        assert_eq!(r[1].outcome, Outcome::NoHud);
        assert!((r[1].gate - 0.10).abs() < 1e-6);
        assert_eq!(f.calls_for(30_000), 0);
        assert_eq!(clock_readings(&r).len(), 2);
    }

    #[test]
    fn validation_accepts_garbage_as_empty_read() {
        let f = Fake::with(&[
            (0, vec![Ok("I see nothing".into())]),
            (30_000, vec![Ok("{}".into())]),
            (60_000, vec![Ok("[]".into())]),
        ]);
        let r = read(&frames(4, &[]), &f, 1, &FakeClock::default()).unwrap();
        assert_eq!(
            r[0].outcome,
            Outcome::Read {
                raw: "I see nothing".into()
            }
        );
        assert_eq!(r[2].outcome, Outcome::Read { raw: "[]".into() });
        let c = clock_readings(&r);
        assert_eq!(
            c,
            [ClockReading {
                vod_time: 90.0,
                clock_s: 60
            }]
        );
    }

    #[test]
    fn retryable_then_ok() {
        let f = Fake::with(&[(0, vec![retryable(), retryable()])]);
        let clock = FakeClock::default();
        let r = read(&frames(1, &[]), &f, 1, &clock).unwrap();
        assert!(matches!(r[0].outcome, Outcome::Read { .. }));
        assert_eq!(f.calls_for(0), 3);
        assert_eq!(clock.now(), Duration::from_secs(2));
    }

    #[test]
    fn three_retryables_is_failed() {
        let f = Fake::with(&[(0, vec![retryable(), retryable(), retryable()])]);
        let r = read(&frames(20, &[]), &f, 1, &FakeClock::default()).unwrap();
        assert_eq!(
            r[0].outcome,
            Outcome::Failed {
                error: "503".into()
            }
        );
        assert_eq!(f.calls_for(0), 3);
    }

    #[test]
    fn rate_limit_waits_without_spending_an_attempt() {
        let rl = || Err(ReadError::RateLimited(Some(Duration::from_secs(7))));
        let f = Fake::with(&[(0, vec![rl(), rl(), rl(), retryable(), retryable()])]);
        let clock = FakeClock::default();
        let r = read(&frames(1, &[]), &f, 1, &clock).unwrap();
        assert!(matches!(r[0].outcome, Outcome::Read { .. }));
        assert_eq!(clock.now(), Duration::from_secs(21 + 2));
    }

    #[test]
    fn first_request_fatal_aborts() {
        let f = Fake::with(&[(0, vec![Err(ReadError::Fatal("400: unknown field".into()))])]);
        let e = read(&frames(3, &[]), &f, 1, &FakeClock::default()).unwrap_err();
        assert!(
            matches!(&e, Error::Reader(m) if m.contains("400: unknown field")),
            "{e}"
        );
        assert_eq!(f.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn later_fatal_is_failed() {
        let f = Fake::with(&[(30_000, vec![Err(ReadError::Fatal("413: too large".into()))])]);
        let r = read(&frames(20, &[]), &f, 1, &FakeClock::default()).unwrap();
        assert_eq!(
            r[1].outcome,
            Outcome::Failed {
                error: "413: too large".into()
            }
        );
    }

    #[test]
    fn failed_share_over_10_percent_errors() {
        let three = || vec![retryable(), retryable(), retryable()];
        let one = Fake::with(&[(0, three())]);
        assert!(read(&frames(10, &[]), &one, 1, &FakeClock::default()).is_ok());
        let two = Fake::with(&[(0, three()), (30_000, three())]);
        let e = read(&frames(10, &[]), &two, 1, &FakeClock::default()).unwrap_err();
        assert!(
            matches!(&e, Error::Reader(m) if m == "2 of 10 reads failed"),
            "{e}"
        );
    }

    #[test]
    fn stall_after_300s_errors() {
        static CLOCK: std::sync::LazyLock<FakeClock> = std::sync::LazyLock::new(FakeClock::default);
        let f = Fake {
            advance: Some((&CLOCK, Duration::from_secs(120))),
            ..Fake::with(&[
                (0, vec![retryable(), retryable(), retryable()]),
                (30_000, vec![retryable(), retryable()]),
            ])
        };
        let e = read(&frames(10, &[]), &f, 1, &*CLOCK).unwrap_err();
        assert!(
            matches!(&e, Error::Reader(m) if m.contains("stalled")),
            "{e}"
        );
    }

    #[test]
    fn parallel_4_and_1_same_readings() {
        let make = || Fake {
            jitter: true,
            ..Fake::with(&[
                (90_000, vec![Ok("garbage".into())]),
                (300_000, vec![retryable()]),
            ])
        };
        let fs = frames(40, &[60_000, 600_000]);
        let one = read(&fs, &make(), 1, &FakeClock::default()).unwrap();
        let four = read(&fs, &make(), 4, &FakeClock::default()).unwrap();
        assert_eq!(one, four);
        assert!(one.windows(2).all(|w| w[0].pts_ms < w[1].pts_ms));
    }

    #[test]
    fn progress_counts_every_frame() {
        let done = AtomicUsize::new(0);
        let f = Fake::default();
        read_frames(
            &frames(5, &[0]),
            &gate,
            0.40,
            &f,
            2,
            &FakeClock::default(),
            &|d, total| {
                assert_eq!(total, 5);
                done.fetch_max(d, Ordering::SeqCst);
            },
        )
        .unwrap();
        assert_eq!(done.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn no_clock_error_names_profile() {
        let r = vec![
            FrameReading {
                pts_ms: 0,
                gate: 0.21,
                outcome: Outcome::NoHud,
            },
            FrameReading {
                pts_ms: 30_000,
                gate: 0.45,
                outcome: Outcome::Read { raw: "{}".into() },
            },
        ];
        let e = require_clock(&r, &Profile::default()).unwrap_err();
        let m = e.to_string();
        assert!(
            m.contains("--profile") && m.contains("0.45") && m.contains("center_x=0.5"),
            "{m}"
        );
        let ok = vec![FrameReading {
            pts_ms: 0,
            gate: 0.5,
            outcome: Outcome::Read {
                raw: r#"{"clock":"0:09"}"#.into(),
            },
        }];
        assert!(require_clock(&ok, &Profile::default()).is_ok());
    }
}
