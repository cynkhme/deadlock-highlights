//! The HUD-present gate: template matching of the clock digits in the band
//! (spec section 2; reference `scratch/measure/clock_ncc.py` and `clock_window.py`).

use std::collections::BTreeMap;

use crate::profile::{BAND_H, BAND_W};

/// Pixels at or above this are glyph strokes; the templates were cut at the same value.
const THR: u8 = 150;
const SCALES: [f64; 5] = [0.85, 0.92, 1.0, 1.08, 1.16];
/// Chains of 4 glyphs (`0:09`) and of 5 (`14:39`).
const CHAIN_LENGTHS: [usize; 2] = [4, 5];
/// x distance between neighbouring glyph left edges, px.
const GAP_MIN: usize = 5;
const GAP_MAX: usize = 16;
/// The clock's place in the band: the chain's first glyph x, and the glyph rows, px.
const WINDOW_X: (usize, usize) = (150, 230);
const WINDOW_Y: (usize, usize) = (12, 26);

/// A template at one scale, zero-mean, with its sum of squares.
struct Scaled {
    w: usize,
    h: usize,
    centred: Vec<f64>,
    energy: f64,
}

pub(crate) struct Gate {
    /// The templates as shipped, for tests that check them.
    #[cfg(test)]
    glyphs: BTreeMap<char, Vec<Vec<f32>>>,
    /// Per scale, every class.
    scaled: Vec<Vec<Scaled>>,
}

#[derive(serde::Deserialize)]
struct Bank {
    classes: BTreeMap<char, Vec<Vec<f32>>>,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        let bank: Bank = serde_json::from_str(include_str!("../templates/clock_glyphs.json"))
            .expect("the shipped glyph bank parses");
        let scaled = SCALES
            .iter()
            .map(|&s| bank.classes.values().map(|t| scale(t, s)).collect())
            .collect();
        Gate {
            #[cfg(test)]
            glyphs: bank.classes,
            scaled,
        }
    }

    #[cfg(test)]
    fn glyph(&self, c: char) -> &[Vec<f32>] {
        &self.glyphs[&c]
    }

    /// The best glyph chain inside the clock window: the minimum glyph score along the
    /// chain, maximised over scales, chain lengths and positions; -1 when none fits.
    pub(crate) fn score(&self, gray: &[u8]) -> f32 {
        let (bw, bh) = (BAND_W as usize, BAND_H as usize);
        assert_eq!(gray.len(), bw * bh, "a {bw}x{bh} band");
        let img: Vec<f64> = gray
            .iter()
            .map(|&v| f64::from(u8::from(v >= THR)))
            .collect();
        let mut best = -1.0_f64;
        for classes in &self.scaled {
            let (tw, th) = (classes[0].w, classes[0].h);
            let (map_w, map_h) = (bw - tw + 1, bh - th + 1);
            // Every glyph of a chain that ends in the window starts in [x_lo, x_hi).
            let longest = CHAIN_LENGTHS[CHAIN_LENGTHS.len() - 1] - 1;
            let x_lo = WINDOW_X.0 + longest * GAP_MIN - longest * GAP_MAX;
            let x_hi = map_w.min(WINDOW_X.1 + longest * GAP_MAX);
            for y in WINDOW_Y.0..=WINDOW_Y.1.min(map_h - 1) {
                let mut row = vec![f64::NEG_INFINITY; map_w];
                for (x, cell) in row.iter_mut().enumerate().take(x_hi).skip(x_lo) {
                    *cell = classes
                        .iter()
                        .map(|t| ncc(&img, bw, x, y, t))
                        .fold(f64::NEG_INFINITY, f64::max);
                }
                for &k in &CHAIN_LENGTHS {
                    best = best.max(chain_in_window(&row, k));
                }
            }
        }
        best as f32
    }
}

/// Bilinear resize with half-pixel centres, edges clamped (`clock_ncc.resize`), then
/// centred. Sizes round half to even, as Python's `round` does.
#[expect(clippy::many_single_char_names, reason = "the reference's own symbols")]
fn scale(t: &[Vec<f32>], s: f64) -> Scaled {
    let (h, w) = (t.len(), t[0].len());
    let nh = ((h as f64 * s).round_ties_even() as usize).max(3);
    let nw = ((w as f64 * s).round_ties_even() as usize).max(3);
    let coords = |n: usize, len: usize| -> Vec<(usize, usize, f64)> {
        (0..n)
            .map(|i| {
                let c =
                    ((i as f64 + 0.5) * len as f64 / n as f64 - 0.5).clamp(0.0, (len - 1) as f64);
                let c0 = c.floor() as usize;
                (c0, (c0 + 1).min(len - 1), c - c0 as f64)
            })
            .collect()
    };
    let (ys, xs) = (coords(nh, h), coords(nw, w));
    let mut v = Vec::with_capacity(nh * nw);
    for &(y0, y1, fy) in &ys {
        for &(x0, x1, fx) in &xs {
            let p = |y: usize, x: usize| f64::from(t[y][x]);
            v.push(
                p(y0, x0) * (1.0 - fy) * (1.0 - fx)
                    + p(y0, x1) * (1.0 - fy) * fx
                    + p(y1, x0) * fy * (1.0 - fx)
                    + p(y1, x1) * fy * fx,
            );
        }
    }
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let centred: Vec<f64> = v.iter().map(|x| x - mean).collect();
    let energy = centred.iter().map(|x| x * x).sum();
    Scaled {
        w: nw,
        h: nh,
        centred,
        energy,
    }
}

/// Zero-mean normalised cross-correlation of `t` with the binary image at (x, y).
fn ncc(img: &[f64], stride: usize, x: usize, y: usize, t: &Scaled) -> f64 {
    let (mut s1, mut num) = (0.0, 0.0);
    for dy in 0..t.h {
        let row = &img[(y + dy) * stride + x..(y + dy) * stride + x + t.w];
        let trow = &t.centred[dy * t.w..(dy + 1) * t.w];
        for (p, c) in row.iter().zip(trow) {
            s1 += p;
            num += p * c;
        }
    }
    // The image is 0/1, so the sum of squares equals the sum.
    let n = (t.w * t.h) as f64;
    let den = ((s1 - s1 * s1 / n).max(0.0) * t.energy).sqrt();
    if den > 1e-9 { num / den.max(1e-9) } else { 0.0 }
}

/// The best chain of `k` glyphs along one row whose last glyph ends in the window.
fn chain_in_window(row: &[f64], k: usize) -> f64 {
    let n = row.len();
    let mut v = row.to_vec();
    for _ in 1..k {
        let mut cur = vec![f64::NEG_INFINITY; n];
        for (x, c) in cur.iter_mut().enumerate() {
            for g in GAP_MIN..=GAP_MAX.min(x) {
                *c = c.max(v[x - g].min(row[x]));
            }
        }
        v = cur;
    }
    let lo = WINDOW_X.0 + (k - 1) * GAP_MIN;
    let hi = n.min(WINDOW_X.1 + (k - 1) * GAP_MAX);
    v.get(lo..hi).map_or(f64::NEG_INFINITY, |w| {
        w.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{BAND_H, BAND_W};

    /// Uniform [0, 140] with 2% of pixels at 255, from a fixed LCG.
    fn background() -> Vec<u8> {
        let mut x: u32 = 12345;
        (0..BAND_W * BAND_H)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let r = (x >> 16) & 0x7fff;
                if r.is_multiple_of(50) {
                    255
                } else {
                    (r % 141) as u8
                }
            })
            .collect()
    }

    fn stamp(band: &mut [u8], glyph: &[Vec<f32>], x0: usize, y0: usize) {
        for (dy, row) in glyph.iter().enumerate() {
            for (dx, &v) in row.iter().enumerate() {
                if v >= 0.5 {
                    band[(y0 + dy) * BAND_W as usize + x0 + dx] = 255;
                }
            }
        }
    }

    fn clock_at(x: usize) -> Vec<u8> {
        let g = Gate::new();
        let mut b = background();
        for (c, dx) in [('1', 0), ('4', 11), (':', 22), ('3', 29), ('9', 40)] {
            stamp(&mut b, g.glyph(c), x + dx, 14);
        }
        b
    }

    #[test]
    fn bank_has_eleven_classes_of_11x10() {
        let g = Gate::new();
        for c in "0123456789:".chars() {
            let t = g.glyph(c);
            assert_eq!((t.len(), t[0].len()), (11, 10), "{c}");
        }
    }

    #[test]
    fn synthetic_clock_passes() {
        let s = Gate::new().score(&clock_at(160));
        assert!(s >= 0.40, "{s}");
    }

    #[test]
    fn noise_only_fails() {
        let s = Gate::new().score(&background());
        assert!(s < 0.40, "{s}");
    }

    #[test]
    fn glyphs_outside_window_fail() {
        let s = Gate::new().score(&clock_at(20));
        assert!(s < 0.40, "{s}");
    }

    #[test]
    fn blank_band_scores_zero() {
        assert!(Gate::new().score(&vec![0; (BAND_W * BAND_H) as usize]) <= 0.0);
    }

    /// Parity with the Python reference on the local atlas30 bands; not a package test.
    #[test]
    #[ignore = "needs /tmp/atlas30/band and scratch/measure/gate_reference.json"]
    fn gate_matches_reference() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let (Ok(refs), true) = (
            std::fs::read_to_string(root.join("scratch/measure/gate_reference.json")),
            std::path::Path::new("/tmp/atlas30/band").is_dir(),
        ) else {
            eprintln!("skipping: reference bands absent");
            return;
        };
        let refs: std::collections::BTreeMap<String, f64> = serde_json::from_str(&refs).unwrap();
        let g = Gate::new();
        let t0 = std::time::Instant::now();
        let mut worst = 0.0_f64;
        for (name, want) in &refs {
            let band = std::fs::read(format!("/tmp/atlas30/band/{name}")).unwrap();
            let got = f64::from(g.score(&band));
            worst = worst.max((got - want).abs());
            assert!(
                (got - want).abs() < 1e-4,
                "{name}: rust {got} python {want}"
            );
        }
        eprintln!(
            "{} frames, max |diff| {worst:.2e}, {:.1} ms per frame",
            refs.len(),
            t0.elapsed().as_secs_f64() * 1000.0 / refs.len() as f64
        );
    }
}
