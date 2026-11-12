//! `cargo run --release -p ferritron-bench-fx -- [s1|s2|s6 ...] [--full]
//!  [--json out.json] [--compare baseline.json] [--golden dir]`
//!
//! Scenarios: s1 (no automation), s2 (8 parameters automated every sample),
//! s6 (silence, plugin asleep). Each processes 1 s of stereo audio per
//! (rate, block) pair and reports real-time ratio, per-block mean/p99/max,
//! `process` calls and audio-thread allocations. These are measurements of
//! one machine; the framework version is recorded in the JSON.

use ferritron::testing::{AllocGuard, Harness};
use ferritron_bench_fx::{Fx, process_calls};
use serde_json::{Value, json};
use std::time::Instant;

#[global_allocator]
static ALLOC: AllocGuard = AllocGuard;

struct Row {
    scenario: &'static str,
    rate: f64,
    block: usize,
    mean_us: f64,
    p99_us: f64,
    max_us: f64,
    realtime_x: f64,
    calls_per_block: f64,
    allocs: u64,
    audio: Vec<f32>,
}

fn run(scenario: &'static str, rate: f64, block: usize) -> Row {
    let mut h = Harness::<Fx>::new(rate, block);
    let blocks = (rate as usize / block).max(1);
    let keys = ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7"];
    let (mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block]);
    let fill = |l: &mut [f32], r: &mut [f32], b: usize| {
        for (i, (x, y)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            let v = if scenario == "s6" { 0.0 } else { (((b * block + i) % 97) as f32 / 97.0 - 0.5) * 0.5 };
            *x = v;
            *y = v;
        }
    };
    // Silence needs a few seconds before the plugin sleeps (`Tail::Auto`).
    let warm = if scenario == "s6" { (rate as usize * 3 / block).max(20) } else { 20 };
    for b in 0..warm {
        fill(&mut l, &mut r, b);
        h.process(&mut [&mut l, &mut r]);
    }
    let calls0 = process_calls();
    let allocs0 = h.audio_allocations();
    let mut times = Vec::with_capacity(blocks);
    let mut audio = Vec::new();
    for b in 0..blocks {
        fill(&mut l, &mut r, b);
        if scenario == "s2" {
            for (k, key) in keys.iter().enumerate() {
                for i in 0..block as u32 {
                    let v = ((b * block + i as usize + k * 13) % 200) as f64 / 200.0 * 2.0 - 1.0;
                    h.automate(i, key, v);
                }
            }
        }
        let t = Instant::now();
        let ok = h.process(&mut [&mut l, &mut r]);
        times.push(t.elapsed().as_secs_f64() * 1e6);
        assert!(ok, "plugin panicked");
        if scenario != "s6" {
            audio.extend_from_slice(&l);
        }
    }
    let calls = process_calls() - calls0;
    let allocs = h.audio_allocations() - allocs0;
    let total_s = times.iter().sum::<f64>() / 1e6;
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    Row {
        scenario,
        rate,
        block,
        mean_us: mean,
        p99_us: times[((times.len() - 1) as f64 * 0.99) as usize],
        max_us: *times.last().unwrap(),
        realtime_x: (blocks * block) as f64 / rate / total_s,
        calls_per_block: calls as f64 / blocks as f64,
        allocs,
        audio,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let asked: Vec<&String> = args.iter().filter(|a| matches!(a.as_str(), "s1" | "s2" | "s6")).collect();
    let scenarios: Vec<&'static str> = ["s1", "s2", "s6"].into_iter().filter(|s| asked.is_empty() || asked.iter().any(|a| a.as_str() == *s)).collect();
    let (rates, blocks): (&[f64], &[usize]) = if args.iter().any(|a| a == "--full") {
        (&[44100.0, 48000.0, 96000.0, 192000.0], &[32, 64, 128, 256, 512, 1024])
    } else {
        (&[48000.0], &[64, 512])
    };
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("Ferritron {} framework-cost benchmark ({cores} logical cores)", ferritron::VERSION);
    println!("{:<4} {:>7} {:>6} {:>10} {:>10} {:>10} {:>10} {:>11} {:>7}", "scn", "rate", "block", "mean us", "p99 us", "max us", "x realtime", "calls/block", "allocs");
    let mut rows = Vec::new();
    for &s in &scenarios {
        for &rate in rates {
            for &block in blocks {
                let row = run(s, rate, block);
                println!(
                    "{:<4} {:>7} {:>6} {:>10.2} {:>10.2} {:>10.2} {:>10.0} {:>11.2} {:>7}",
                    row.scenario, row.rate, row.block, row.mean_us, row.p99_us, row.max_us, row.realtime_x, row.calls_per_block, row.allocs
                );
                rows.push(row);
            }
        }
    }
    if let Some(dir) = flag("--golden") {
        golden(&dir, &rows);
    }
    if let Some(path) = flag("--json") {
        let result = json!({
            "ferritron": ferritron::VERSION,
            "cores": cores,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "rows": rows.iter().map(|r| json!({
                "scenario": r.scenario, "rate": r.rate, "block": r.block, "mean_us": r.mean_us, "p99_us": r.p99_us,
                "max_us": r.max_us, "realtime_x": r.realtime_x, "calls_per_block": r.calls_per_block, "allocs": r.allocs,
            })).collect::<Vec<_>>(),
        });
        std::fs::write(&path, serde_json::to_string_pretty(&result).unwrap()).expect("write json");
        println!("wrote {path}");
    }
    if let Some(path) = flag("--compare") {
        compare(&path, &rows);
    }
}

/// Writes each row's output to `dir` when absent, else compares with it.
fn golden(dir: &str, rows: &[Row]) {
    std::fs::create_dir_all(dir).expect("golden dir");
    for r in rows.iter().filter(|r| !r.audio.is_empty() && r.rate == 48000.0) {
        let path = format!("{dir}/{}_{}.f32", r.scenario, r.block);
        match std::fs::read(&path) {
            Err(_) => {
                let bytes: Vec<u8> = r.audio.iter().flat_map(|f| f.to_le_bytes()).collect();
                std::fs::write(&path, bytes).expect("write golden");
                println!("golden written: {path}");
            }
            Ok(old) => {
                let old: Vec<f32> = old.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                let (mut worst, mut first) = (0.0f32, None);
                for (i, (a, b)) in old.iter().zip(&r.audio).enumerate() {
                    let d = (a - b).abs();
                    if d > 1e-6 && first.is_none() {
                        first = Some(i);
                    }
                    worst = worst.max(d);
                }
                println!("golden {path}: max error {worst:.3e}, first difference at {first:?}, lengths {} vs {}", old.len(), r.audio.len());
            }
        }
    }
}

fn compare(path: &str, rows: &[Row]) {
    let base: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("read baseline")).expect("baseline json");
    println!("\nvs {path} (Ferritron {}): mean time now / then", base["ferritron"]);
    for r in rows {
        let old = base["rows"].as_array().into_iter().flatten().find(|o| o["scenario"] == r.scenario && o["rate"] == r.rate && o["block"] == r.block);
        if let Some(o) = old {
            let then = o["mean_us"].as_f64().unwrap();
            println!(
                "  {} {:>6} {:>5}: {:>8.2} us / {:>8.2} us = {:.2}x; calls/block {:.2} -> {:.2}",
                r.scenario, r.rate, r.block, r.mean_us, then, r.mean_us / then, o["calls_per_block"].as_f64().unwrap(), r.calls_per_block
            );
        }
    }
}
