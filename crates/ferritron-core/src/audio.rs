/// The audio buffers of one processing block, processed in place.
///
/// Inputs have already been copied to the outputs, so a plugin reads and
/// writes the same samples.
pub struct AudioIo<'a> {
    channels: &'a [*mut f32],
    frames: usize,
}

impl<'a> AudioIo<'a> {
    /// # Safety
    ///
    /// Every pointer in `channels` must be valid for reads and writes of
    /// `frames` samples for `'a`, and no two pointers may overlap.
    pub unsafe fn from_raw(channels: &'a [*mut f32], frames: usize) -> Self {
        Self { channels, frames }
    }

    /// Number of samples per channel in this block.
    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        // SAFETY: guaranteed by `from_raw`; `&mut self` prevents aliasing slices.
        unsafe { std::slice::from_raw_parts_mut(self.channels[index], self.frames) }
    }

    /// Copies channel `from` into channel `to`, e.g. to spread a mono signal.
    pub fn copy_channel(&mut self, from: usize, to: usize) {
        if from != to {
            // SAFETY: both pointers are valid for `frames` samples and do not overlap.
            unsafe { std::ptr::copy_nonoverlapping(self.channels[from], self.channels[to], self.frames) };
        }
    }

    /// Calls `f` for every sample of every channel.
    pub fn for_each_sample(&mut self, mut f: impl FnMut(&mut f32)) {
        for index in 0..self.channels.len() {
            self.channel_mut(index).iter_mut().for_each(&mut f);
        }
    }
}
