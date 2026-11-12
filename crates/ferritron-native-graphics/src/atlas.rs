//! The mask atlas: coverage masks of paths and glyphs packed into one 8-bit texture image.
//!
//! This module only manages the pixels and the packing on the CPU side; the wgpu backend
//! uploads the changed rectangles.

use std::collections::HashMap;

use crate::raster::Mask;

pub(crate) const MIN_SIZE: u32 = 1024;
pub(crate) const MAX_SIZE: u32 = 4096;

/// Identity of a cached mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MaskKey {
    pub kind: u8,
    pub a: u64,
    pub b: u64,
    pub xf: [u32; 6],
}

impl MaskKey {
    pub fn glyph(font: u64, glyph: u16, size: u32, sub: u8) -> MaskKey {
        MaskKey { kind: 0, a: font, b: ((glyph as u64) << 40) | ((size as u64) << 8) | sub as u64, xf: [0; 6] }
    }
}

/// Shelf index of an entry that takes no room (an empty mask).
const NO_SHELF: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    /// Position in the atlas.
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Offset of the mask's top-left relative to its natural origin (device position for
    /// paths, the glyph origin for glyphs).
    pub mx: i32,
    pub my: i32,
    pub used: u64,
    /// The shelf the mask lives on (`NO_SHELF` for empty masks).
    shelf: u32,
}

struct Shelf {
    y: u32,
    h: u32,
    /// Next free x on this shelf (0 when the shelf was emptied by an eviction).
    x: u32,
    /// The last frame a mask on this shelf was used or added.
    used: u64,
}

pub(crate) struct Atlas {
    pub size: u32,
    pub data: Vec<u8>,
    shelves: Vec<Shelf>,
    next_y: u32,
    entries: HashMap<MaskKey, Entry>,
    /// Masks of this frame that were placed past the frame's budget: not kept, forgotten at the
    /// next frame (their shelves are then evictable like any other).
    frame_entries: HashMap<MaskKey, Entry>,
    /// Pixels of the masks this frame put into `entries`.
    frame_area: u64,
    /// Rectangles changed since the last upload `(x, y, w, h)`.
    pub dirty: Vec<(u32, u32, u32, u32)>,
    /// The pixels were replaced (size changed or cleared): upload everything.
    pub full_dirty: bool,
    pub frame: u64,
}

pub(crate) enum InsertError {
    Full,
}

impl Atlas {
    pub fn new() -> Atlas {
        Atlas {
            size: MIN_SIZE,
            data: vec![0; (MIN_SIZE * MIN_SIZE) as usize],
            shelves: Vec::new(),
            next_y: 0,
            entries: HashMap::new(),
            frame_entries: HashMap::new(),
            frame_area: 0,
            dirty: Vec::new(),
            full_dirty: true,
            frame: 0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.frame_area = 0;
        self.frame_entries.clear();
    }

    /// The entry for `key`, marked as used in this frame (so its shelf is not evicted).
    pub fn get(&mut self, key: &MaskKey) -> Option<Entry> {
        let f = self.frame;
        let e = match self.entries.get_mut(key) {
            Some(e) => e,
            None => self.frame_entries.get_mut(key)?,
        };
        e.used = f;
        if let Some(s) = self.shelves.get_mut(e.shelf as usize) {
            s.used = f;
        }
        Some(*e)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Place `mask` (pixel data and offsets as returned by the rasterizer; `ox`, `oy` make the
    /// entry's offset). Zero-sized masks are stored as empty entries.
    ///
    /// When the atlas is full it grows to its maximum first; then the least recently used shelf
    /// that no mask of this frame uses is emptied, and its masks are drawn again when they are
    /// next asked for. `Err(Full)` only when every shelf is in use this frame.
    pub fn insert(&mut self, key: MaskKey, mask: &Mask, ox: i32, oy: i32) -> Result<Entry, InsertError> {
        if mask.is_empty() {
            let e = Entry { x: 0, y: 0, w: 0, h: 0, mx: ox, my: oy, used: self.frame, shelf: NO_SHELF };
            self.entries.insert(key, e);
            return Ok(e);
        }
        let (w, h) = (mask.w, mask.h);
        if w > MAX_SIZE || h > MAX_SIZE {
            return Err(InsertError::Full);
        }
        // Once this frame has added a quarter of the atlas, its other new masks are for this
        // frame only: a moving shape must not fill the atlas with a new mask every frame.
        let quarter = (self.size as u64 * self.size as u64) / 4;
        let transient = self.frame_area >= quarter;
        if !transient {
            self.frame_area += w as u64 * h as u64;
        }
        let (shelf, x, y) = loop {
            match self.alloc(w, h) {
                Some(p) => break p,
                None => {
                    if self.size < MAX_SIZE {
                        self.grow();
                    } else if !self.evict_one_shelf() {
                        return Err(InsertError::Full);
                    }
                }
            }
        };
        self.shelves[shelf as usize].used = self.frame;
        for row in 0..h {
            let dst = ((y + row) * self.size + x) as usize;
            let src = (row * w) as usize;
            self.data[dst..dst + w as usize].copy_from_slice(&mask.data[src..src + w as usize]);
        }
        if !self.full_dirty {
            self.dirty.push((x, y, w, h));
        }
        let e = Entry { x, y, w, h, mx: ox, my: oy, used: self.frame, shelf };
        if transient {
            self.frame_entries.insert(key, e);
        } else {
            self.entries.insert(key, e);
        }
        Ok(e)
    }

    /// Empty the least recently used shelf that holds something and no mask of this frame
    /// uses. Its entries are forgotten. `false` when there is no such shelf.
    fn evict_one_shelf(&mut self) -> bool {
        let frame = self.frame;
        let victim = self.shelves.iter().enumerate().filter(|(_, s)| s.x > 0 && s.used < frame).min_by_key(|(_, s)| s.used).map(|(i, _)| i as u32);
        let Some(i) = victim else { return false };
        self.entries.retain(|_, e| e.shelf != i);
        let s = &mut self.shelves[i as usize];
        s.x = 0;
        true
    }

    /// Room for a `w` x `h` mask: its shelf and position, or `None` when no shelf has it.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32, u32)> {
        // best shelf: the shortest one that fits the height and has room
        let mut best: Option<usize> = None;
        for (i, s) in self.shelves.iter().enumerate() {
            if s.h >= h && s.h <= h + h / 2 + 2 && s.x + w <= self.size && best.is_none_or(|b| s.h < self.shelves[b].h) {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let s = &mut self.shelves[i];
            let p = (i as u32, s.x, s.y);
            s.x += w;
            return Some(p);
        }
        // A new shelf spans the atlas: the mask must fit its width too (a wider one waits for
        // the atlas to grow).
        if w <= self.size && self.next_y + h <= self.size {
            let y = self.next_y;
            let index = self.shelves.len() as u32;
            self.shelves.push(Shelf { y, h, x: w, used: self.frame });
            self.next_y += h;
            return Some((index, 0, y));
        }
        None
    }

    fn grow(&mut self) {
        let new = (self.size * 2).min(MAX_SIZE);
        let mut data = vec![0u8; (new * new) as usize];
        for row in 0..self.size {
            let s = (row * self.size) as usize;
            let d = (row * new) as usize;
            data[d..d + self.size as usize].copy_from_slice(&self.data[s..s + self.size as usize]);
        }
        self.data = data;
        self.size = new;
        self.full_dirty = true;
        self.dirty.clear();
    }

    /// Forget every mask (the atlas keeps its size).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.frame_entries.clear();
        self.frame_area = 0;
        self.shelves.clear();
        self.next_y = 0;
        self.data.iter_mut().for_each(|b| *b = 0);
        self.full_dirty = true;
        self.dirty.clear();
    }

    pub fn take_dirty(&mut self) -> (bool, Vec<(u32, u32, u32, u32)>) {
        let full = std::mem::take(&mut self.full_dirty);
        let d = std::mem::take(&mut self.dirty);
        (full, d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(w: u32, h: u32, v: u8) -> Mask {
        Mask { x: 0, y: 0, w, h, data: vec![v; (w * h) as usize] }
    }

    #[test]
    fn packs_without_overlap_and_remembers_keys() {
        let mut a = Atlas::new();
        let k1 = MaskKey::glyph(1, 1, 16, 0);
        let k2 = MaskKey::glyph(1, 2, 16, 0);
        let e1 = a.insert(k1, &mask(10, 12, 100), -1, -9).ok().unwrap();
        let e2 = a.insert(k2, &mask(10, 12, 200), 0, 0).ok().unwrap();
        assert!(e1.x + e1.w <= e2.x || e2.x + e2.w <= e1.x || e1.y != e2.y);
        assert_eq!(a.data[(e1.y * a.size + e1.x) as usize], 100);
        assert_eq!(a.data[(e2.y * a.size + e2.x) as usize], 200);
        assert_eq!(a.get(&k1).unwrap().mx, -1);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn grows_up_to_the_maximum_then_reports_full() {
        let mut a = Atlas::new();
        let mut n = 0u64;
        let mut full = false;
        for _ in 0..100 {
            match a.insert(MaskKey::glyph(n, 0, 0, 0), &mask(1000, 1000, 1), 0, 0) {
                Ok(_) => n += 1,
                Err(_) => {
                    full = true;
                    break;
                }
            }
        }
        assert!(full);
        assert_eq!(a.size, MAX_SIZE);
        assert!(n >= 12, "a 4096 atlas holds at least 16 masks of 1000x1000 ({n})");
        a.clear();
        assert!(a.insert(MaskKey::glyph(999, 0, 0, 0), &mask(1000, 1000, 1), 0, 0).is_ok());
    }

    /// A mask wider than the atlas makes it grow; the shelves never reach past its edge.
    #[test]
    fn a_mask_wider_than_the_atlas_grows_it() {
        let mut a = Atlas::new();
        let wide = a.insert(MaskKey::glyph(1, 0, 0, 0), &mask(MIN_SIZE + 500, 20, 7), 0, 0).ok().unwrap();
        assert!(a.size > MIN_SIZE);
        assert!(wide.x + wide.w <= a.size);
        for n in 2..40u64 {
            let e = a.insert(MaskKey::glyph(n, 0, 0, 0), &mask(323, 20, 1), 0, 0).ok().unwrap();
            assert!(e.x + e.w <= a.size, "mask {n} at x {} of {}", e.x, a.size);
        }
        for s in &a.shelves {
            assert!(s.x <= a.size);
        }
    }

    /// Fill a maximum-size atlas with 1000 x 1000 masks, one per frame-1 key (4 per shelf).
    /// The 16 masks of 1000 x 1000 that exactly fill a maximum-size atlas, one per frame (so the
    /// frame budget keeps every one of them).
    fn full_atlas() -> (Atlas, Vec<MaskKey>) {
        let mut a = Atlas::new();
        let mut keys = Vec::new();
        for n in 0..16u64 {
            a.begin_frame();
            let key = MaskKey::glyph(n, 0, 0, 0);
            a.insert(key, &mask(1000, 1000, 1), 0, 0).ok().unwrap();
            keys.push(key);
        }
        assert_eq!(a.size, MAX_SIZE);
        assert_eq!(a.len(), 16);
        (a, keys)
    }

    #[test]
    fn a_full_atlas_evicts_the_oldest_shelf_only() {
        let (mut a, keys) = full_atlas();
        let before = a.len();
        // the oldest shelf is the one least recently used
        let oldest = (0..a.shelves.len()).filter(|i| a.shelves[*i].x > 0).min_by_key(|i| a.shelves[*i].used).unwrap() as u32;
        let (on_oldest, others): (Vec<MaskKey>, Vec<MaskKey>) = keys.iter().copied().partition(|k| a.entries[k].shelf == oldest);
        assert!(!on_oldest.is_empty() && !others.is_empty());
        a.begin_frame();
        // every shelf but the oldest is in use in this frame
        for k in &others {
            assert!(a.get(k).is_some());
        }
        let new_key = MaskKey::glyph(999, 0, 0, 0);
        assert!(a.insert(new_key, &mask(1000, 1000, 2), 0, 0).is_ok(), "room made by evicting");
        for k in &on_oldest {
            assert!(a.get(k).is_none(), "the oldest shelf was emptied");
        }
        for k in &others {
            assert!(a.get(k).is_some(), "the other shelves keep their masks");
        }
        assert_eq!(a.len(), before - on_oldest.len() + 1);
        let e = a.get(&new_key).unwrap();
        assert_eq!(a.data[(e.y * a.size + e.x) as usize], 2, "the new mask's pixels are in place");
    }

    #[test]
    fn masks_used_in_this_frame_are_never_evicted() {
        let (mut a, keys) = full_atlas();
        a.begin_frame();
        for k in &keys {
            assert!(a.get(k).is_some(), "every mask is used this frame");
        }
        assert!(a.insert(MaskKey::glyph(999, 0, 0, 0), &mask(1000, 1000, 2), 0, 0).is_err(), "nothing may be evicted");
        assert_eq!(a.len(), keys.len());
    }

    #[test]
    fn masks_past_the_frame_budget_are_for_this_frame_only() {
        let mut a = Atlas::new();
        a.begin_frame();
        // a 1024 x 1024 atlas: a quarter is 262144 pixels, three 300 x 300 masks pass it
        for n in 0..6u64 {
            a.insert(MaskKey::glyph(n, 0, 0, 0), &mask(300, 300, 1), 0, 0).ok().unwrap();
        }
        assert_eq!(a.len(), 3, "the first three are kept");
        assert!(a.get(&MaskKey::glyph(5, 0, 0, 0)).is_some(), "this frame finds its transient masks");
        a.begin_frame();
        assert!(a.get(&MaskKey::glyph(5, 0, 0, 0)).is_none(), "they are gone the next frame");
        assert!(a.get(&MaskKey::glyph(0, 0, 0, 0)).is_some(), "kept masks stay");
    }

    #[test]
    fn dirty_rectangles_are_reported_once() {
        let mut a = Atlas::new();
        let (full, _) = a.take_dirty();
        assert!(full, "a new atlas needs a full upload");
        a.insert(MaskKey::glyph(1, 1, 1, 1), &mask(4, 4, 9), 0, 0).ok().unwrap();
        let (full, d) = a.take_dirty();
        assert!(!full);
        assert_eq!(d.len(), 1);
        assert!(a.take_dirty().1.is_empty());
    }
}
