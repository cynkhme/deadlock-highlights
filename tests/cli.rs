//! Usage errors and failures before the first request: nothing here reaches a network.

use std::process::{Command, Output};

/// The binary with `args` and no key in its environment.
fn bare(args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dlhl"));
    c.args(args)
        .env_remove("DLHL_SERVER_API_KEY")
        .env_remove("DEADLOCK_API_KEY");
    c
}

/// The binary with `args` and a model; no test reaches the server it names.
fn dlhl(args: &[&str]) -> Command {
    let mut c = bare(args);
    c.args(["--model", "m"]);
    c
}

fn check(mut c: Command, code: i32, needle: &str) -> Output {
    let o = c.output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(code), "stderr: {err}");
    assert!(err.contains(needle), "{needle:?} not in stderr: {err}");
    o
}

#[test]
fn missing_account_is_2() {
    check(
        dlhl(&["v.mp4", "--server", "http://127.0.0.1:9/v1"]),
        2,
        "--account",
    );
}

#[test]
fn missing_server_is_2() {
    check(dlhl(&["v.mp4", "--account", "1"]), 2, "--server");
}

#[test]
fn missing_model_is_2() {
    check(
        bare(&[
            "v.mp4",
            "--account",
            "1",
            "--server",
            "http://127.0.0.1:9/v1",
        ]),
        2,
        "--model",
    );
}

#[test]
fn vod_start_without_zone_is_2() {
    check(
        dlhl(&[
            "v.mp4",
            "--account",
            "1",
            "--vod-start",
            "2026-11-02T18:00:00",
            "--server",
            "http://127.0.0.1:9/v1",
        ]),
        2,
        "RFC 3339",
    );
}

#[test]
fn zero_parallel_is_2() {
    check(
        dlhl(&[
            "v.mp4",
            "--account",
            "1",
            "--parallel",
            "0",
            "--server",
            "http://127.0.0.1:9/v1",
        ]),
        2,
        "--parallel",
    );
}

#[test]
fn missing_profile_is_2() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("nope.json");
    check(
        dlhl(&[
            "v.mp4",
            "--account",
            "1",
            "--profile",
            p.to_str().unwrap(),
            "--server",
            "http://127.0.0.1:9/v1",
        ]),
        2,
        "nope.json",
    );
}

#[test]
fn key_with_plain_http_remote_is_2() {
    let mut c = dlhl(&[
        "/nonexistent/v.mp4",
        "--account",
        "1",
        "--server",
        "http://10.0.0.5:1/v1",
    ]);
    c.env("DLHL_SERVER_API_KEY", "x");
    check(
        c,
        2,
        "DLHL_SERVER_API_KEY is only sent over https or to a loopback server",
    );
}

#[test]
fn missing_vod_is_1() {
    let d = tempfile::tempdir().unwrap();
    let v = d.path().join("absent.mp4");
    let o = check(
        dlhl(&[
            v.to_str().unwrap(),
            "--account",
            "1",
            "--server",
            "http://127.0.0.1:9/v1",
        ]),
        1,
        "absent.mp4",
    );
    assert!(!d.path().join("absent.mp4.dlhl").exists(), "{o:?}");
}

#[test]
fn no_ffmpeg_is_1() {
    let d = tempfile::tempdir().unwrap();
    let v = d.path().join("v.mp4");
    std::fs::write(&v, b"not a video").unwrap();
    let empty = d.path().join("bin");
    std::fs::create_dir(&empty).unwrap();
    let mut c = dlhl(&[
        v.to_str().unwrap(),
        "--account",
        "1",
        "--server",
        "http://127.0.0.1:9/v1",
    ]);
    c.env("PATH", &empty);
    check(c, 1, "ffmpeg");
}

#[test]
fn old_highlights_removed_when_a_run_starts() {
    let d = tempfile::tempdir().unwrap();
    let v = d.path().join("v.mp4");
    let out = d.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("highlights.json"), "{}").unwrap();
    let empty = d.path().join("bin");
    std::fs::create_dir(&empty).unwrap();
    std::fs::write(&v, b"not a video").unwrap();
    let mut c = dlhl(&[
        v.to_str().unwrap(),
        "--account",
        "1",
        "--out",
        out.to_str().unwrap(),
        "--server",
        "http://127.0.0.1:9/v1",
    ]);
    c.env("PATH", &empty);
    check(c, 1, "ffmpeg");
    assert!(!out.join("highlights.json").exists());
}
