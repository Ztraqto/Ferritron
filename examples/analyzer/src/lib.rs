//! A spectrum analyzer: the audio passes through untouched, and what the editor shows is
//! computed off the audio thread.
//!
//! ```text
//!  audio thread                 worker thread (every 20 ms)             editor
//!  process: mono mix            tick: read the tap, window,
//!   -> tap "wave" (ring) ----->  Fft (ferritron::dsp), dB
//!  process: read the Swap  <---  Swap (triple buffer)
//!   -> stream "spectrum" (written only while an editor is open) -----> ferritron.onStream
//! ```
//!
//! * `[streams] wave = { kind = "tap" }` is the single-producer ring the worker reads.
//! * `#[ferritron::plugin(worker = SpectrumWorker)]` runs `SpectrumWorker` on Ferritron's
//!   worker thread; it hears about parameter changes through `params_changed`.
//! * The worker's result travels to the audio thread in a `Swap`, whose reader the worker
//!   hands over with `attach` / `Handoff` before `Process::init` runs.
//! * `[streams] spectrum` carries it on to the editor (web: `ferritron.onStream`; a GPU
//!   editor reads `ui.stream("spectrum")`).

use std::sync::Arc;
use std::time::Duration;

use ferritron::dsp::Fft;
use ferritron::prelude::*;
use ferritron::util::{Swap, SwapReader, SwapWriter};
use ferritron::{ChangedParams, Handoff, TapReader, Worker, WorkerContext};

/// FFT length: 4096 samples (85 ms at 48 kHz) give 2048 values for the stream.
const N: usize = 4096;
const BINS: usize = N / 2;

#[derive(Default)]
struct Analyzer {
    /// The worker's newest spectrum (dB).
    spectrum: Option<SwapReader<Vec<f32>>>,
    mono: Vec<f32>,
}

#[ferritron::plugin(worker = SpectrumWorker)]
impl Process for Analyzer {
    fn init(&mut self, instance: &ferritron::InstanceHandle) {
        // The worker was created first and left the reader for us.
        if let Some(handoff) = instance.shared::<Handoff<SwapReader<Vec<f32>>>>() {
            self.spectrum = handoff.take();
        }
    }

    fn activate(&mut self, setup: &Setup) {
        self.mono = vec![0.0; setup.max_block_size];
    }

    fn process(&mut self, ctx: &mut Context) {
        // Nobody sees the analysis of an editor that is closed: do nothing (the audio is untouched).
        if !ctx.editor_open() {
            return;
        }
        let frames = {
            let io = ctx.io();
            let frames = io.frames().min(self.mono.len());
            let channels = io.num_channels().max(1);
            self.mono[..frames].fill(0.0);
            for c in 0..channels {
                for (m, s) in self.mono[..frames].iter_mut().zip(io.channel_mut(c).iter()) {
                    *m += *s / channels as f32;
                }
            }
            frames
        };
        ctx.streams().wave.write(&self.mono[..frames]);
        if let Some(reader) = &mut self.spectrum {
            ctx.streams().spectrum.write(reader.read());
        }
    }
}

struct SpectrumWorker {
    tap: Option<TapReader>,
    fft: Fft,
    window: Vec<f32>,
    /// The newest `N` samples.
    history: Vec<f32>,
    chunk: Vec<f32>,
    frame: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    out: SwapWriter<Vec<f32>>,
    floor: f32,
}

impl Worker for SpectrumWorker {
    type Plugin = Analyzer;
    const MIN_INTERVAL: Duration = Duration::from_millis(20);

    fn new(ctx: &mut WorkerContext<'_, Analyzer>) -> Self {
        let (out, reader) = Swap::new(|| vec![-120.0f32; BINS]);
        ctx.instance().attach(Arc::new(Handoff::new(reader)));
        let fft = Fft::new(N);
        let bins = fft.bins();
        SpectrumWorker {
            tap: ctx.tap("wave"),
            fft,
            // Hann window
            window: (0..N).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos()).collect(),
            history: vec![0.0; N],
            chunk: vec![0.0; N],
            frame: vec![0.0; N],
            re: vec![0.0; bins],
            im: vec![0.0; bins],
            out,
            floor: ctx.params().floor.value(),
        }
    }

    fn params_changed(&mut self, ctx: &mut WorkerContext<'_, Analyzer>, changed: &ChangedParams<'_>) {
        if changed.contains(&ctx.params().floor) {
            self.floor = ctx.params().floor.value();
        }
    }

    fn tick(&mut self, _ctx: &mut WorkerContext<'_, Analyzer>) {
        let Some(tap) = &mut self.tap else { return };
        let mut got = false;
        loop {
            let n = tap.read(&mut self.chunk);
            if n == 0 {
                break;
            }
            got = true;
            self.history.copy_within(n.., 0);
            self.history[N - n..].copy_from_slice(&self.chunk[..n]);
        }
        if !got {
            return;
        }
        for ((f, h), w) in self.frame.iter_mut().zip(&self.history).zip(&self.window) {
            *f = h * w;
        }
        self.fft.forward(&self.frame, &mut self.re, &mut self.im);
        // A full-scale sine gives N / 4 in its bin with a Hann window: 0 dB.
        let scale = 4.0 / N as f32;
        let floor = self.floor;
        let back = self.out.back();
        for (k, v) in back.iter_mut().enumerate() {
            let mag = self.re[k].hypot(self.im[k]) * scale;
            *v = (20.0 * (mag + 1e-9).log10()).max(floor);
        }
        self.out.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferritron::testing::Harness;

    #[global_allocator]
    static ALLOC: ferritron::testing::AllocGuard = ferritron::testing::AllocGuard;

    const RATE: f64 = 48_000.0;

    /// The worker pool is process-wide: the tests that count its workers take turns.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Feeds `blocks` blocks of a sine (frequency `hz`, amplitude 1) through the plugin.
    fn feed(h: &mut Harness<Analyzer>, hz: f64, blocks: usize, phase: &mut f64) {
        for _ in 0..blocks {
            let mut l = vec![0.0f32; 256];
            for s in l.iter_mut() {
                *s = (*phase * std::f64::consts::TAU).sin() as f32;
                *phase = (*phase + hz / RATE).fract();
            }
            let mut r = l.clone();
            assert!(h.process(&mut [&mut l, &mut r]));
        }
    }

    /// Runs blocks and worker turns until the stream has a full spectrum.
    fn settle(h: &mut Harness<Analyzer>, hz: f64) -> Vec<f32> {
        let mut phase = 0.0;
        feed(h, hz, 20, &mut phase); // 5120 samples: more than one FFT window
        h.run_workers(); // the worker reads the tap and publishes the spectrum
        feed(h, hz, 1, &mut phase); // the audio thread forwards it to the stream
        h.stream("spectrum")
    }

    #[test]
    fn a_sine_shows_up_as_a_peak_at_its_bin_in_the_stream() {
        let _turn = serial();
        let mut h = Harness::<Analyzer>::new(RATE, 256);
        h.set_editor_open(true);
        let spectrum = settle(&mut h, 1000.0);
        assert_eq!(spectrum.len(), 2048);
        let (peak, level) = spectrum.iter().copied().enumerate().fold((0, f32::MIN), |m, (i, v)| if v > m.1 { (i, v) } else { m });
        let expect = (1000.0 * N as f64 / RATE).round() as usize; // bin 85
        assert!(peak.abs_diff(expect) <= 1, "peak at bin {peak}, expected {expect}");
        assert!(level > -3.0 && level <= 0.5, "a full-scale sine is about 0 dB: {level}");
        // far from the peak the spectrum is down at the floor of the window's leakage
        assert!(spectrum[1500] < -60.0, "{}", spectrum[1500]);
    }

    #[test]
    fn nothing_is_made_while_the_editor_is_closed() {
        let _turn = serial();
        let mut h = Harness::<Analyzer>::new(RATE, 256);
        let mut phase = 0.0;
        feed(&mut h, 1000.0, 20, &mut phase);
        h.run_workers();
        feed(&mut h, 1000.0, 2, &mut phase);
        assert!(h.stream("spectrum").is_empty(), "the stream drops writes while no editor is open");
        h.set_editor_open(true);
        assert!(!settle(&mut h, 1000.0).is_empty(), "and fills after the editor opens");
        h.set_editor_open(false);
        let mut l = vec![0.5f32; 256];
        let mut r = l.clone();
        h.process(&mut [&mut l, &mut r]);
        assert_eq!(l[0], 0.5, "the audio passes through untouched in any case");
    }

    #[test]
    fn a_parameter_change_reaches_the_worker() {
        let _turn = serial();
        let mut h = Harness::<Analyzer>::new(RATE, 256);
        h.set_editor_open(true);
        h.set_param("floor", -50.0);
        let spectrum = settle(&mut h, 1000.0);
        let min = spectrum.iter().copied().fold(f32::MAX, f32::min);
        assert_eq!(min, -50.0, "the worker clamps to the floor it was told about");
    }

    #[test]
    fn the_audio_thread_allocates_nothing_while_analyzing() {
        let _turn = serial();
        let mut h = Harness::<Analyzer>::new(RATE, 256);
        h.set_editor_open(true);
        let mut phase = 0.0;
        // warm up: the first blocks may touch lazy state
        feed(&mut h, 1000.0, 4, &mut phase);
        h.run_workers();
        feed(&mut h, 1000.0, 40, &mut phase);
        h.assert_no_alloc();
    }

    #[test]
    fn the_worker_thread_alone_keeps_the_stream_flowing() {
        let _turn = serial();
        // No `run_workers`: the real worker thread (every 20 ms) does the FFT.
        let mut h = Harness::<Analyzer>::new(RATE, 256);
        h.set_editor_open(true);
        let mut phase = 0.0;
        let start = std::time::Instant::now();
        let mut seen = 0;
        let mut reader = h.instance().stream("spectrum").expect("the stream exists");
        let mut out = Vec::new();
        while seen < 3 && start.elapsed() < Duration::from_secs(10) {
            feed(&mut h, 1000.0, 4, &mut phase);
            std::thread::sleep(Duration::from_millis(5));
            if reader.read_new(&mut out) && out.len() == 2048 && out.iter().any(|v| *v > -40.0) {
                seen += 1;
            }
        }
        assert!(seen >= 3, "the stream delivered spectra with signal ({seen})");
    }

    #[test]
    fn the_worker_leaves_the_pool_with_its_instance() {
        let _turn = serial();
        let before = ferritron::worker::registered();
        let h = Harness::<Analyzer>::new(RATE, 256);
        assert_eq!(ferritron::worker::registered(), before + 1);
        drop(h);
        assert_eq!(ferritron::worker::registered(), before);
    }

    #[test]
    fn a_gpu_editor_reads_the_stream_through_ui() {
        let _turn = serial();
        use ferritron::gpu_view::Ui;
        let mut h = Harness::<Analyzer>::new(RATE, 256);
        let mut ui = Ui::for_tests::<Analyzer>(480.0, 240.0);
        // As the shim does for a real editor: link the instance (this opens the editor).
        ui.attach_instance_for_tests(h.instance().clone());
        assert!(h.instance().is_editor_open());
        assert!(ui.stream("spectrum").is_none(), "nothing yet");
        let mut phase = 0.0;
        feed(&mut h, 1000.0, 20, &mut phase);
        h.run_workers();
        feed(&mut h, 1000.0, 1, &mut phase);
        assert!(ui.refresh_streams(), "new values arrived");
        let s = ui.stream("spectrum").expect("the newest values");
        assert_eq!(s.len(), 2048);
        assert!(s.iter().any(|v| *v > -10.0));
        assert!(!ui.refresh_streams(), "nothing new until the next write");
        assert!(ui.stream("no_such_stream").is_none());
    }
}
