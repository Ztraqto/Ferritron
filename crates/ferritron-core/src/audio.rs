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

    /// All channels at once, each as its own `&mut [f32]`: for processing that
    /// reads one channel while writing another (mid/side, stereo link,
    /// surround). The channels never overlap, and nothing is allocated. At most
    /// [`MAX_CHANNELS`] channels are lent.
    pub fn channels_mut(&mut self) -> ChannelsMut<'_> {
        let len = self.channels.len().min(MAX_CHANNELS);
        let frames = self.frames;
        let ptrs = self.channels;
        // SAFETY: `from_raw` promises valid, non-overlapping channels; `&mut self` is held for the result's lifetime.
        let slices = std::array::from_fn(|i| if i < len { unsafe { std::slice::from_raw_parts_mut(ptrs[i], frames) } } else { <&mut [f32]>::default() });
        ChannelsMut { slices, len }
    }

    /// The first `N` channels as an array (`let [l, r] = io.split_mut::<2>()?`),
    /// or `None` when there are fewer than `N`.
    pub fn split_mut<const N: usize>(&mut self) -> Option<[&mut [f32]; N]> {
        if N > MAX_CHANNELS || self.channels.len() < N {
            return None;
        }
        let frames = self.frames;
        let ptrs = self.channels;
        // SAFETY: as in `channels_mut`.
        Some(std::array::from_fn(|i| unsafe { std::slice::from_raw_parts_mut(ptrs[i], frames) }))
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

/// Most channels a bus has (and [`AudioIo::channels_mut`] lends).
pub const MAX_CHANNELS: usize = 32;

/// Every channel of a block as a separate `&mut [f32]`
/// ([`AudioIo::channels_mut`]). Behaves like a slice of channels: `len()`,
/// `ch[i]`, `iter_mut()`, `split_at_mut`.
pub struct ChannelsMut<'a> {
    slices: [&'a mut [f32]; MAX_CHANNELS],
    len: usize,
}

impl<'a> std::ops::Deref for ChannelsMut<'a> {
    type Target = [&'a mut [f32]];
    fn deref(&self) -> &Self::Target {
        &self.slices[..self.len]
    }
}

impl<'a> std::ops::DerefMut for ChannelsMut<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slices[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_lent_together_without_overlap() {
        let (mut l, mut r, mut c) = ([1.0f32, 2.0, 3.0], [10.0f32, 20.0, 30.0], [0.0f32; 3]);
        let ptrs = [l.as_mut_ptr(), r.as_mut_ptr(), c.as_mut_ptr()];
        let mut io = unsafe { AudioIo::from_raw(&ptrs, 3) };
        {
            let mut ch = io.channels_mut();
            assert_eq!(ch.len(), 3);
            // Mid/side in place: reads two channels, writes both.
            for i in 0..3 {
                let (a, b) = (ch[0][i], ch[1][i]);
                ch[0][i] = (a + b) * 0.5;
                ch[1][i] = (a - b) * 0.5;
            }
            for (n, s) in ch.iter_mut().enumerate() {
                s[2] += n as f32 * 100.0;
            }
        }
        assert_eq!(l, [5.5, 11.0, 16.5 + 0.0]);
        assert_eq!(r[..2], [-4.5, -9.0]);
        assert_eq!(c[2], 200.0);
        let mut io = unsafe { AudioIo::from_raw(&ptrs[..1], 3) };
        assert!(io.split_mut::<2>().is_none(), "mono has no stereo pair");
        let mut io = unsafe { AudioIo::from_raw(&ptrs, 3) };
        let [a, b] = io.split_mut::<2>().unwrap();
        a[0] = 7.0;
        b[0] = 8.0;
        assert_eq!((l[0], r[0]), (7.0, 8.0));
        assert!(io.split_mut::<33>().is_none());
    }
}
