//! `dlhl`: highlight windows and clips of a Deadlock VOD.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use dlhl::{AnchorSource, Config, Error, Highlight, MatchStatus, Output, Profile, Progress};

/// Finds the fights of a Deadlock VOD from the game clock on screen and deadlock-api.com
/// match data, and cuts those with the most own kills and assists as clips.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// The VOD file.
    vod: PathBuf,
    /// The streamer's 32-bit Steam account id.
    #[arg(long)]
    account: u32,
    /// Output directory [default: <VOD>.dlhl].
    #[arg(long)]
    out: Option<PathBuf>,
    /// The VOD's wall-clock start: unix seconds, or RFC 3339 with an offset.
    #[arg(long)]
    vod_start: Option<String>,
    /// A HUD profile file for another clock position.
    #[arg(long)]
    profile: Option<PathBuf>,
    /// Base URL of an OpenAI-compatible chat server; its key is read from
    /// `DLHL_SERVER_API_KEY`.
    #[arg(long)]
    server: String,
    /// A vision model the server serves.
    #[arg(long)]
    model: String,
    /// Concurrent requests to the chat server.
    #[arg(long, default_value_t = Config::DEFAULT_PARALLEL, value_parser = clap::value_parser!(u16).range(1..))]
    parallel: u16,
    /// Windows scoring at least this (2 per own kill, 1 per assist) are cut as clips and
    /// listed here.
    #[arg(long, default_value_t = Config::DEFAULT_CLIPS_MIN_SCORE)]
    clips_min_score: u32,
    /// At most this many clips, best first.
    #[arg(long)]
    clips_max: Option<usize>,
}

/// The run completed but placed no match; the reasons are in `highlights.json`.
const EXIT_NONE_PLACED: u8 = 3;

fn main() -> ExitCode {
    let args = Args::parse();
    match cli(args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("dlhl: {}", message(&e));
            ExitCode::from(error_code(&e))
        }
    }
}

fn error_code(e: &Error) -> u8 {
    if matches!(e, Error::Usage(_) | Error::InsecureKey { .. }) {
        2
    } else {
        1
    }
}

/// The flag that may get a failed run further.
fn hint(e: &Error) -> Option<&'static str> {
    match e {
        Error::NoClock { .. } => Some("pass --profile for another HUD layout"),
        Error::OffsetRejected { .. } => Some("pass --vod-start with the VOD's start time"),
        _ => None,
    }
}

fn message(e: &Error) -> String {
    match (e, hint(e)) {
        (Error::InsecureKey { server }, _) => format!(
            "DLHL_SERVER_API_KEY is only sent over https or to a loopback server, not to {server}"
        ),
        (_, Some(h)) => format!("{e}; {h}"),
        (_, None) => e.to_string(),
    }
}

fn vod_start(s: &str) -> Result<i64, Error> {
    Config::parse_vod_start(s).map_err(|e| Error::Usage(format!("--vod-start {e}")))
}

fn cli(args: Args) -> Result<u8, Error> {
    let mut cfg = Config::new(&args.vod, args.account, args.server, args.model);
    if let Some(out) = args.out {
        cfg.out = out;
    }
    cfg.vod_start = args.vod_start.as_deref().map(vod_start).transpose()?;
    if let Some(p) = &args.profile {
        cfg.profile = Profile::load(p)?;
    }
    cfg.server_api_key = std::env::var("DLHL_SERVER_API_KEY").ok();
    cfg.deadlock_api_key = std::env::var("DEADLOCK_API_KEY").ok();
    cfg.parallel = args.parallel;
    cfg.clips_min_score = args.clips_min_score;
    cfg.clips_max = args.clips_max;
    cfg.progress = Some(Arc::new(|p| {
        if let Some(line) = progress_line(&p) {
            eprintln!("{line}");
        }
    }));

    // A failed run must not leave another run's result beside the VOD.
    let file = cfg.out.join("highlights.json");
    match std::fs::remove_file(&file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(Error::Io {
                path: file,
                source: e,
            });
        }
        _ => {}
    }
    let output = dlhl::run(&cfg)?;
    write_atomic(&file, &output)?;
    print_table(&output, cfg.clips_min_score, &file);
    if output.retryable {
        eprintln!(
            "a rerun may place more: see `retryable` in {}",
            file.display()
        );
    }
    let placed = output
        .matches
        .iter()
        .any(|m| m.status == MatchStatus::Placed);
    Ok(if placed { 0 } else { EXIT_NONE_PLACED })
}

fn write_atomic(file: &Path, output: &Output) -> Result<(), Error> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io { path, source }
    };
    let body = serde_json::to_vec_pretty(output).expect("the output serialises");
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(io(&tmp))?;
    std::fs::rename(&tmp, file).map_err(io(file))
}

fn hms(s: f64) -> String {
    let s = s.max(0.0).floor() as u64;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn event_counts(h: &Highlight) -> String {
    format!(
        "{}K {}A {}D, {} deaths, {} obj",
        h.my_kills, h.my_assists, h.my_deaths, h.deaths, h.objectives
    )
}

fn print_table(o: &Output, min_score: u32, file: &Path) {
    let shown: Vec<&Highlight> = o
        .highlights
        .iter()
        .filter(|h| h.score >= min_score)
        .collect();
    let mut w = std::io::stdout().lock();
    if !shown.is_empty() {
        let _ = writeln!(
            w,
            "{:>4}  {:>8}  {:>5}  {:>8}  {:>5}  {:>10}  events",
            "rank", "start", "len s", "combat s", "score", "match"
        );
    }
    for (i, h) in shown.iter().enumerate() {
        let _ = writeln!(
            w,
            "{:>4}  {:>8}  {:>5.0}  {:>8}  {:>5}  {:>10}  {}",
            i + 1,
            hms(h.start),
            h.end - h.start,
            h.combat_s,
            h.score,
            h.match_id,
            event_counts(h)
        );
    }
    let _ = writeln!(
        w,
        "{} more windows in {}",
        o.highlights.len() - shown.len(),
        file.display()
    );
}

fn progress_line(p: &Progress) -> Option<String> {
    Some(match p {
        Progress::Probed { duration_s } => format!("VOD: {}", hms(*duration_s)),
        Progress::History { matches, anchor } => format!(
            "history: {matches} matches; VOD start from {}",
            match anchor {
                Some(AnchorSource::Flag) => "--vod-start",
                Some(AnchorSource::InfoJson) => "info.json",
                Some(_) => "another source",
                None => "the clock alone",
            }
        ),
        // One line per 50 frames, and the last.
        Progress::Read { done, total } if done % 50 == 0 || done == total => {
            format!("read {done}/{total} frames")
        }
        Progress::Read { .. } => return None,
        Progress::Aligned {
            offset,
            start_count,
        } => {
            format!("aligned: VOD 0:00 at unix {offset}, {start_count} game starts")
        }
        Progress::Match(m) => {
            let status = match &m.status {
                MatchStatus::Placed => "placed".to_owned(),
                MatchStatus::Remake => "remake, skipped".to_owned(),
                MatchStatus::UnsupportedMode { game_mode } => {
                    format!("game mode {game_mode:?} not supported")
                }
                MatchStatus::NotYours => "the account is not in this match".to_owned(),
                MatchStatus::NotIngested => "not in deadlock-api yet".to_owned(),
                MatchStatus::Unavailable { retry_after_s } => match retry_after_s {
                    Some(s) => format!("unavailable, retry after {s} s"),
                    None => "unavailable".to_owned(),
                },
                MatchStatus::DecodeFailed { message } => message.clone(),
                _ => format!("{:?}", m.status),
            };
            let degraded = if m.degraded.is_empty() {
                String::new()
            } else {
                format!(" (degraded: {:?})", m.degraded)
            };
            format!("match {}: {status}{degraded}", m.match_id)
        }
        Progress::Warning(w) => format!("warning: {w}"),
        _ => format!("{p:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_name_the_flag_that_fixes_the_error() {
        let no_clock = Error::NoClock {
            band: "b".into(),
            best_gate: 0.1,
        };
        assert_eq!(
            hint(&no_clock),
            Some("pass --profile for another HUD layout")
        );
        let rejected = Error::OffsetRejected {
            offset: None,
            hits: 1,
            start_count: 4,
            anchored: false,
        };
        assert_eq!(
            hint(&rejected),
            Some("pass --vod-start with the VOD's start time")
        );
        assert_eq!(hint(&Error::Usage("x".into())), None);
        assert!(message(&no_clock).ends_with("; pass --profile for another HUD layout"));
    }

    #[test]
    fn insecure_key_message_names_the_variable() {
        let e = Error::InsecureKey {
            server: "http://10.0.0.5:1/v1".into(),
        };
        assert_eq!(
            message(&e),
            "DLHL_SERVER_API_KEY is only sent over https or to a loopback server, not to http://10.0.0.5:1/v1"
        );
        assert_eq!(error_code(&e), 2);
    }

    #[test]
    fn flag_defaults_are_the_library_defaults() {
        let a = Args::try_parse_from([
            "dlhl",
            "v",
            "--account",
            "1",
            "--server",
            "s",
            "--model",
            "m",
        ])
        .unwrap();
        assert_eq!(a.parallel, Config::DEFAULT_PARALLEL);
        assert_eq!(a.clips_min_score, Config::DEFAULT_CLIPS_MIN_SCORE);
    }

    #[test]
    fn clips_min_score_flag_parses() {
        let a = Args::try_parse_from([
            "dlhl",
            "v",
            "--account",
            "1",
            "--server",
            "s",
            "--model",
            "m",
            "--clips-min-score",
            "5",
        ])
        .unwrap();
        assert_eq!(a.clips_min_score, 5);
    }

    #[test]
    fn error_codes() {
        assert_eq!(error_code(&Error::Usage("x".into())), 2);
        assert_eq!(error_code(&Error::Reader("x".into())), 1);
    }

    #[test]
    fn vod_start_error_names_the_flag() {
        let e = vod_start("2026-11-02T18:00:00").unwrap_err();
        assert!(e.to_string().starts_with("--vod-start "), "{e}");
    }

    #[test]
    fn hms_floors_and_pads() {
        assert_eq!(hms(0.0), "0:00:00");
        assert_eq!(hms(3661.9), "1:01:01");
        assert_eq!(hms(-3.0), "0:00:00");
    }

    #[test]
    fn read_progress_is_thinned() {
        let line = |done| progress_line(&Progress::Read { done, total: 120 });
        assert_eq!(line(49), None);
        assert_eq!(line(50).as_deref(), Some("read 50/120 frames"));
        assert_eq!(line(120).as_deref(), Some("read 120/120 frames"));
    }
}
