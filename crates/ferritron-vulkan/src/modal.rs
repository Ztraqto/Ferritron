//! A persistent modal bank: thousands of resonators (one biquad each) on
//! the GPU, fed block by block through memory while the kernel keeps
//! running (design doc 9.3). See `shaders/modal.slang` for the GPU side.
//!
//! Per block the host writes the input and a block number into GPU memory
//! it can map, then waits (spinning, a few tens of microseconds) until
//! every workgroup has written its partial mix and flagged the block done.
//! No submission, no readback copy: the round trip is the PCIe latency.

use crate::{Buffer, Commands, Gpu, Pipeline, Place};
use ash::vk;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{Ordering, fence};
use std::time::{Duration, Instant};

const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/modal.spv"));
const GROUP: usize = 64;
const MAX_OUTPUTS: usize = 8;
/// Runs kept on the queue: when one hands over, the next is already there.
const RUNS: usize = 2;

/// One persistent kernel per computer. Two on one queue wait for each other
/// (each one's runs stand in front of the other's); one in another process
/// makes this one wait for GPU time slices, and the other way round. So a
/// second bank is refused, here or in any other process, and its caller
/// uses another path. The claim is an exclusive lock on a file, which the
/// system releases even if the process crashes.
static CLAIM: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn claim() -> Result<(), String> {
    let mut held = CLAIM.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.is_some() {
        return Err("another persistent kernel is already running in this process".into());
    }
    let path = std::env::temp_dir().join("ferritron-persistent-kernel.lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {
            *held = Some(file);
            Ok(())
        }
        Err(_) => Err("another persistent kernel is already running on this computer".into()),
    }
}

fn release() {
    if let Some(file) = CLAIM.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
        let _ = file.unlock();
    }
}

/// One resonator: a biquad (b0, b1, b2, a1, a2) from an input channel to an
/// output channel.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ModalLane {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
    pub input: u32,
    pub output: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    lanes: u32,
    inputs: u32,
    outputs: u32,
    max_frames: u32,
    groups: u32,
    max_blocks: u32,
    max_spins: u32,
    pad: u32,
}

/// Thousands of resonators with a persistent kernel: blocks of any size
/// (64 samples and less) on the GPU, synchronously, without latency. Use
/// [`process`](Self::process) from the audio thread; it reports a block the
/// GPU did not finish in time (then the output is silent).
pub struct PersistentModalBank {
    gpu: Arc<Gpu>,
    _pipeline: Pipeline,
    commands: Commands,
    in_flight: [bool; RUNS],
    next_run: usize,
    control: Buffer,
    input: Buffer,
    lanes: Buffer,
    partial: Buffer,
    done: Buffer,
    _state: Buffer,
    _progress: Buffer,
    params: Params,
    seq: u32,
    deadline: Duration,
    misses: u64,
}

// The mapped pointers are only used by the owner.
unsafe impl Send for PersistentModalBank {}

impl PersistentModalBank {
    /// Prepares the bank; blocks of up to `max_frames`. `deadline` is how
    /// long `process` waits for the GPU (keep it well inside the block's
    /// real-time budget).
    pub fn new(lanes: &[ModalLane], inputs: usize, outputs: usize, max_frames: usize, deadline: Duration) -> Result<Self, String> {
        if outputs == 0 || outputs > MAX_OUTPUTS {
            return Err(format!("a modal bank has 1 to {MAX_OUTPUTS} outputs"));
        }
        if lanes.iter().any(|l| l.input as usize >= inputs || l.output as usize >= outputs) {
            return Err("a lane reads or writes a channel the bank does not have".into());
        }
        let gpu = Gpu::get()?;
        claim()?;
        // Released by Drop once the bank exists; until then, on any error.
        struct Claim(bool);
        impl Drop for Claim {
            fn drop(&mut self) {
                if self.0 {
                    release();
                }
            }
        }
        let mut claim = Claim(true);
        let groups = lanes.len().div_ceil(GROUP).max(1);
        let storage = vk::BufferUsageFlags::STORAGE_BUFFER;
        let control = Buffer::new(&gpu, 16, storage, Place::Upload)?;
        let input = Buffer::new(&gpu, inputs * max_frames * 4, storage, Place::Upload)?;
        let lane_buffer = Buffer::new(&gpu, groups * GROUP * std::mem::size_of::<ModalLane>(), storage, Place::Upload)?;
        let partial = Buffer::new(&gpu, groups * outputs * max_frames * 4, storage, Place::Readback)?;
        let done = Buffer::new(&gpu, groups * 4, storage, Place::Readback)?;
        let state = Buffer::new(&gpu, groups * GROUP * 8, storage, Place::Upload)?;
        let progress = Buffer::new(&gpu, groups * 8, storage, Place::Upload)?;
        unsafe { std::ptr::copy_nonoverlapping(lanes.as_ptr(), lane_buffer.slice::<ModalLane>(), lanes.len()) };
        let pipeline = Pipeline::new(&gpu, SPIRV, &[&control, &input, &lane_buffer, &partial, &done, &state, &progress], std::mem::size_of::<Params>() as u32)?;
        // A run lasts at most ~250 blocks of 64 frames at 48 kHz (~0.35 s),
        // or a few hundred ms idle: far from the 2 s after which Windows
        // resets the GPU. Hosts call process continuously, so a run is
        // relaunched (~0.3 ms) only after a real pause.
        let params = Params {
            lanes: lanes.len() as u32,
            inputs: inputs as u32,
            outputs: outputs as u32,
            max_frames: max_frames as u32,
            groups: groups as u32,
            max_blocks: 256,
            max_spins: 2_000_000,
            pad: 0,
        };
        let commands = Commands::new(&gpu, RUNS)?;
        let device = &gpu.device;
        for &cb in &commands.buffers {
            unsafe {
                device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).map_err(|e| e.to_string())?;
                // A run starts after the previous one ended (it reads the state that one saved).
                let barrier = vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE);
                device.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::COMPUTE_SHADER, vk::PipelineStageFlags::COMPUTE_SHADER, vk::DependencyFlags::empty(), &[barrier], &[], &[]);
                device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipeline.pipeline);
                device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, pipeline.layout, 0, &[pipeline.set], &[]);
                let bytes = std::slice::from_raw_parts(&params as *const Params as *const u8, std::mem::size_of::<Params>());
                device.cmd_push_constants(cb, pipeline.layout, vk::ShaderStageFlags::COMPUTE, 0, bytes);
                device.cmd_dispatch(cb, groups as u32, 1, 1);
                device.end_command_buffer(cb).map_err(|e| e.to_string())?;
            }
        }
        let mut bank = Self {
            gpu: gpu.clone(),
            _pipeline: pipeline,
            commands,
            in_flight: [false; RUNS],
            next_run: 0,
            control,
            input,
            lanes: lane_buffer,
            partial,
            done,
            _state: state,
            _progress: progress,
            params,
            seq: 0,
            deadline,
            misses: 0,
        };
        // Warm up: the first run pays the pipeline's first launch.
        let silence = vec![vec![0.0f32; max_frames]; inputs];
        let mut sink = vec![vec![0.0f32; max_frames]; outputs];
        let ins: Vec<&[f32]> = silence.iter().map(|c| &c[..]).collect();
        let mut outs: Vec<&mut [f32]> = sink.iter_mut().map(|c| &mut c[..]).collect();
        let saved = bank.deadline;
        bank.deadline = Duration::from_secs(2);
        let ok = bank.process(&ins, &mut outs);
        bank.deadline = saved;
        bank.misses = 0;
        claim.0 = false; // the bank's Drop releases it from now on
        if !ok {
            return Err("the persistent kernel did not answer (the GPU or driver does not let host writes reach a running kernel)".into());
        }
        Ok(bank)
    }

    pub fn adapter(&self) -> &str {
        self.gpu.name()
    }

    /// True once the device this bank runs on is lost (see [`Gpu::get`]). A
    /// lost bank is silent until [`revive`](Self::revive) builds it again.
    pub fn lost(&self) -> bool {
        self.commands.lost()
    }

    /// Marks this bank's device as lost, as if the device had been lost (for
    /// tests: no device is lost, and the mark holds for every user of the
    /// process's device).
    #[doc(hidden)]
    pub fn simulate_loss(&self) {
        self.gpu.lost.store(true, Ordering::Relaxed);
    }

    /// Builds the bank again after its device was lost: on the current
    /// device, which is a new one if the lost one is still current (see
    /// [`Gpu::get`]). The lanes keep their current coefficients. Consumes the
    /// bank (its resources and the kernel claim are released first), and is
    /// called off the audio thread. On error the caller plays without it.
    pub fn revive(self) -> Result<Self, String> {
        let count = self.params.lanes as usize;
        let lanes: Vec<ModalLane> = {
            let src = self.lanes.slice::<ModalLane>();
            (0..count).map(|i| unsafe { *src.add(i) }).collect()
        };
        let (inputs, outputs, max_frames, deadline) = (self.params.inputs as usize, self.params.outputs as usize, self.params.max_frames as usize, self.deadline);
        drop(self);
        Self::new(&lanes, inputs, outputs, max_frames, deadline)
    }

    /// Blocks the GPU did not finish in time (they played silent).
    pub fn missed_blocks(&self) -> u64 {
        self.misses
    }

    /// Replaces a lane's coefficients; takes effect with the next block.
    pub fn set_lane(&mut self, lane: usize, coefficients: [f32; 5]) {
        if lane < self.params.lanes as usize {
            let lanes = self.lanes.slice::<ModalLane>();
            unsafe {
                let l = &mut *lanes.add(lane);
                (l.b0, l.b1, l.b2, l.a1, l.a2) = (coefficients[0], coefficients[1], coefficients[2], coefficients[3], coefficients[4]);
            }
        }
    }

    /// Makes sure the kernel is running and answering (off the audio
    /// thread, after setup): the first real block then pays no launch.
    pub fn settle(&mut self) -> bool {
        let p = self.params;
        let silence = vec![vec![0.0f32; p.max_frames as usize]; p.inputs as usize];
        let mut sink = vec![vec![0.0f32; p.max_frames as usize]; p.outputs as usize];
        let saved = (self.deadline, self.misses);
        self.deadline = Duration::from_secs(2);
        let mut ok = true;
        for _ in 0..3 {
            let ins: Vec<&[f32]> = silence.iter().map(|c| &c[..]).collect();
            let mut outs: Vec<&mut [f32]> = sink.iter_mut().map(|c| &mut c[..]).collect();
            ok &= self.process(&ins, &mut outs);
        }
        (self.deadline, self.misses) = saved;
        ok
    }

    /// Clears every lane's state, from the next block on.
    pub fn reset(&mut self) {
        let control = self.control.slice::<u32>();
        unsafe { control.add(3).write_volatile(control.add(3).read_volatile().wrapping_add(1)) };
        fence(Ordering::SeqCst);
    }

    /// Keeps a run on the GPU (and one queued behind it).
    fn keep_running(&mut self) {
        for i in 0..RUNS {
            if self.in_flight[i] && self.commands.finished(i) {
                self.in_flight[i] = false;
            }
        }
        while self.in_flight.iter().filter(|&&f| f).count() < RUNS {
            let i = self.next_run;
            if self.in_flight[i] {
                break;
            }
            if self.commands.submit(i).is_err() {
                break;
            }
            self.in_flight[i] = true;
            self.next_run = (i + 1) % RUNS;
        }
    }

    /// Filters one block (`inputs` channels in, `outputs` channels out, at
    /// most `max_frames`). Returns false if the GPU missed the deadline (the
    /// output is then silent).
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) -> bool {
        let p = self.params;
        let n = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0).min(p.max_frames as usize);
        if self.commands.lost() {
            // The device is gone: this block is silent, like a missed deadline.
            outputs.iter_mut().take(p.outputs as usize).for_each(|out| out[..n].fill(0.0));
            self.misses += 1;
            return false;
        }
        // Input and frame count first, then the block number that announces them.
        let input = self.input.slice::<f32>();
        for (ch, data) in inputs.iter().take(p.inputs as usize).enumerate() {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), input.add(ch * p.max_frames as usize), n) };
        }
        let control = self.control.slice::<u32>();
        unsafe { control.add(2).write_volatile(n as u32) };
        fence(Ordering::SeqCst); // also drains write-combining buffers
        self.keep_running();
        self.seq = self.seq.wrapping_add(1).max(1);
        unsafe { control.write_volatile(self.seq) };
        fence(Ordering::SeqCst);

        let done = self.done.slice::<u32>();
        let started = Instant::now();
        let all_done = |seq| (0..p.groups as usize).all(|g| unsafe { done.add(g).read_volatile() } == seq);
        let mut ok = true;
        while !all_done(self.seq) {
            if started.elapsed() > self.deadline {
                ok = false;
                break;
            }
            // A run that handed over (idle, or its block budget) is replaced.
            if self.in_flight.iter().zip(0..).any(|(&f, i)| f && self.commands.finished(i)) {
                self.keep_running();
            }
            std::hint::spin_loop();
        }
        fence(Ordering::Acquire);
        let partial = self.partial.slice::<f32>();
        for (o, out) in outputs.iter_mut().take(p.outputs as usize).enumerate() {
            let out = &mut out[..n];
            out.fill(0.0);
            if !ok {
                continue;
            }
            for g in 0..p.groups as usize {
                let row = unsafe { partial.add((g * p.outputs as usize + o) * p.max_frames as usize) };
                for (i, s) in out.iter_mut().enumerate() {
                    *s += unsafe { row.add(i).read_volatile() };
                }
            }
        }
        if !ok {
            self.misses += 1;
        }
        ok
    }
}

impl Drop for PersistentModalBank {
    fn drop(&mut self) {
        unsafe { self.control.slice::<u32>().add(1).write_volatile(1) };
        fence(Ordering::SeqCst);
        for i in 0..RUNS {
            self.commands.wait(i);
        }
        release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resonator(freq: f32, rate: f32, r: f32) -> [f32; 5] {
        let w = std::f32::consts::TAU * freq / rate;
        [1.0 - r, 0.0, 0.0, -2.0 * r * w.cos(), r * r]
    }

    /// The same lanes on the CPU (transposed direct form II, like the GPU).
    fn cpu(lanes: &[ModalLane], outputs: usize, input: &[Vec<f32>], state: &mut [[f32; 2]]) -> Vec<Vec<f32>> {
        let n = input[0].len();
        let mut out = vec![vec![0.0f32; n]; outputs];
        for (lane, z) in lanes.iter().zip(state.iter_mut()) {
            for i in 0..n {
                let x = input[lane.input as usize][i];
                let y = lane.b0 * x + z[0];
                z[0] = lane.b1 * x - lane.a1 * y + z[1];
                z[1] = lane.b2 * x - lane.a2 * y;
                out[lane.output as usize][i] += y;
            }
        }
        out
    }

    #[test]
    fn one_persistent_kernel_at_a_time() {
        let _serial = crate::serial();
        let lanes: Vec<ModalLane> = (0..256).map(|_| ModalLane { b0: 0.001, a1: -1.9, a2: 0.95, ..Default::default() }).collect();
        let first = match PersistentModalBank::new(&lanes, 1, 1, 256, Duration::from_millis(20)) {
            Ok(b) => b,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let second = PersistentModalBank::new(&lanes, 1, 1, 256, Duration::from_millis(20));
        assert!(second.is_err_and(|e| e.contains("already running")));
        drop(first);
        let mut again = PersistentModalBank::new(&lanes, 1, 1, 256, Duration::from_millis(20)).expect("free again after the first is dropped");
        let mut out = vec![0.0f32; 256];
        assert!(again.process(&[&[0.0f32; 256]], &mut [&mut out]));
    }

    /// V1 (rebuild, simulated loss): a lost bank is silent; `revive` builds it
    /// again on a new device (the loss is set by hand, the device is real),
    /// and it then gives the CPU's answer.
    #[test]
    fn a_lost_bank_is_built_again_and_answers_like_the_cpu() {
        let _serial = crate::serial();
        let lanes: Vec<ModalLane> = (0..512)
            .map(|i| {
                let [b0, b1, b2, a1, a2] = resonator(60.0 + 7.0 * i as f32, 48_000.0, 0.999);
                ModalLane { b0, b1, b2, a1, a2, input: 0, output: 0, pad: 0 }
            })
            .collect();
        let mut bank = match PersistentModalBank::new(&lanes, 1, 1, 64, Duration::from_millis(20)) {
            Ok(b) => b,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let before = bank.gpu.generation();
        bank.gpu.lost.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut out = vec![1.0f32; 64];
        assert!(!bank.process(&[&[1.0f32; 64]], &mut [&mut out]), "a lost bank does not answer");
        assert!(out.iter().all(|&v| v == 0.0), "a lost bank is silent");
        assert!(bank.lost(), "the bank reports its lost device");
        let mut bank = bank.revive().expect("built again");
        assert!(!bank.lost(), "the new bank is not on a lost device");
        assert!(bank.gpu.generation() > before, "built again on a new device");
        let input: Vec<f32> = (0..64).map(|i| if i == 3 { 1.0 } else { 0.0 }).collect();
        let mut state = vec![[0.0f32; 2]; lanes.len()];
        let want = cpu(&lanes, 1, std::slice::from_ref(&input), &mut state);
        let mut got = vec![0.0f32; 64];
        assert!(bank.process(&[&input], &mut [&mut got]), "the new bank answers");
        let peak = want[0].iter().map(|v| v.abs()).fold(1e-3f32, f32::max);
        let worst = got.iter().zip(&want[0]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3 * peak, "off by {worst} (peak {peak})");
    }

    #[test]
    fn blocks_after_an_idle_pause() {
        let _serial = crate::serial();
        let lanes: Vec<ModalLane> = (0..2048).map(|i| ModalLane { b0: 0.001, a1: -1.9, a2: 0.95, input: 0, output: (i % 2) as u32, ..Default::default() }).collect();
        let Ok(mut bank) = PersistentModalBank::new(&lanes, 1, 2, 64, Duration::from_millis(50)) else { return eprintln!("skipping") };
        let input = vec![0.0f32; 64];
        for pause in [0u64, 1, 10, 100, 400, 1000] {
            std::thread::sleep(Duration::from_millis(pause));
            let mut out = vec![vec![0.0f32; 64]; 2];
            let begun = Instant::now();
            let ok = {
                let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
                bank.process(&[&input], &mut outs)
            };
            eprintln!("after {pause} ms idle: {} us", begun.elapsed().as_micros());
            assert!(ok, "a block after {pause} ms idle missed the deadline");
        }
    }

    #[test]
    fn a_persistent_bank_matches_the_cpu_block_after_block() {
        let _serial = crate::serial();
        let lanes: Vec<ModalLane> = (0..2048)
            .map(|i| {
                let [b0, b1, b2, a1, a2] = resonator(60.0 + 7.0 * i as f32, 48_000.0, 0.9995);
                ModalLane { b0, b1, b2, a1, a2, input: (i % 2) as u32, output: (i % 2) as u32, pad: 0 }
            })
            .collect();
        let mut bank = match PersistentModalBank::new(&lanes, 2, 2, 64, Duration::from_millis(20)) {
            Ok(b) => b,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let mut state = vec![[0.0f32; 2]; lanes.len()];
        let mut times = Vec::new();
        for block in 0..400 {
            let input: Vec<Vec<f32>> = (0..2).map(|c| (0..64).map(|i| if (block * 64 + i) % 997 == c * 13 { 1.0 } else { 0.0 }).collect()).collect();
            let want = cpu(&lanes, 2, &input, &mut state);
            let mut got = vec![vec![0.0f32; 64]; 2];
            let begun = Instant::now();
            let ok = {
                let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
                let mut outs: Vec<&mut [f32]> = got.iter_mut().map(|c| &mut c[..]).collect();
                bank.process(&ins, &mut outs)
            };
            times.push(begun.elapsed().as_micros() as u64);
            assert!(ok, "block {block}: the GPU missed the deadline");
            for o in 0..2 {
                let worst = got[o].iter().zip(&want[o]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
                let peak = want[o].iter().map(|v| v.abs()).fold(1e-3f32, f32::max);
                assert!(worst < 1e-3 * peak.max(1.0), "block {block}, output {o}: off by {worst} (peak {peak})");
            }
        }
        // A reset silences the ringing resonators at once.
        bank.reset();
        let silence = vec![vec![0.0f32; 64]; 2];
        let mut out = vec![vec![1.0f32; 64]; 2];
        {
            let ins: Vec<&[f32]> = silence.iter().map(|c| &c[..]).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
            assert!(bank.process(&ins, &mut outs));
        }
        assert!(out.iter().flatten().all(|&v| v == 0.0), "reset clears the state");
        times.sort();
        eprintln!("persistent kernel on {}: 64 frames x 2048 resonators, median {} us, 99% {} us, max {} us", bank.adapter(), times[times.len() / 2], times[times.len() * 99 / 100], times[times.len() - 1]);
    }
}
