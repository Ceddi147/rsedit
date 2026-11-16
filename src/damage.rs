//! Works out which parts of the window changed since the last frame.
//!
//! egui redraws the whole UI on every input event, but most frames differ from
//! the previous one only in a few places (the text cursor, a hovered button, the
//! line being typed in) or not at all (the pointer moved over plain text).
//! Rasterizing on the CPU is the expensive part of a frame, so only the changed
//! pixels are drawn again; a frame that changes nothing is not drawn at all.
//!
//! Each tessellated primitive is identified by a hash of its mesh and clip
//! rectangle. Primitives present in only one of the two frames damage their
//! pixel bounds. Primitives present in both but painted in a different order
//! (a popup raised above another) damage their bounds too, so outside the
//! damage every pixel is covered by the same primitives in the same order and
//! is therefore unchanged.

use egui::epaint::{ClippedPrimitive, Primitive};
use std::collections::HashMap;

/// A pixel rectangle; `x1`/`y1` are exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl PxRect {
    fn area(self) -> u64 {
        u64::from(self.x1 - self.x0) * u64::from(self.y1 - self.y0)
    }

    fn union(self, o: Self) -> Self {
        Self {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    fn overlaps(self, o: Self) -> bool {
        self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1
    }
}

/// Damage regions are merged down to at most this many rectangles: each one
/// costs a pass over the primitives that touch it.
const MAX_RECTS: usize = 6;
/// Past this many raw rectangles (e.g. scrolling, where every line moves),
/// merging pairwise isn't worth it: everything is joined into one.
const MAX_RAW_RECTS: usize = 64;
/// Buffer contents up to this many frames old can be brought up to date.
const HISTORY: usize = 3;

#[derive(Clone, Copy)]
struct Key {
    hash: u64,
    rect: PxRect,
}

#[derive(Default)]
pub struct Damage {
    /// The visible primitives of the last frame, in paint order.
    prev: Vec<Key>,
    /// Everything that affects every pixel; a change invalidates all of them.
    setup: Option<(u32, u32, u32, u32)>,
    /// What each recently presented frame redrew, newest first.
    history: Vec<Vec<PxRect>>,
    // Scratch, kept to avoid reallocating every frame.
    keys: Vec<Key>,
    by_hash: HashMap<u64, Vec<usize>>,
}

impl Damage {
    /// Forgets the presented frames, e.g. when the pixel buffers are dropped.
    pub fn forget_buffers(&mut self) {
        self.history.clear();
    }

    /// Compares `prims` with the previous frame and returns the changed pixel
    /// regions (empty: the frame looks exactly like the last one).
    pub fn diff(
        &mut self,
        prims: &[ClippedPrimitive],
        pixels_per_point: f32,
        width: u32,
        height: u32,
        background: u32,
        textures: &egui::TexturesDelta,
    ) -> Vec<PxRect> {
        let full = PxRect {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
        };
        self.keys.clear();
        for p in prims {
            if let Some(key) = key(p, pixels_per_point, width, height) {
                self.keys.push(key);
            }
        }

        let setup = (width, height, pixels_per_point.to_bits(), background);
        // A replaced or freed texture may be drawn by unchanged meshes. Partial
        // updates only fill unused space (new glyphs in the font atlas).
        let textures_replaced = !textures.free.is_empty()
            || textures
                .set
                .iter()
                .any(|(_, ds)| ds.iter().any(|d| d.pos.is_none()));
        let damage = if self.setup != Some(setup) || textures_replaced {
            self.setup = Some(setup);
            vec![full]
        } else {
            self.changed_rects()
        };
        std::mem::swap(&mut self.prev, &mut self.keys);
        merge(damage, full)
    }

    fn changed_rects(&mut self) -> Vec<PxRect> {
        let mut out = Vec::new();
        self.by_hash.clear();
        for (i, k) in self.prev.iter().enumerate().rev() {
            self.by_hash.entry(k.hash).or_default().push(i);
        }
        // Old index of each new primitive (`usize::MAX`: not in the last frame).
        let mut old_of: Vec<usize> = Vec::with_capacity(self.keys.len());
        let mut matched = vec![false; self.prev.len()];
        for k in &self.keys {
            match self.by_hash.get_mut(&k.hash).and_then(Vec::pop) {
                Some(i) => {
                    matched[i] = true;
                    old_of.push(i);
                }
                None => {
                    out.push(k.rect);
                    old_of.push(usize::MAX);
                }
            }
        }
        for (k, m) in self.prev.iter().zip(&matched) {
            if !m {
                out.push(k.rect);
            }
        }
        // Primitives that kept their relative paint order form the longest
        // increasing run of old indices; every other matched one was reordered.
        let in_order = longest_increasing(&old_of);
        for (i, k) in self.keys.iter().enumerate() {
            if old_of[i] != usize::MAX && !in_order[i] {
                out.push(k.rect);
            }
        }
        out
    }

    /// The regions to redraw in a buffer last presented `age` frames ago
    /// (softbuffer's buffer age; 0 = unknown contents), given this frame's
    /// `damage`. `None`: redraw everything.
    pub fn repaint_regions(&self, damage: &[PxRect], age: u8) -> Option<Vec<PxRect>> {
        let missed = usize::from(age).checked_sub(1)?;
        if missed > self.history.len() {
            return None;
        }
        let mut rects = damage.to_vec();
        for frame in &self.history[..missed] {
            rects.extend_from_slice(frame);
        }
        let (width, height, _, _) = self.setup?;
        let full = PxRect {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
        };
        Some(merge(rects, full))
    }

    /// Records that a frame with `damage` was presented.
    pub fn presented(&mut self, damage: Vec<PxRect>) {
        self.history.insert(0, damage);
        self.history.truncate(HISTORY);
    }
}

/// Identifies a primitive and finds the pixels it can touch; `None` if it
/// draws nothing.
fn key(p: &ClippedPrimitive, ppp: f32, width: u32, height: u32) -> Option<Key> {
    let Primitive::Mesh(mesh) = &p.primitive else {
        return None;
    };
    if mesh.vertices.is_empty() || mesh.indices.is_empty() {
        return None;
    }
    let mut h = Fx::default();
    let c = p.clip_rect;
    h.add2(c.min.x.to_bits(), c.min.y.to_bits());
    h.add2(c.max.x.to_bits(), c.max.y.to_bits());
    match mesh.texture_id {
        egui::TextureId::Managed(id) => h.add(id),
        egui::TextureId::User(id) => h.add(id ^ (1 << 63)),
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for v in &mesh.vertices {
        h.add2(v.pos.x.to_bits(), v.pos.y.to_bits());
        h.add2(v.uv.x.to_bits(), v.uv.y.to_bits());
        h.add(u64::from(u32::from_ne_bytes(v.color.to_array())));
        x0 = x0.min(v.pos.x);
        y0 = y0.min(v.pos.y);
        x1 = x1.max(v.pos.x);
        y1 = y1.max(v.pos.y);
    }
    for pair in mesh.indices.chunks(2) {
        h.add2(pair[0], pair.get(1).copied().unwrap_or(u32::MAX));
    }
    // Within the clip rectangle, in pixels, one pixel of slack for rounding.
    // (Float-to-int `as` saturates, so off-screen edges clamp to 0.)
    let x0 = ((x0.max(c.min.x) * ppp).floor() - 1.0) as u32;
    let y0 = ((y0.max(c.min.y) * ppp).floor() - 1.0) as u32;
    let x1 = ((x1.min(c.max.x) * ppp).ceil() + 1.0) as u32;
    let y1 = ((y1.min(c.max.y) * ppp).ceil() + 1.0) as u32;
    let (x1, y1) = (x1.min(width), y1.min(height));
    (x0 < x1 && y0 < y1).then_some(Key {
        hash: h.0,
        rect: PxRect { x0, y0, x1, y1 },
    })
}

/// FxHash-style mixing: plenty for telling meshes apart, and fast enough to
/// run over every vertex of every frame.
#[derive(Default)]
struct Fx(u64);

impl Fx {
    fn add(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn add2(&mut self, a: u32, b: u32) {
        self.add(u64::from(a) | (u64::from(b) << 32));
    }
}

/// Marks the elements of `seq` that form a longest strictly increasing
/// subsequence, ignoring `usize::MAX` entries (never marked).
fn longest_increasing(seq: &[usize]) -> Vec<bool> {
    // tails[k]: index in `seq` of the smallest tail of an increasing run of length k+1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        if v == usize::MAX {
            continue;
        }
        let k = tails.partition_point(|&t| seq[t] < v);
        if k > 0 {
            prev[i] = tails[k - 1];
        }
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut marked = vec![false; seq.len()];
    let mut i = tails.last().copied().unwrap_or(usize::MAX);
    while i != usize::MAX {
        marked[i] = true;
        i = prev[i];
    }
    marked
}

/// Joins damage rectangles into at most [`MAX_RECTS`] non-overlapping ones,
/// trading a little overdraw for fewer passes.
fn merge(mut rects: Vec<PxRect>, full: PxRect) -> Vec<PxRect> {
    if rects.len() > MAX_RAW_RECTS {
        let all = rects.iter().fold(rects[0], |a, &r| a.union(r));
        rects = vec![all];
    }
    loop {
        // Overlapping rectangles must be joined (their pixels would be drawn
        // twice); beyond that, join the cheapest pair while there are too many.
        let mut best: Option<(usize, usize, u64)> = None;
        'search: for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let (a, b) = (rects[i], rects[j]);
                if a.overlaps(b) {
                    best = Some((i, j, 0));
                    break 'search;
                }
                let grow = a.union(b).area().saturating_sub(a.area() + b.area());
                if best.is_none_or(|(_, _, g)| grow < g) {
                    best = Some((i, j, grow));
                }
            }
        }
        match best {
            Some((i, j, grow))
                if grow == 0 && rects[i].overlaps(rects[j]) || rects.len() > MAX_RECTS =>
            {
                let b = rects.swap_remove(j);
                rects[i] = rects[i].union(b);
            }
            _ => break,
        }
    }
    // Mostly everything anyway: one pass over the whole window is cheaper.
    let area: u64 = rects.iter().map(|r| r.area()).sum();
    if area * 10 > full.area() * 7 {
        return vec![full];
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: u32, y0: u32, x1: u32, y1: u32) -> PxRect {
        PxRect { x0, y0, x1, y1 }
    }

    #[test]
    fn increasing_run() {
        let m = longest_increasing(&[3, 0, usize::MAX, 1, 2]);
        assert_eq!(m, [false, true, false, true, true]);
        assert_eq!(longest_increasing(&[]), Vec::<bool>::new());
    }

    #[test]
    fn merged_rects_never_overlap() {
        let full = r(0, 0, 1000, 1000);
        let rects = (0..40)
            .map(|i| r(i * 20, i * 7, i * 20 + 30, i * 7 + 25))
            .collect();
        let out = merge(rects, full);
        assert!(out.len() <= MAX_RECTS);
        for (i, a) in out.iter().enumerate() {
            for b in &out[i + 1..] {
                assert!(!a.overlaps(*b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn separate_rects_stay_separate() {
        let out = merge(
            vec![r(0, 0, 10, 10), r(500, 500, 510, 510)],
            r(0, 0, 1000, 1000),
        );
        assert_eq!(out.len(), 2);
    }
}
