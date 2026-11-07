//! The default time stretch of ARA playback regions: deterministic granular
//! overlap-add. Every output sample depends only on the source and on its
//! position in the region, so the renderer needs no state and can start
//! anywhere (a block, a loop, a jump of the playhead).

use crate::Source;
use std::sync::OnceLock;

/// Grain length in samples.
const GRAIN: usize = 2048;
/// Distance between grains in the output: a quarter of a grain (Hann windows
/// at 75 % overlap add up to 2).
const HOP: usize = GRAIN / 4;

fn window() -> &'static [f32; GRAIN] {
    static WINDOW: OnceLock<[f32; GRAIN]> = OnceLock::new();
    WINDOW.get_or_init(|| {
        let mut w = [0.0f32; GRAIN];
        for (j, w) in w.iter_mut().enumerate() {
            *w = (0.5 - 0.5 * (2.0 * std::f64::consts::PI * j as f64 / GRAIN as f64).cos()) as f32; // periodic Hann
        }
        w
    })
}

/// How far a grain may move to line up with the previous one, in samples.
const SEARCH: i64 = 640;
/// Length of the segment compared when lining grains up.
const COMPARE: usize = 512;
/// A request this many grains ahead of the chain starts a new chain (the playhead jumped).
const MAX_AHEAD: i64 = 64;

/// The grains chosen so far for one region being played, so that each grain
/// lines up with the one before it (waveform similarity overlap-add).
struct Chain {
    key: (u64, u64, usize, usize),
    /// (grain number, start in the source) for consecutive grains.
    grains: std::collections::VecDeque<(i64, i64)>,
}

thread_local! {
    static CHAINS: std::cell::RefCell<Vec<Chain>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn mono(source: &Source<'_>, at: i64) -> f32 {
    if !(0..source.len() as i64).contains(&at) {
        return 0.0;
    }
    let sum: f32 = (0..source.channels()).map(|c| source.channel(c)[at as usize]).sum();
    sum / source.channels() as f32
}

/// Start of the grain after one that starts at `previous`: near `ideal`, the
/// position that continues the previous grain best.
fn align(source: &Source<'_>, previous: i64, ideal: i64) -> i64 {
    let reference: Vec<f32> = (0..COMPARE as i64).map(|i| mono(source, previous + HOP as i64 + i)).collect();
    let span = COMPARE + 2 * SEARCH as usize;
    let candidates: Vec<f32> = (0..span as i64).map(|i| mono(source, ideal - SEARCH + i)).collect();
    let mut energy: f32 = candidates[..COMPARE].iter().map(|s| s * s).sum();
    let (mut best, mut best_score) = (0i64, f32::MIN);
    for d in 0..=2 * SEARCH as usize {
        if d > 0 {
            let (gone, new) = (candidates[d - 1], candidates[d + COMPARE - 1]);
            energy += new * new - gone * gone;
        }
        let dot: f32 = reference.iter().zip(&candidates[d..d + COMPARE]).map(|(a, b)| a * b).sum();
        let score = dot / (energy.max(1e-9)).sqrt();
        // Slightly prefer staying near the ideal position.
        let score = score * (1.0 - 0.02 * ((d as i64 - SEARCH).abs() as f32 / SEARCH as f32));
        if score > best_score {
            best_score = score;
            best = d as i64 - SEARCH;
        }
    }
    ideal + best
}

/// Renders `out` (silent on entry) as the source stretched by `ratio`
/// (source samples per output sample; 0.5 plays at half speed with the
/// pitch unchanged).
///
/// `source_pos` is where output sample 0 of `out` lies in the source and
/// `region_offset` its position inside the playback region. Grains sit on a
/// grid anchored at the start of the region and each lines up with the one
/// before it, so the sound is continuous when the region plays on from block
/// to block (a jump of the playhead starts the alignment afresh).
pub fn granular(source: &Source<'_>, source_pos: f64, ratio: f64, region_offset: i64, out: &mut [&mut [f32]]) {
    let n = out.first().map_or(0, |o| o.len());
    if n == 0 || source.channels() == 0 || source.is_empty() || !(ratio.is_finite() && ratio > 0.0) {
        return;
    }
    let window = window();
    let len = source.len() as i64;
    let anchor = source_pos - region_offset as f64 * ratio;
    let overlap = (GRAIN / HOP) as i64;
    // The first grain of the region is the first there is: no grains before it fade the start in.
    let first = (region_offset.div_euclid(HOP as i64) - overlap + 1).max(0);
    let last = (region_offset + n as i64 - 1).div_euclid(HOP as i64);
    let ideal = |k: i64| (anchor + (k * HOP as i64) as f64 * ratio).round() as i64;
    let key = (anchor.to_bits(), ratio.to_bits(), source.channel(0).as_ptr() as usize, source.len());

    // Grain starts for first..=last, continuing the chain of this region if there is one.
    let starts: Vec<i64> = CHAINS.with(|chains| {
        let mut chains = chains.borrow_mut();
        let index = match chains.iter().position(|c| c.key == key) {
            Some(i) => i,
            None => {
                if chains.len() >= 16 {
                    chains.remove(0);
                }
                chains.push(Chain { key, grains: Default::default() });
                chains.len() - 1
            }
        };
        let chain = &mut chains[index];
        let usable = match (chain.grains.front(), chain.grains.back()) {
            (Some(&(front, _)), Some(&(back, _))) => front <= first && first <= back + 1 && last <= back + MAX_AHEAD,
            _ => false,
        };
        if !usable {
            chain.grains.clear();
            chain.grains.push_back((first, ideal(first)));
        }
        while chain.grains.back().is_some_and(|&(k, _)| k < last) {
            let &(k, s) = chain.grains.back().unwrap();
            chain.grains.push_back((k + 1, align(source, s, ideal(k + 1))));
        }
        while chain.grains.front().is_some_and(|&(k, _)| k < first.min(last) - 8) {
            chain.grains.pop_front();
        }
        let front = chain.grains.front().unwrap().0;
        (first..=last).map(|k| chain.grains[(k - front) as usize].1).collect()
    });

    let direct = source.channels() == out.len();
    let scale = 1.0 / source.channels() as f32;
    for i in 0..n {
        let q = region_offset + i as i64;
        let newest = q.div_euclid(HOP as i64);
        let mut total = 0.0f32;
        let mut mixed = [0.0f32; 8];
        for k in (newest - overlap + 1).max(0)..=newest {
            let j = (q - k * HOP as i64) as usize;
            // The first grain of the region starts at full level.
            let w = if k == 0 && j < GRAIN / 2 { 1.0 } else { window[j] };
            total += w;
            let at = starts[(k - first) as usize] + j as i64;
            if !(0..len).contains(&at) {
                continue;
            }
            if direct {
                for (c, m) in mixed.iter_mut().enumerate().take(out.len()) {
                    *m += w * source.channel(c)[at as usize];
                }
            } else {
                let sum = (0..source.channels()).map(|c| source.channel(c)[at as usize]).sum::<f32>() * scale;
                for m in mixed.iter_mut().take(out.len()) {
                    *m += w * sum;
                }
            }
        }
        if total > 0.0 {
            for (c, out) in out.iter_mut().enumerate().take(8) {
                out[i] += mixed[c] / total;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() as f32).collect()
    }

    fn crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    }

    fn render(source: &Source<'_>, ratio: f64, n: usize, block: usize) -> Vec<f32> {
        let mut result = vec![0.0f32; n];
        for (b, chunk) in result.chunks_mut(block).enumerate() {
            let at = b * block;
            granular(source, at as f64 * ratio, ratio, at as i64, &mut [chunk]);
        }
        result
    }

    #[test]
    fn stretching_keeps_pitch_and_level() {
        let rate = 48_000.0;
        let input = sine(440.0, rate, 96_000);
        let ptrs = [input.as_ptr()];
        let source = Source { channels: &ptrs, len: input.len(), sample_rate: rate };
        // Half speed: 1 s of output from 0.5 s of source.
        let out = render(&source, 0.5, 48_000, 480);
        let body = &out[4_000..44_000];
        // 440 Hz stays 440 Hz (40 000 samples = 0.8333 s -> ~366 periods).
        let expected = 440.0 * body.len() as f64 / rate;
        assert!((crossings(body) as f64 - expected).abs() < 8.0, "{} vs {expected}", crossings(body));
        let peak = body.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.8..1.2).contains(&peak), "peak {peak}");
    }

    #[test]
    fn blocks_do_not_change_the_result() {
        let input = sine(220.0, 44_100.0, 44_100);
        let ptrs = [input.as_ptr()];
        let source = Source { channels: &ptrs, len: input.len(), sample_rate: 44_100.0 };
        let whole = render(&source, 1.5, 20_000, 20_000);
        let blocks = render(&source, 1.5, 20_000, 333);
        assert_eq!(whole, blocks);
    }

    #[test]
    fn the_start_of_a_region_is_not_faded_in() {
        let input = sine(440.0, 48_000.0, 48_000);
        let ptrs = [input.as_ptr()];
        let source = Source { channels: &ptrs, len: input.len(), sample_rate: 48_000.0 };
        let out = render(&source, 1.0 / 0.75, 2_000, 2_000);
        let early = out[..300].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(early > 0.8, "{early}");
    }
}
