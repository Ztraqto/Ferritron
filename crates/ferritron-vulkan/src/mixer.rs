//! A matrix mixer on Tensor cores (design doc 9.8): every output is a
//! weighted sum of every input, for hundreds of channels at once (spatial
//! audio, large routing matrices). See `shaders/mixer.slang`.
//!
//! The work is a matrix product, which Tensor cores do in f16 with f32
//! sums: about three decimal digits per weight and sample, plenty for
//! mixing, not for feedback paths.

use crate::{Buffer, Commands, Gpu, Pipeline, Place};
use ash::vk;
use half::f16;
use std::sync::Arc;

const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mixer.spv"));

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    inputs: u32,
    outputs: u32,
    frames: u32,
    pad: u32,
}

fn pad16(n: usize) -> usize {
    n.div_ceil(16).max(1) * 16
}

/// `out = matrix · in` on Tensor cores. Waits for the GPU in
/// [`process`](Self::process) (tens of microseconds plus the work).
pub struct TensorMixer {
    gpu: Arc<Gpu>,
    pipeline: Pipeline,
    commands: Commands,
    matrix: Buffer,
    input: Buffer,
    output: Buffer,
    inputs: usize,
    outputs: usize,
    max_frames: usize,
    /// Padded channel counts.
    pin: usize,
    pout: usize,
}

unsafe impl Send for TensorMixer {}

impl TensorMixer {
    /// A mixer from `inputs` to `outputs` channels, for blocks of up to
    /// `max_frames`. All weights start at zero. Fails without Tensor cores
    /// (VK_KHR_cooperative_matrix with 16×16×16 f16 tiles).
    pub fn new(inputs: usize, outputs: usize, max_frames: usize) -> Result<Self, String> {
        let gpu = Gpu::get()?;
        if !gpu.features.cooperative_matrix || !gpu.supports_f16_tiles() {
            return Err("no Tensor cores (cooperative matrices with 16x16 f16 tiles)".into());
        }
        let (pin, pout, pframes) = (pad16(inputs), pad16(outputs), pad16(max_frames));
        let storage = vk::BufferUsageFlags::STORAGE_BUFFER;
        let matrix = Buffer::new(&gpu, pout * pin * 2, storage, Place::Upload)?;
        let input = Buffer::new(&gpu, pin * pframes * 2, storage, Place::Upload)?;
        let output = Buffer::new(&gpu, pout * pframes * 4, storage, Place::Readback)?;
        let pipeline = Pipeline::new(&gpu, SPIRV, &[&matrix, &input, &output], std::mem::size_of::<Params>() as u32)?;
        let commands = Commands::new(&gpu, 1)?;
        Ok(Self { gpu, pipeline, commands, matrix, input, output, inputs, outputs, max_frames, pin, pout })
    }

    pub fn adapter(&self) -> &str {
        self.gpu.name()
    }

    /// True once the device this mixer runs on is lost (see [`Gpu::get`]). A
    /// lost mixer is silent until [`revive`](Self::revive) builds it again.
    pub fn lost(&self) -> bool {
        self.commands.lost()
    }

    /// Marks this mixer's device as lost, as if the device had been lost (for
    /// tests; see [`PersistentModalBank::simulate_loss`](crate::PersistentModalBank::simulate_loss)).
    #[doc(hidden)]
    pub fn simulate_loss(&self) {
        self.gpu.lost.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Builds the mixer again after its device was lost (see
    /// [`PersistentModalBank::revive`](crate::PersistentModalBank::revive)):
    /// the weights are kept. Consumes the mixer; off the audio thread.
    pub fn revive(self) -> Result<Self, String> {
        let (inputs, outputs, max_frames) = (self.inputs, self.outputs, self.max_frames);
        let count = self.pin * self.pout;
        let mut weights = vec![f16::ZERO; count];
        unsafe { std::ptr::copy_nonoverlapping(self.matrix.slice::<f16>(), weights.as_mut_ptr(), count) };
        drop(self);
        let fresh = Self::new(inputs, outputs, max_frames)?;
        unsafe { std::ptr::copy_nonoverlapping(weights.as_ptr(), fresh.matrix.slice::<f16>(), count) };
        Ok(fresh)
    }

    /// Sets the weight of input `input` in output `output`.
    pub fn set_gain(&mut self, output: usize, input: usize, gain: f32) {
        if output < self.outputs && input < self.inputs {
            unsafe { self.matrix.slice::<f16>().add(output * self.pin + input).write(f16::from_f32(gain)) };
        }
    }

    /// Sets every weight: `weights[output * inputs + input]`.
    pub fn set_matrix(&mut self, weights: &[f32]) {
        for o in 0..self.outputs {
            for i in 0..self.inputs {
                self.set_gain(o, i, weights.get(o * self.inputs + i).copied().unwrap_or(0.0));
            }
        }
    }

    /// Mixes one block (at most `max_frames`): `inputs` channels in,
    /// `outputs` channels out.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let n = inputs.iter().map(|c| c.len()).chain(outputs.iter().map(|c| c.len())).min().unwrap_or(0).min(self.max_frames);
        if n == 0 {
            return;
        }
        if self.commands.lost() {
            // The device is gone: silence, not the input (the mixer is not a dry path).
            outputs.iter_mut().for_each(|out| out[..n].fill(0.0));
            return;
        }
        let frames = pad16(n);
        let input = self.input.slice::<f16>();
        for i in 0..self.pin {
            let row = unsafe { std::slice::from_raw_parts_mut(input.add(i * frames), frames) };
            match inputs.get(i).filter(|_| i < self.inputs) {
                Some(data) => {
                    for (d, &s) in row.iter_mut().zip(&data[..n]) {
                        *d = f16::from_f32(s);
                    }
                    row[n..].fill(f16::ZERO);
                }
                None => row.fill(f16::ZERO),
            }
        }
        let params = Params { inputs: self.pin as u32, outputs: self.pout as u32, frames: frames as u32, pad: 0 };
        let device = &self.gpu.device;
        let cb = self.commands.buffers[0];
        unsafe {
            let _ = device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT));
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline.pipeline);
            device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline.layout, 0, &[self.pipeline.set], &[]);
            let bytes = std::slice::from_raw_parts(&params as *const Params as *const u8, std::mem::size_of::<Params>());
            device.cmd_push_constants(cb, self.pipeline.layout, vk::ShaderStageFlags::COMPUTE, 0, bytes);
            device.cmd_dispatch(cb, (frames / 16) as u32, (self.pout / 16) as u32, 1);
            let _ = device.end_command_buffer(cb);
        }
        if self.commands.submit(0).is_err() || !self.commands.wait(0) {
            // Submission failed, or the GPU did not finish in time: silence.
            outputs.iter_mut().for_each(|out| out[..n].fill(0.0));
            return;
        }
        let output = self.output.slice::<f32>();
        for (o, out) in outputs.iter_mut().take(self.outputs).enumerate() {
            let row = unsafe { std::slice::from_raw_parts(output.add(o * frames), n) };
            out[..n].copy_from_slice(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn mixes_like_the_cpu_within_f16_precision() {
        let _serial = crate::serial();
        let (inputs, outputs, frames) = (192, 160, 512);
        let mut mixer = match TensorMixer::new(inputs, outputs, frames) {
            Ok(m) => m,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let weights: Vec<f32> = (0..inputs * outputs).map(|k| ((k * 7919 % 1000) as f32 / 1000.0 - 0.5) / 16.0).collect();
        mixer.set_matrix(&weights);
        let input: Vec<Vec<f32>> = (0..inputs).map(|c| (0..frames).map(|f| ((f * (c + 3)) as f32 * 0.01).sin() * 0.5).collect()).collect();
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let mut got = vec![vec![0.0f32; frames]; outputs];
        let begun = Instant::now();
        {
            let mut outs: Vec<&mut [f32]> = got.iter_mut().map(|c| &mut c[..]).collect();
            mixer.process(&ins, &mut outs);
        }
        let gpu_time = begun.elapsed();
        let begun = Instant::now();
        let mut want = vec![vec![0.0f32; frames]; outputs];
        for o in 0..outputs {
            for i in 0..inputs {
                let w = weights[o * inputs + i];
                for f in 0..frames {
                    want[o][f] += w * input[i][f];
                }
            }
        }
        let cpu_time = begun.elapsed();
        let peak = want.iter().flatten().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = got.iter().flatten().zip(want.iter().flatten()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        eprintln!("tensor mixer on {}: {inputs}x{outputs}x{frames} in {gpu_time:?} (CPU {cpu_time:?}), worst {worst} of peak {peak}", mixer.adapter());
        assert!(peak > 0.1 && worst < 5e-3 * peak, "off by {worst} (peak {peak})");
        // A block shorter than a tile, and fewer channels than the mixer has.
        let mut short = vec![vec![0.0f32; 5]; outputs];
        let mut outs: Vec<&mut [f32]> = short.iter_mut().map(|c| &mut c[..]).collect();
        let few: Vec<&[f32]> = input.iter().map(|c| &c[..5]).collect();
        mixer.process(&few, &mut outs);
        assert!((short[0][3] - want[0][3]).abs() < 5e-3 * peak);
    }

    /// V1 (rebuild, simulated loss): a lost mixer is silent; `revive` builds it
    /// again on a new device with the same weights.
    #[test]
    fn a_lost_mixer_is_built_again_with_its_weights() {
        let _serial = crate::serial();
        let (inputs, outputs, frames) = (32, 16, 64);
        let mut mixer = match TensorMixer::new(inputs, outputs, frames) {
            Ok(m) => m,
            Err(e) => return eprintln!("skipping: {e}"),
        };
        let weights: Vec<f32> = (0..inputs * outputs).map(|k| ((k * 7919 % 1000) as f32 / 1000.0 - 0.5) / 8.0).collect();
        mixer.set_matrix(&weights);
        let before = mixer.gpu.generation();
        mixer.gpu.lost.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(mixer.lost(), "the mixer reports its lost device");
        let input: Vec<Vec<f32>> = (0..inputs).map(|c| (0..frames).map(|f| ((f * (c + 3)) as f32 * 0.01).sin() * 0.5).collect()).collect();
        let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let mut got = vec![vec![1.0f32; frames]; outputs];
        {
            let mut outs: Vec<&mut [f32]> = got.iter_mut().map(|c| &mut c[..]).collect();
            mixer.process(&ins, &mut outs);
        }
        assert!(got.iter().flatten().all(|&v| v == 0.0), "a lost mixer is silent");
        let mut mixer = mixer.revive().expect("built again");
        assert!(mixer.gpu.generation() > before, "built again on a new device");
        let mut got = vec![vec![0.0f32; frames]; outputs];
        {
            let mut outs: Vec<&mut [f32]> = got.iter_mut().map(|c| &mut c[..]).collect();
            mixer.process(&ins, &mut outs);
        }
        let mut want = vec![vec![0.0f32; frames]; outputs];
        for o in 0..outputs {
            for i in 0..inputs {
                for f in 0..frames {
                    want[o][f] += weights[o * inputs + i] * input[i][f];
                }
            }
        }
        let peak = want.iter().flatten().map(|v| v.abs()).fold(0.0f32, f32::max);
        let worst = got.iter().flatten().zip(want.iter().flatten()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.01 && worst < 5e-3 * peak, "off by {worst} (peak {peak})");
    }
}
