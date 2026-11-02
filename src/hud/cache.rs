//! The read-stage cache: one file per key, written once when a read completes.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::hud::profile::{Band, Profile};
use crate::hud::reader::{FrameReading, ReaderIdentity, too_many_failed};

/// Bumped whenever request parameters or strip preparation change: old files then miss.
pub(crate) const READ_FORMAT: u32 = 1;

/// Everything that decides what a read of this VOD returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CacheKey {
    pub band: Band,
    pub t: f32,
    pub reader: ReaderIdentity,
    pub grid_s: u32,
    pub vod_size: u64,
    pub vod_mtime: i64,
    pub read_format: u32,
}

#[derive(Serialize)]
struct File<'a> {
    key: &'a CacheKey,
    profile: &'a Profile,
    grid_s: u32,
    frames: &'a [FrameReading],
}

/// FNV-1a 64 over the key's JSON, as 16 lowercase hex digits.
pub(crate) fn key_hash(k: &CacheKey) -> String {
    let bytes = serde_json::to_vec(k).expect("a cache key serialises");
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{h:016x}")
}

fn path(out: &Path, k: &CacheKey) -> PathBuf {
    out.join("readings").join(format!("{}.json", key_hash(k)))
}

/// The readings stored under `k`, or `None` for a missing, unreadable or foreign file.
///
/// A file is written only after a complete read, and the key holds the VOD's size, mtime
/// and grid step, so a matching key means the whole grid is covered.
pub(crate) fn load(out: &Path, k: &CacheKey) -> Option<Vec<FrameReading>> {
    let text = std::fs::read_to_string(path(out, k)).ok()?;
    let mut v: serde_json::Value = serde_json::from_str(&text).ok()?;
    // The reader identity holds borrowed strings, so the key compares as parsed JSON text:
    // `to_value` would widen `t` to an f64 that the text form does not hold.
    let want: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(k).ok()?).ok()?;
    if v.get("key")? != &want {
        return None;
    }
    serde_json::from_value(v.get_mut("frames")?.take()).ok()
}

/// Writes the readings under `k`, unless too many reads failed to be worth keeping.
///
/// The file is written beside its final name and renamed, so an interrupted write leaves
/// no file that parses as a hit.
pub(crate) fn store(out: &Path, k: &CacheKey, profile: &Profile, r: &[FrameReading]) -> Result<()> {
    if too_many_failed(r).is_some() {
        return Ok(());
    }
    let dest = path(out, k);
    let dir = dest.parent().expect("a readings file has a directory");
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io { path, source }
    };
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let body = serde_json::to_vec_pretty(&File {
        key: k,
        profile,
        grid_s: k.grid_s,
        frames: r,
    })
    .expect("readings serialise");
    let tmp = dest.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(io(&tmp))?;
    std::fs::rename(&tmp, &dest).map_err(io(&dest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hud::reader::Outcome;

    fn key() -> CacheKey {
        CacheKey {
            band: Profile::default().band,
            t: 0.40,
            reader: ReaderIdentity {
                protocol: "openai-chat",
                server: "http://localhost:11434/v1".into(),
                model: "gemma4:e4b".into(),
                prompt: "p",
            },
            grid_s: 30,
            vod_size: 1000,
            vod_mtime: 1_793_000_000,
            read_format: READ_FORMAT,
        }
    }

    fn readings(n: u64, failed: u64) -> Vec<FrameReading> {
        (0..n)
            .map(|i| FrameReading {
                pts_ms: i * 30_000,
                gate: 0.5,
                outcome: if i < failed {
                    Outcome::Failed {
                        error: "503".into(),
                    }
                } else {
                    Outcome::Read {
                        raw: format!(r#"{{"clock":"{i}:00"}}"#),
                    }
                },
            })
            .collect()
    }

    #[test]
    fn cache_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let r = readings(10, 1);
        store(d.path(), &key(), &Profile::default(), &r).unwrap();
        assert_eq!(load(d.path(), &key()), Some(r));
        let f = d
            .path()
            .join("readings")
            .join(format!("{}.json", key_hash(&key())));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        assert_eq!(v["grid_s"], 30);
        assert_eq!(v["profile"]["t"], serde_json::json!(0.4));
    }

    #[test]
    fn cache_key_changes_with_mtime_size_band_t_model() {
        let d = tempfile::tempdir().unwrap();
        store(d.path(), &key(), &Profile::default(), &readings(3, 0)).unwrap();
        let base = key_hash(&key());
        let variants: Vec<CacheKey> = vec![
            CacheKey {
                vod_mtime: 1,
                ..key()
            },
            CacheKey {
                vod_size: 1001,
                ..key()
            },
            CacheKey {
                band: Band {
                    center_x: 0.4,
                    top: 0.0,
                },
                ..key()
            },
            CacheKey { t: 0.35, ..key() },
            CacheKey {
                reader: ReaderIdentity {
                    model: "qwen3-vl:4b-instruct".into(),
                    ..key().reader
                },
                ..key()
            },
            CacheKey {
                reader: ReaderIdentity {
                    server: "http://127.0.0.1:8080/v1".into(),
                    ..key().reader
                },
                ..key()
            },
            CacheKey {
                reader: ReaderIdentity {
                    prompt: "q",
                    ..key().reader
                },
                ..key()
            },
            CacheKey {
                grid_s: 15,
                ..key()
            },
            CacheKey {
                read_format: READ_FORMAT + 1,
                ..key()
            },
        ];
        for k in variants {
            assert_ne!(key_hash(&k), base);
            assert_eq!(load(d.path(), &k), None);
        }
    }

    #[test]
    fn cache_not_written_over_10_percent_failed() {
        let d = tempfile::tempdir().unwrap();
        store(d.path(), &key(), &Profile::default(), &readings(10, 2)).unwrap();
        assert_eq!(load(d.path(), &key()), None);
        assert!(
            !d.path().join("readings").exists()
                || std::fs::read_dir(d.path().join("readings"))
                    .unwrap()
                    .count()
                    == 0
        );
    }

    #[test]
    fn corrupt_cache_is_miss() {
        let d = tempfile::tempdir().unwrap();
        store(d.path(), &key(), &Profile::default(), &readings(3, 0)).unwrap();
        let f = d
            .path()
            .join("readings")
            .join(format!("{}.json", key_hash(&key())));
        let text = std::fs::read_to_string(&f).unwrap();
        std::fs::write(&f, &text[..text.len() / 2]).unwrap();
        assert_eq!(load(d.path(), &key()), None);
    }

    #[test]
    fn cache_write_is_atomic() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("readings");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("{}.json", key_hash(&key())));
        std::fs::write(&f, "{ truncated").unwrap();
        assert_eq!(load(d.path(), &key()), None);
        store(d.path(), &key(), &Profile::default(), &readings(3, 0)).unwrap();
        assert!(load(d.path(), &key()).is_some());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn stored_key_must_match_not_just_name() {
        let d = tempfile::tempdir().unwrap();
        store(d.path(), &key(), &Profile::default(), &readings(3, 0)).unwrap();
        let dir = d.path().join("readings");
        let f = dir.join(format!("{}.json", key_hash(&key())));
        let other = dir.join(format!(
            "{}.json",
            key_hash(&CacheKey {
                vod_size: 7,
                ..key()
            })
        ));
        std::fs::rename(f, other).unwrap();
        assert_eq!(
            load(
                d.path(),
                &CacheKey {
                    vod_size: 7,
                    ..key()
                }
            ),
            None
        );
    }

    #[test]
    fn key_hash_is_stable() {
        assert_eq!(key_hash(&key()), "6d104abfce1290cc");
    }
}
