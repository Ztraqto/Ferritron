//! Images registered with a renderer.

use crate::scene::ImageId;

/// A level of the mip chain: `(width, height, premultiplied RGBA8)`.
pub(crate) type MipLevel = (u32, u32, Vec<u8>);

pub(crate) struct StoredImage {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA8 (sRGB-encoded color).
    #[allow(dead_code)]
    pub premul: Vec<u8>,
    /// Levels 1.. of the mip chain (level 0 is `premul`), each half the size of the one before
    /// (rounded down, at least 1). Built once when the image is stored; the GPU backend uploads
    /// them and the CPU backend samples them by the same level of detail.
    pub mips: Vec<MipLevel>,
    /// Bumped when the pixels change (the GPU backend re-uploads).
    #[allow(dead_code)]
    pub generation: u64,
}

/// The images known to a renderer. Insert RGBA8 (straight alpha, sRGB) pixels and use the
/// returned [`ImageId`] in a [`Paint::Image`](crate::Paint) or `scene.image`.
#[derive(Default)]
pub struct ImageStore {
    pub(crate) items: Vec<Option<StoredImage>>,
    free: Vec<u32>,
    gen_counter: u64,
}

impl ImageStore {
    pub fn new() -> ImageStore {
        ImageStore::default()
    }

    /// Add an image. `rgba` must hold `width * height * 4` bytes (straight alpha).
    ///
    /// # Panics
    /// If `rgba` has the wrong length.
    pub fn insert(&mut self, rgba: &[u8], width: u32, height: u32) -> ImageId {
        assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4, "rgba length does not match width * height * 4");
        let img = self.make(rgba, width, height);
        if let Some(i) = self.free.pop() {
            self.items[i as usize] = Some(img);
            ImageId(i)
        } else {
            self.items.push(Some(img));
            ImageId(self.items.len() as u32 - 1)
        }
    }

    /// Replace the pixels of an existing image (the size may change).
    pub fn update(&mut self, id: ImageId, rgba: &[u8], width: u32, height: u32) {
        assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4, "rgba length does not match width * height * 4");
        let img = self.make(rgba, width, height);
        if let Some(slot) = self.items.get_mut(id.0 as usize) {
            *slot = Some(img);
        }
    }

    pub fn remove(&mut self, id: ImageId) {
        if let Some(slot) = self.items.get_mut(id.0 as usize) {
            if slot.take().is_some() {
                self.free.push(id.0);
            }
        }
    }

    /// `(width, height)` in pixels.
    pub fn size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.get(id).map(|i| (i.width, i.height))
    }

    pub(crate) fn get(&self, id: ImageId) -> Option<&StoredImage> {
        self.items.get(id.0 as usize).and_then(|s| s.as_ref())
    }

    fn make(&mut self, rgba: &[u8], width: u32, height: u32) -> StoredImage {
        self.gen_counter += 1;
        let mut premul = Vec::with_capacity(rgba.len());
        for px in rgba.chunks_exact(4) {
            let a = px[3] as u32;
            premul.push(((px[0] as u32 * a + 127) / 255) as u8);
            premul.push(((px[1] as u32 * a + 127) / 255) as u8);
            premul.push(((px[2] as u32 * a + 127) / 255) as u8);
            premul.push(px[3]);
        }
        let mips = box_levels(&premul, width, height);
        StoredImage { width, height, premul, mips, generation: self.gen_counter }
    }
}

/// The mip chain below `base` (`w` x `h`, premultiplied RGBA8): each level averages 2 x 2 texels
/// of the one before (in the encoded values, as bilinear sampling does). An odd last row or
/// column is dropped, the edge texel is repeated where a level is one texel wide.
pub(crate) fn box_levels(base: &[u8], w: u32, h: u32) -> Vec<MipLevel> {
    let mut out: Vec<MipLevel> = Vec::new();
    let (mut cw, mut ch) = (w, h);
    while cw > 1 || ch > 1 {
        let (nw, nh) = ((cw / 2).max(1), (ch / 2).max(1));
        let mut dst = vec![0u8; (nw * nh * 4) as usize];
        {
            let src: &[u8] = match out.last() {
                Some((_, _, d)) => d,
                None => base,
            };
            for y in 0..nh {
                let (y0, y1) = ((2 * y).min(ch - 1), (2 * y + 1).min(ch - 1));
                for x in 0..nw {
                    let (x0, x1) = ((2 * x).min(cw - 1), (2 * x + 1).min(cw - 1));
                    let o = ((y * nw + x) * 4) as usize;
                    let i = [((y0 * cw + x0) * 4) as usize, ((y0 * cw + x1) * 4) as usize, ((y1 * cw + x0) * 4) as usize, ((y1 * cw + x1) * 4) as usize];
                    for c in 0..4 {
                        let sum: u32 = i.iter().map(|&k| src[k + c] as u32).sum();
                        dst[o + c] = ((sum + 2) / 4) as u8;
                    }
                }
            }
        }
        out.push((nw, nh, dst));
        cw = nw;
        ch = nh;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_premultiplies_and_reuses_slots() {
        let mut s = ImageStore::new();
        let a = s.insert(&[255, 0, 0, 128], 1, 1);
        assert_eq!(s.get(a).unwrap().premul, vec![128, 0, 0, 128]);
        s.remove(a);
        let b = s.insert(&[0, 0, 0, 255], 1, 1);
        assert_eq!(a, b);
        assert_eq!(s.size(b), Some((1, 1)));
    }

    #[test]
    fn mip_levels_halve_and_average() {
        // 4 x 2: left half white, right half black (opaque)
        let mut px = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 255 } else { 0 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut s = ImageStore::new();
        let id = s.insert(&px, 4, 2);
        let img = s.get(id).unwrap();
        assert_eq!(img.mips.len(), 2, "4 x 2 -> 2 x 1 -> 1 x 1");
        assert_eq!((img.mips[0].0, img.mips[0].1), (2, 1));
        assert_eq!(img.mips[0].2[0], 255, "left texel: white");
        assert_eq!(img.mips[0].2[4], 0, "right texel: black");
        assert_eq!((img.mips[1].0, img.mips[1].1), (1, 1));
        assert_eq!(img.mips[1].2[0], 128, "the average of white and black");
    }
}
