//! `dlhl`: highlight windows and clips of a Deadlock VOD.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use dlhl::{AnchorSource, Config, Error, Highlight, MatchStatus, Output, Profile, Progress};

/// Finds the fights of a Deadlock VOD from the game clock on screen and deadlock-api.com
/// match data, and cuts the densest ones as clips.
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
    /// `DLHL_OPENAI_API_KEY`.
    #[arg(long, default_value = "http://localhost:11434/v1")]
    server: String,
    #[arg(long, default_value = "gemma4:e4b")]
    model: String,
    /// Concurrent requests to the chat server.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    parallel: u16,
    /// Windows at or above this density are cut as clips and listed here.
    #[arg(long, default_value_t = 0.30)]
    clips_min_density: f64,
    /// At most this many clips, best first.
    #[arg(long)]
    clips_max: Option<usize>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match cli(args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("dlhl: {e}");
            ExitCode::from(if matches!(e, Error::Usage(_)) { 2 } else { 1 })
        }
    }
}

fn cli(args: Args) -> Result<u8, Error> {
    let mut cfg = Config::new(&args.vod, args.account);
    if let Some(out) = args.out {
        cfg.out = out;
    }
    cfg.vod_start = args
        .vod_start
        .as_deref()
        .map(Config::parse_vod_start)
        .transpose()?;
    if let Some(p) = &args.profile {
        cfg.profile = Profile::load(p)?;
    }
    cfg.server = args.server;
    cfg.model = args.model;
    cfg.openai_api_key = std::env::var("DLHL_OPENAI_API_KEY").ok();
    cfg.deadlock_api_key = std::env::var("DEADLOCK_API_KEY").ok();
    cfg.parallel = usize::from(args.parallel);
    cfg.clips_min_density = args.clips_min_density;
    cfg.clips_max = args.clips_max;
    cfg.progress = Some(Arc::new(|p| {
        if let Some(line) = progress_line(&p) {
            eprintln!("{line}");
        }
    }));

    // A failed run must not leave another run's result beside the VOD ([R17]).
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
    print_table(&output, cfg.clips_min_density, &file);
    if output.retryable {
        eprintln!(
            "a rerun may place more: see `retryable` in {}",
            file.display()
        );
    }
    Ok(output.exit_code())
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

fn summary(h: &Highlight) -> String {
    format!(
        "{}K {}A {}D, {} deaths, {} obj",
        h.my_kills, h.my_assists, h.my_deaths, h.deaths, h.objectives
    )
}

fn print_table(o: &Output, min_density: f64, file: &Path) {
    let shown: Vec<&Highlight> = o
        .highlights
        .iter()
        .filter(|h| h.density >= min_density)
        .collect();
    let mut w = std::io::stdout().lock();
    if !shown.is_empty() {
        let _ = writeln!(
            w,
            "{:>4}  {:>8}  {:>5}  {:>7}  {:>5}  {:>10}  events",
            "rank", "start", "len s", "density", "score", "match"
        );
    }
    for (i, h) in shown.iter().enumerate() {
        let _ = writeln!(
            w,
            "{:>4}  {:>8}  {:>5.0}  {:>7.2}  {:>5}  {:>10}  {}",
            i + 1,
            hms(h.start),
            h.end - h.start,
            h.density,
            h.score,
            h.match_id,
            summary(h)
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
        Progress::Aligned { offset, starts } => {
            format!("aligned: VOD 0:00 at unix {offset}, {starts} game starts")
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
