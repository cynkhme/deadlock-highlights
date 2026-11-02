use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The clock band at height 1080, px. Fixed: the gate's search window is in band pixels.
pub(crate) const BAND_W: u32 = 360;
pub(crate) const BAND_H: u32 = 64;

/// Where the clock band sits on the frame, and the gate threshold.
///
/// Positions are fractions of the frame after it is scaled to height 1080.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Profile {
    pub version: u32,
    pub band: Band,
    /// Frames whose gate score is at least `t` are read.
    pub t: f32,
}

/// The band's horizontal centre as a fraction of the frame width, and its top edge as a
/// fraction of the frame height.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Band {
    pub center_x: f64,
    pub top: f64,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            version: 1,
            band: Band {
                center_x: 0.5,
                top: 0.0,
            },
            t: 0.40,
        }
    }
}

impl Profile {
    /// Reads a HUD profile file.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] when the file is missing, malformed, of another version, or holds a
    /// value outside [0, 1].
    pub fn load(path: &Path) -> Result<Profile> {
        let bad = |why: String| Error::Usage(format!("profile {}: {why}", path.display()));
        let text = std::fs::read_to_string(path).map_err(|e| bad(e.to_string()))?;
        let p: Profile = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
        if p.version != 1 {
            return Err(bad(format!(
                "version {} is not supported, expected 1",
                p.version
            )));
        }
        let unit = |v: f64| (0.0..=1.0).contains(&v);
        if !unit(p.band.center_x) || !unit(p.band.top) || !unit(f64::from(p.t)) {
            return Err(bad("band and t must lie in [0, 1]".into()));
        }
        Ok(p)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact default values")]
mod tests {
    use super::*;

    #[test]
    fn profile_default_values() {
        let p = Profile::default();
        assert_eq!(p.version, 1);
        assert_eq!((p.band.center_x, p.band.top), (0.5, 0.0));
        assert_eq!(p.t, 0.40);
    }

    #[test]
    fn profile_load_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("p.json");
        std::fs::write(
            &f,
            r#"{"version":1,"band":{"center_x":0.25,"top":0.1},"t":0.3}"#,
        )
        .unwrap();
        let p = Profile::load(&f).unwrap();
        assert_eq!((p.band.center_x, p.band.top, p.t), (0.25, 0.1, 0.3));
    }

    #[test]
    fn profile_load_rejects_bad_version_and_range() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("p.json");
        for bad in [
            r#"{"version":2,"band":{"center_x":0.5,"top":0.0},"t":0.4}"#,
            r#"{"version":1,"band":{"center_x":1.5,"top":0.0},"t":0.4}"#,
            r#"{"version":1,"band":{"center_x":0.5,"top":-0.1},"t":0.4}"#,
            r#"{"version":1,"band":{"center_x":0.5,"top":0.0},"t":1.4}"#,
            r#"{"version":1}"#,
            "not json",
        ] {
            std::fs::write(&f, bad).unwrap();
            assert!(matches!(Profile::load(&f), Err(Error::Usage(_))), "{bad}");
        }
        assert!(matches!(
            Profile::load(&d.path().join("missing.json")),
            Err(Error::Usage(_))
        ));
    }
}
