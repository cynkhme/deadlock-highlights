//! ffmpeg and ffprobe: probe, the keyframe band pass, and stream-copied clips; and yt-dlp's
//! info.json beside the VOD.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
use crate::hud::profile::{BAND_H, BAND_W, FRAME_H, Profile};

/// One keyframe per this many seconds of VOD is gated and read, s.
pub(crate) const GRID_S: u32 = 30;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Probe {
    pub duration_s: f64,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub mtime: i64,
    pub audio: Vec<AudioTrack>,
}

/// An audio stream of the VOD.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AudioTrack {
    /// Position among the file's audio streams, from 0.
    pub index: usize,
    pub name: Option<String>,
}

/// A keyframe's clock band: gray for the gate, a 2x PNG for the reader.
pub(crate) struct Frame {
    pub pts_ms: u64,
    pub gray: Vec<u8>,
    pub png: Vec<u8>,
}

fn command_output(cmd: &mut Command, what: &str) -> Result<Vec<u8>> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().map_err(|e| {
        Error::Ffmpeg(if e.kind() == std::io::ErrorKind::NotFound {
            format!("{program} not found on PATH; dlhl needs ffmpeg and ffprobe")
        } else {
            format!("{program}: {e}")
        })
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(3).collect();
        return Err(Error::Ffmpeg(format!(
            "{what}: {program} failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    Ok(out.stdout)
}

pub(crate) fn probe(vod: &Path) -> Result<Probe> {
    let what = vod.display().to_string();
    let meta = std::fs::metadata(vod).map_err(|e| Error::Ffmpeg(format!("{what}: {e}")))?;
    let out = command_output(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(vod),
        &what,
    )?;
    let v: serde_json::Value = serde_json::from_slice(&out)
        .map_err(|e| Error::Ffmpeg(format!("{what}: ffprobe output: {e}")))?;
    let streams = v["streams"].as_array().map_or(&[][..], Vec::as_slice);
    let of_type = |t: &'static str| streams.iter().filter(move |s| s["codec_type"] == t);
    let stream = of_type("video").next().unwrap_or(&serde_json::Value::Null);
    let audio = of_type("audio")
        .enumerate()
        .map(|(index, s)| AudioTrack {
            index,
            name: track_name(&s["tags"]),
        })
        .collect();
    let dim = |k: &str| {
        stream[k]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
    };
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok());
    let (Some(width), Some(height), Some(duration_s)) = (dim("width"), dim("height"), duration)
    else {
        return Err(Error::Ffmpeg(format!(
            "{what}: no video stream with a size and duration"
        )));
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    Ok(Probe {
        duration_s,
        width,
        height,
        size: meta.len(),
        mtime,
        audio,
    })
}

/// A stream's title: `title`, or `name` as OBS and ffmpeg's MP4 demuxer give it, in any case.
fn track_name(tags: &serde_json::Value) -> Option<String> {
    let tags = tags.as_object()?;
    ["title", "name"].iter().find_map(|key| {
        tags.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| v.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
    })
}

/// An info.json whose duration differs from the VOD's by more than this describes
/// another file, such as the whole VOD of a section download.
const INFO_JSON_DURATION_TOLERANCE_S: f64 = 10.0;

/// The `timestamp` of `<vod stem>.info.json`; a missing or unparsable file is no anchor.
pub(crate) fn info_json_start(vod: &Path, probe: &Probe, warn: &dyn Fn(String)) -> Option<i64> {
    let file = vod.with_file_name(format!("{}.info.json", vod.file_stem()?.to_string_lossy()));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let timestamp = v.get("timestamp")?.as_i64()?;
    let duration = v.get("duration")?.as_f64()?;
    if (duration - probe.duration_s).abs() > INFO_JSON_DURATION_TOLERANCE_S {
        warn(format!(
            "ignoring {}: its duration {duration} s differs from the VOD's {} s (a section download?)",
            file.display(),
            probe.duration_s
        ));
        return None;
    }
    Some(timestamp)
}

/// The frame width at height 1080 (even, as the scaler needs), and the band's top-left there.
fn crop_origin(width: u32, height: u32, profile: &Profile) -> (u32, u32, u32) {
    let w = ((f64::from(width) * f64::from(FRAME_H) / f64::from(height) / 2.0).round() as u32 * 2)
        .max(BAND_W);
    let x = ((profile.band.center_x * f64::from(w)).round() as i64 - i64::from(BAND_W / 2))
        .clamp(0, i64::from(w - BAND_W)) as u32;
    let y = ((profile.band.top * f64::from(FRAME_H)).round() as i64)
        .clamp(0, i64::from(FRAME_H - BAND_H)) as u32;
    (w, x, y)
}

/// One keyframe per `every_s` seconds, each reduced to its clock band, in pts order.
pub(crate) fn keyframes(
    vod: &Path,
    probe: &Probe,
    profile: &Profile,
    every_s: u32,
) -> Result<Vec<Frame>> {
    let what = vod.display().to_string();
    let tmp = tempfile::tempdir().map_err(|e| Error::Io {
        path: std::env::temp_dir(),
        source: e,
    })?;
    let (gdir, cdir) = (tmp.path().join("g"), tmp.path().join("c"));
    for d in [&gdir, &cdir] {
        std::fs::create_dir(d).map_err(|e| Error::Io {
            path: d.clone(),
            source: e,
        })?;
    }
    let (w, x, y) = crop_origin(probe.width, probe.height, profile);
    let graph = format!(
        "[0:v]select='isnan(prev_selected_t)+gte(t-prev_selected_t,{})',settb=1/1000,\
         scale={w}:{FRAME_H},crop={BAND_W}:{BAND_H}:{x}:{y},split=2[g][c];\
         [g]format=gray[go];[c]scale={}:{}:flags=bicubic[co]",
        every_s.saturating_sub(1),
        2 * BAND_W,
        2 * BAND_H,
    );
    let per_output = [
        "-fps_mode",
        "passthrough",
        "-enc_time_base",
        "1:1000",
        "-frame_pts",
        "1",
    ];
    command_output(
        Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-skip_frame", "nokey", "-i"])
            .arg(vod)
            .args(["-filter_complex", &graph, "-map", "[go]"])
            .args(per_output)
            .args(["-c:v", "rawvideo", "-pix_fmt", "gray", "-f", "image2"])
            .arg(gdir.join("%d.raw"))
            .args(["-map", "[co]"])
            .args(per_output)
            .args(["-c:v", "png", "-f", "image2"])
            .arg(cdir.join("%d.png")),
        &what,
    )?;
    let names = |dir: &Path, ext: &str| -> Result<Vec<u64>> {
        let rd = std::fs::read_dir(dir).map_err(|e| Error::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let mut v: Vec<u64> = rd
            .filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_str()?
                    .strip_suffix(ext)?
                    .parse()
                    .ok()
            })
            .collect();
        v.sort_unstable();
        Ok(v)
    };
    let (gray, color) = (names(&gdir, ".raw")?, names(&cdir, ".png")?);
    if gray != color {
        return Err(Error::Ffmpeg(format!(
            "{what}: the gray and PNG band passes disagree on frames"
        )));
    }
    gray.into_iter()
        .map(|pts_ms| {
            let read = |p: std::path::PathBuf| {
                std::fs::read(&p).map_err(|e| Error::Io { path: p, source: e })
            };
            let g = read(gdir.join(format!("{pts_ms}.raw")))?;
            if g.len() != (BAND_W * BAND_H) as usize {
                return Err(Error::Ffmpeg(format!(
                    "{what}: band of frame {pts_ms} ms has {} bytes",
                    g.len()
                )));
            }
            Ok(Frame {
                pts_ms,
                gray: g,
                png: read(cdir.join(format!("{pts_ms}.png")))?,
            })
        })
        .collect()
}

/// Stream-copies [start, end) of the VOD's first video stream and `audio` to `dest`, each
/// audio track titled with its name; a partial file never takes its name.
/// The tracks `wanted` names, in its order: each a 1-based position, or else a name in any
/// case. None wanted is every track.
///
/// # Errors
///
/// [`Error::Usage`] for a position or name the VOD lacks, a name on several tracks, or a
/// track wanted twice; the message lists the VOD's tracks.
pub(crate) fn select_audio(tracks: &[AudioTrack], wanted: &[String]) -> Result<Vec<AudioTrack>> {
    if wanted.is_empty() {
        return Ok(tracks.to_vec());
    }
    let listed = || {
        if tracks.is_empty() {
            return "the VOD has no audio track".to_owned();
        }
        let list: Vec<String> = tracks
            .iter()
            .map(|t| match &t.name {
                Some(n) => format!("{} {n:?}", t.index + 1),
                None => format!("{} (no name)", t.index + 1),
            })
            .collect();
        format!("the VOD's audio tracks are {}", list.join(", "))
    };
    let mut picked: Vec<AudioTrack> = Vec::new();
    for w in wanted {
        let w = w.trim();
        let track = if let Ok(n) = w.parse::<usize>() {
            n.checked_sub(1)
                .and_then(|i| tracks.get(i))
                .ok_or_else(|| Error::Usage(format!("no audio track {n}: {}", listed())))?
        } else {
            let found: Vec<&AudioTrack> = tracks
                .iter()
                .filter(|t| t.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(w)))
                .collect();
            match found[..] {
                [t] => t,
                [] => {
                    return Err(Error::Usage(format!("no audio track {w:?}: {}", listed())));
                }
                _ => {
                    let at: Vec<String> = found.iter().map(|t| (t.index + 1).to_string()).collect();
                    return Err(Error::Usage(format!(
                        "audio tracks {} are all named {w:?}; pick one by position",
                        at.join(" and ")
                    )));
                }
            }
        };
        if picked.contains(track) {
            return Err(Error::Usage(format!(
                "audio track {} is wanted twice",
                track.index + 1
            )));
        }
        picked.push(track.clone());
    }
    Ok(picked)
}

pub(crate) fn cut_clip(
    vod: &Path,
    start: f64,
    end: f64,
    audio: &[AudioTrack],
    dest: &Path,
) -> Result<()> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| Error::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    // ffmpeg picks the container from the extension, so the temporary name keeps it.
    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let part = dir.join(format!("{stem}.part.{ext}"));
    let result = command_output(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-ss",
                &format!("{start:.3}"),
                "-i",
            ])
            .arg(vod)
            .args(["-t", &format!("{:.3}", end - start), "-map", "0:v:0"])
            .args(audio_args(audio))
            .args(["-c", "copy", "-avoid_negative_ts", "make_zero"])
            .arg(&part),
        &vod.display().to_string(),
    );
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest).map_err(|e| Error::Io {
        path: dest.to_path_buf(),
        source: e,
    })
}

/// A map per track; a title per named one, which the MP4 muxer writes as the track's name.
fn audio_args(audio: &[AudioTrack]) -> Vec<String> {
    let mut args = Vec::new();
    for (out, t) in audio.iter().enumerate() {
        args.extend(["-map".to_owned(), format!("0:a:{}", t.index)]);
        if let Some(n) = &t.name {
            args.extend([format!("-metadata:s:a:{out}"), format!("title={n}")]);
        }
    }
    args
}

#[cfg(test)]
#[allow(clippy::many_single_char_names, reason = "short-lived test values")]
mod tests {
    use super::*;
    use crate::test_support::have_ffmpeg;

    /// A synthetic H.264 video with a keyframe every second.
    fn video(dir: &Path, name: &str, source: &str, secs: u32) -> std::path::PathBuf {
        let out = dir.join(name);
        let st = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                source,
                "-t",
                &secs.to_string(),
            ])
            .args(["-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        out
    }

    fn png_size(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        (be(&png[16..20]), be(&png[20..24]))
    }

    #[test]
    fn probe_reads_duration_and_size() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let v = video(d.path(), "a.mp4", "testsrc2=size=1920x1080:rate=10", 95);
        let p = probe(&v).unwrap();
        assert_eq!((p.width, p.height), (1920, 1080));
        assert!((p.duration_s - 95.0).abs() < 0.5, "{}", p.duration_s);
        assert_eq!(p.size, std::fs::metadata(&v).unwrap().len());
        assert!(p.mtime > 1_700_000_000);
    }

    #[test]
    fn probe_missing_file_is_ffmpeg_error() {
        if !have_ffmpeg() {
            return;
        }
        let e = probe(Path::new("/nonexistent/dir/v.mp4")).unwrap_err();
        assert!(
            matches!(&e, Error::Ffmpeg(m) if m.contains("/nonexistent/dir/v.mp4")),
            "{e}"
        );
    }

    #[test]
    fn keyframes_on_30s_grid() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let v = video(d.path(), "a.mp4", "testsrc2=size=1920x1080:rate=10", 95);
        let p = probe(&v).unwrap();
        let f = keyframes(&v, &p, &Profile::default(), GRID_S).unwrap();
        let pts: Vec<u64> = f.iter().map(|f| f.pts_ms).collect();
        assert_eq!(pts.len(), 4, "{pts:?}");
        for (got, want) in pts.iter().zip([0, 30_000, 60_000, 90_000]) {
            assert!(got.abs_diff(want) <= 1000, "{pts:?}");
        }
        for fr in &f {
            assert_eq!(fr.gray.len(), (BAND_W * BAND_H) as usize);
            assert_eq!(png_size(&fr.png), (2 * BAND_W, 2 * BAND_H));
        }
        assert!(
            std::fs::read_dir(d.path()).unwrap().count() == 1,
            "nothing written beside the VOD"
        );
    }

    #[test]
    fn keyframes_band_is_centred_for_ultrawide() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let src =
            "color=c=black:size=2560x1080:rate=10,drawbox=x=1279:y=0:w=2:h=64:color=white:t=fill";
        let v = video(d.path(), "w.mp4", src, 35);
        let p = probe(&v).unwrap();
        let f = keyframes(&v, &p, &Profile::default(), GRID_S).unwrap();
        assert_eq!(f.len(), 2);
        let w = BAND_W as usize;
        for fr in &f {
            for y in [5, 30, 60] {
                let row = &fr.gray[y * w..(y + 1) * w];
                assert!(
                    row[179] >= 200 && row[180] >= 200,
                    "row {y}: {:?}",
                    &row[176..184]
                );
                assert!(row[0] < 50 && row[170] < 50 && row[190] < 50);
            }
        }
    }

    #[test]
    fn keyframes_720p_upscaled() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let v = video(d.path(), "s.mp4", "testsrc2=size=1280x720:rate=10", 35);
        let p = probe(&v).unwrap();
        let f = keyframes(&v, &p, &Profile::default(), GRID_S).unwrap();
        assert_eq!(f.len(), 2);
        assert!(
            f.iter()
                .all(|fr| fr.gray.len() == (BAND_W * BAND_H) as usize)
        );
    }

    #[test]
    fn crop_x_is_clamped() {
        assert_eq!(crop_origin(1920, 1080, &Profile::default()), (1920, 780, 0));
        assert_eq!(crop_origin(1280, 720, &Profile::default()), (1920, 780, 0));
        assert_eq!(
            crop_origin(2560, 1080, &Profile::default()),
            (2560, 1100, 0)
        );
        let mut edge = Profile::default();
        edge.band.center_x = 0.0;
        edge.band.top = 1.0;
        assert_eq!(crop_origin(1920, 1080, &edge), (1920, 0, 1016));
        edge.band.center_x = 1.0;
        assert_eq!(crop_origin(1920, 1080, &edge).1, 1560);
    }

    #[test]
    fn cut_clip_stream_copies() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let v = video(d.path(), "a.mp4", "testsrc2=size=640x360:rate=10", 60);
        let dest = d.path().join("clips").join("1-10.mp4");
        cut_clip(&v, 10.0, 30.0, &[], &dest).unwrap();
        let p = probe(&dest).unwrap();
        assert!((p.duration_s - 20.0).abs() <= 2.0, "{}", p.duration_s);
        let names: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["1-10.mp4"]);
    }

    /// A synthetic video with one audio stream per entry of `names`, titled where given.
    fn video_with_audio(dir: &Path, name: &str, names: &[Option<&str>]) -> std::path::PathBuf {
        let out = dir.join(name);
        let mut c = Command::new("ffmpeg");
        c.args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x240:rate=10");
        for i in 0..names.len() {
            c.args(["-f", "lavfi", "-i"])
                .arg(format!("sine=frequency={}", 300 * (i + 1)));
        }
        c.args(["-map", "0:v"]);
        for (i, n) in names.iter().enumerate() {
            c.args(["-map", &format!("{}:a", i + 1)]);
            if let Some(n) = n {
                c.arg(format!("-metadata:s:a:{i}"))
                    .arg(format!("title={n}"));
            }
        }
        let st = c
            .args([
                "-t", "20", "-c:v", "libx264", "-g", "10", "-pix_fmt", "yuv420p",
            ])
            .args(["-c:a", "aac"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        out
    }

    fn names(tracks: &[AudioTrack]) -> Vec<Option<&str>> {
        tracks.iter().map(|t| t.name.as_deref()).collect()
    }

    fn track(index: usize, name: Option<&str>) -> AudioTrack {
        AudioTrack {
            index,
            name: name.map(Into::into),
        }
    }

    fn select(tracks: &[AudioTrack], wanted: &[&str]) -> Result<Vec<AudioTrack>> {
        let wanted: Vec<String> = wanted.iter().map(|&s| s.to_owned()).collect();
        select_audio(tracks, &wanted)
    }

    #[test]
    fn probe_lists_audio_tracks_in_file_order() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let v = video_with_audio(d.path(), "a.mp4", &[Some("Mix"), None, Some("Mic")]);
        let p = probe(&v).unwrap();
        assert_eq!(names(&p.audio), [Some("Mix"), None, Some("Mic")]);
        assert_eq!(
            p.audio.iter().map(|t| t.index).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        let silent = video(d.path(), "s.mp4", "testsrc2=size=320x240:rate=10", 5);
        assert_eq!(probe(&silent).unwrap().audio, []);
    }

    #[test]
    fn select_audio_by_name_or_position() {
        let tracks = [
            track(0, Some("Mix")),
            track(1, Some("Game")),
            track(2, None),
        ];
        assert_eq!(select(&tracks, &[]).unwrap(), tracks);
        assert_eq!(
            select(&tracks, &["game", " 3", "1"]).unwrap(),
            [tracks[1].clone(), tracks[2].clone(), tracks[0].clone()]
        );
        assert_eq!(select(&[], &[]).unwrap(), []);
    }

    #[test]
    fn select_audio_refuses_what_the_vod_lacks() {
        let tracks = [
            track(0, Some("Mix")),
            track(1, Some("Game")),
            track(2, Some("game")),
        ];
        let cases: [(&[AudioTrack], &[&str], &str); 6] = [
            (&tracks, &["Mic"], "\"Mic\""),
            (&tracks, &["4"], "4"),
            (&tracks, &["0"], "0"),
            (&tracks, &["Game"], "2 and 3"),
            (&tracks, &["Mix", "1"], "twice"),
            (&[], &["1"], "no audio"),
        ];
        for (tracks, wanted, needle) in cases {
            let e = select(tracks, wanted).unwrap_err();
            assert!(
                matches!(&e, Error::Usage(m) if m.contains(needle) && !m.contains("--")),
                "{wanted:?}: {e}"
            );
        }
        let e = select(&tracks, &["Mic"]).unwrap_err().to_string();
        assert!(
            e.contains("1 \"Mix\", 2 \"Game\", 3 \"game\""),
            "lists the tracks: {e}"
        );
    }

    #[test]
    fn cut_clip_keeps_every_audio_track_and_its_name() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        for ext in ["mp4", "mkv"] {
            let v = video_with_audio(
                d.path(),
                &format!("a.{ext}"),
                &[Some("Mix"), None, Some("Mic")],
            );
            let p = probe(&v).unwrap();
            let dest = d.path().join(format!("all.{ext}"));
            cut_clip(&v, 5.0, 10.0, &p.audio, &dest).unwrap();
            assert_eq!(
                names(&probe(&dest).unwrap().audio),
                names(&p.audio),
                "{ext}"
            );
        }
    }

    #[test]
    fn cut_clip_keeps_selected_tracks_in_their_order() {
        if !have_ffmpeg() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let v = video_with_audio(d.path(), "a.mp4", &[Some("Mix"), Some("Game"), Some("Mic")]);
        let p = probe(&v).unwrap();
        let dest = d.path().join("some.mp4");
        let picked = select(&p.audio, &["Mic", "Mix"]).unwrap();
        cut_clip(&v, 5.0, 10.0, &picked, &dest).unwrap();
        assert_eq!(
            names(&probe(&dest).unwrap().audio),
            [Some("Mic"), Some("Mix")]
        );
    }
}
