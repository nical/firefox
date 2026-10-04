/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A sparse tiling path rasterizer.
//!
//! Paths are flattened and their edges binned into 16x16 pixel tiles on the CPU.
//! Tiles that contain edges are rasterized on the GPU by evaluating the area
//! coverage of each of their edges in the fragment shader (see `cs_path_tile`),
//! while tiles that are fully covered are emitted as solid spans. Tiles that
//! are not covered at all are not emitted.
//!
//! Each tile carries a backdrop: the winding number at its top-left corner.
//! It is computed on the CPU by accumulating the winding of the edges that
//! cross the horizontal tile boundaries, from left to right within each row
//! of tiles.

use api::{FillRule, Path, PathEvent};
use api::units::*;
use euclid::default::Box2D;
use euclid::{point2, Transform2D, Transform3D};

use crate::bezier;
use crate::internal_types::FrameVec;
use crate::renderer::{GpuBufferBlockEdge, PathEdgeBufferBuilder};

pub const TILE_SIZE: i32 = 16;
const TILE_SIZE_F32: f32 = TILE_SIZE as f32;

const UNITS_PER_TILE: i32 = 256;
const UNITS_PER_TILE_F32: f32 = UNITS_PER_TILE as f32;
const LOCAL_COORD_MASK: i32 = 255;
const LOCAL_COORD_BITS: i32 = 8;
const COORD_SCALE: f32 = UNITS_PER_TILE_F32 / TILE_SIZE_F32;

/// Edge coordinates have this many steps per pixel, so that edges on integer
/// pixel positions are encoded exactly. Must match the scale in path_tile.glsl.
const EDGE_STEPS_PER_PIXEL: i32 = 15;

/// The largest encoded edge coordinate, which is the tile's right or bottom side.
const MAX_EDGE_COORD: u8 = (EDGE_STEPS_PER_PIXEL * TILE_SIZE) as u8;

/// Encode a tile-local fixed point coordinate (in `0..=UNITS_PER_TILE`) into an
/// edge coordinate, in `0..=MAX_EDGE_COORD`.
#[inline(always)]
fn encode_edge_coord(units: i32) -> u8 {
    ((units * MAX_EDGE_COORD as i32 + UNITS_PER_TILE / 2) >> LOCAL_COORD_BITS) as u8
}

/// Same as `encode_edge_coord`, for a coordinate that is not snapped to the
/// fixed point grid. Values outside of the tile are clamped.
#[inline(always)]
fn encode_edge_coord_f32(units: f32) -> u8 {
    (units * (MAX_EDGE_COORD as f32 / UNITS_PER_TILE_F32) + 0.5).min(MAX_EDGE_COORD as f32) as u8
}

/// Tile coordinates are encoded with 10 bits. The last row and column are
/// reserved for the sentinel event that flushes the last tile.
const MAX_TILES_PER_AXIS: i32 = 1023;
pub const MAX_TILED_SIZE: i32 = MAX_TILES_PER_AXIS * TILE_SIZE;

/// Curves with control points spanning more than this many pixels are
/// replaced with their chord instead of being flattened, to bound the number
/// of segments.
const MAX_CURVE_EXTENT: f32 = 1_000_000.0;

/// Points at a smaller `w` are behind the eye or too close to it.
const MIN_W: f32 = 1e-5;

const DEFAULT_TOLERANCE: f32 = 0.25;

/// Maps the path to task-local device pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub enum PathTransform {
    Affine(Transform2D<f32, LayoutPixel, DevicePixel>),
    Projective(Transform3D<f32, LayoutPixel, DevicePixel>),
}

impl PathTransform {
    pub fn new(transform: &Transform3D<f32, LayoutPixel, DevicePixel>) -> Self {
        // Points on the z = 0 plane are mapped without a perspective divide
        // when w does not depend on x and y.
        if transform.m14 == 0.0 && transform.m24 == 0.0 && transform.m44 == 1.0 {
            PathTransform::Affine(transform.to_2d())
        } else {
            PathTransform::Projective(*transform)
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TilePosition(u32);

impl TilePosition {
    const MASK: u32 = 0x3FF;

    pub fn extended(x: u32, y: u32, extend: u32) -> Self {
        debug_assert!(x <= Self::MASK);
        debug_assert!(y <= Self::MASK);
        debug_assert!(extend <= Self::MASK);

        TilePosition(extend << 20 | x << 10 | y)
    }

    pub fn new(x: u32, y: u32) -> Self {
        debug_assert!(x <= Self::MASK);
        debug_assert!(y <= Self::MASK);

        TilePosition(x << 10 | y)
    }

    pub fn to_u32(&self) -> u32 {
        self.0
    }
    #[cfg(test)]
    pub fn x(&self) -> u32 {
        (self.0 >> 10) & Self::MASK
    }
    #[cfg(test)]
    pub fn y(&self) -> u32 {
        (self.0) & Self::MASK
    }
    #[cfg(test)]
    pub fn extension(&self) -> u32 {
        (self.0 >> 20) & Self::MASK
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct TilePoint {
    x: u16,
    y: u16,
}

impl TilePoint {
    fn new(x: u16, y: u16) -> Self {
        TilePoint { x, y }
    }
}

/// The encoded path tile instance, as consumed by the `cs_path_tile` shader.
///
/// - `data[0]`: the `TilePosition`,
/// - `data[1]`: the index of the tile's first edge in the frame's edge buffer,
/// - `data[2]`: the edge count in the upper 16 bits, the backdrop plus 128 in
///   the lower 16 bits,
/// - `data[3]`: the address of the `PathInfo` in the i32 gpu buffer.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct PathTileInstance {
    pub data: [u32; 4],
}

#[derive(Copy, Clone, Debug)]
struct TileInstance {
    position: TilePosition,
    first_edge: u32,
    edge_count: u16,
    backdrop: i16,
    path_index: u32,
}

impl TileInstance {
    fn encode(self) -> PathTileInstance {
        PathTileInstance {
            data: [
                self.position.to_u32(),
                self.first_edge,
                (self.edge_count as u32) << 16 | (self.backdrop as i32 + 128) as u32,
                self.path_index,
            ]
        }
    }

    #[cfg(test)]
    fn decode(instance: PathTileInstance) -> Self {
        let data = instance.data;
        TileInstance {
            position: TilePosition(data[0]),
            first_edge: data[1],
            edge_count: (data[2] >> 16) as u16,
            backdrop: ((data[2] & 0xFFFF) as i32 - 128) as i16,
            path_index: data[3],
        }
    }
}

/// Per-path parameters, stored in one texel of the i32 gpu buffer.
pub struct PathInfo {
    /// The render task the path is rasterized into.
    pub render_task_address: u32,
    pub fill_rule: FillRule,
    /// Whether to fill the outside of the path instead of its inside.
    pub inverted: bool,
    pub opacity: f32,
}

impl PathInfo {
    pub fn encode(&self) -> [i32; 4] {
        let mut fill_rule = match self.fill_rule {
            FillRule::Evenodd => 0,
            FillRule::Nonzero => 1,
        };
        if self.inverted {
            fill_rule |= 2;
        }

        [
            self.render_task_address as i32,
            fill_rule,
            (self.opacity.max(0.0).min(1.0) * 65535.0) as i32,
            0,
        ]
    }
}

pub struct PathTilerOutput<'l> {
    /// The frame's global edge buffer.
    pub edges: &'l mut PathEdgeBufferBuilder,
    pub tiles: &'l mut FrameVec<PathTileInstance>,
}

pub struct PathTiler {
    events: Vec<Event>,
    tolerance: f32,
    viewport: DeviceRect,
    scissor: DeviceRect,
    // The scissor rect, rounded up to include the top and left part of
    // partially covered tiles. Backdrop calculation for partially covered
    // tiles requires the top-left corver to be included in the clip rect.
    culling_rect: DeviceRect,
    scissor_tiles: Box2D<i32>,
    edge_buffer: Vec<GpuBufferBlockEdge>,
    // Whether segments of the current path can lie outside the culling
    // rect. False when the path's bounding rect is fully inside, which
    // lets the binning skip its per-segment culling tests.
    cull_segments: bool,
}

impl PathTiler {
    pub fn new() -> Self {
        PathTiler {
            events: Vec::new(),
            tolerance: DEFAULT_TOLERANCE,
            viewport: DeviceRect::zero(),
            scissor: DeviceRect::zero(),
            culling_rect: DeviceRect::zero(),
            scissor_tiles: Box2D::zero(),
            edge_buffer: Vec::with_capacity(32),
            cull_segments: true,
        }
    }

    /// Begin rasterizing into a target. Coordinates are relative to the
    /// target's origin and are clamped to `MAX_TILED_SIZE`.
    pub fn begin_target(&mut self, mut viewport: DeviceIntRect) {
        viewport.min.x = viewport.min.x.max(0).min(MAX_TILED_SIZE);
        viewport.min.y = viewport.min.y.max(0).min(MAX_TILED_SIZE);
        viewport.max.x = viewport.max.x.max(0).min(MAX_TILED_SIZE);
        viewport.max.y = viewport.max.y.max(0).min(MAX_TILED_SIZE);
        self.viewport = viewport.to_f32();
        self.set_scissor_rect(viewport);
    }

    pub fn set_scissor_rect(&mut self, i32_scissor: DeviceIntRect) {
        let scissor = i32_scissor.to_f32().intersection_unchecked(&self.viewport);
        self.scissor = scissor;
        self.scissor_tiles = Box2D {
            min: point2(
                i32_scissor.min.x / TILE_SIZE,
                i32_scissor.min.y / TILE_SIZE,
            ),
            max: point2(
                (scissor.max.x as i32) / TILE_SIZE + (i32_scissor.max.x % TILE_SIZE != 0) as i32,
                (scissor.max.y as i32) / TILE_SIZE + (i32_scissor.max.y % TILE_SIZE != 0) as i32,
            ),
        };
        self.culling_rect.min.x = self.scissor_tiles.min.x as f32 * TILE_SIZE_F32;
        self.culling_rect.min.y = self.scissor_tiles.min.y as f32 * TILE_SIZE_F32;
        self.culling_rect.max = self.scissor.max;
    }

    /// Fill a path.
    ///
    /// `path_address` is the address of the path's `PathInfo` in the i32 gpu
    /// buffer.
    pub fn fill_path(
        &mut self,
        path: &Path,
        transform: &PathTransform,
        fill_rule: FillRule,
        inverted: bool,
        path_address: u32,
        output: &mut PathTilerOutput,
    ) {
        tracy_rs::profile_scope!("PathTiler::fill_path");

        self.events.clear();
        self.edge_buffer.clear();

        if self.scissor_tiles.is_empty() {
            return;
        }

        match transform {
            PathTransform::Affine(transform) => {
                let aabb = transform.outer_transformed_box(&path.aabb());
                if !aabb.min.x.is_finite() || !aabb.min.y.is_finite()
                    || !aabb.max.x.is_finite() || !aabb.max.y.is_finite() {
                    // Degenerate path: nothing inside of it is drawn.
                    if inverted {
                        self.generate_tiles(fill_rule, inverted, path_address, output);
                    }
                    return;
                }

                self.cull_segments = !(aabb.min.y >= self.culling_rect.min.y - 1.0
                    && aabb.max.y <= self.culling_rect.max.y + 1.0
                    && aabb.min.x >= self.culling_rect.min.x - 1.0
                    && aabb.max.x <= self.culling_rect.max.x + 1.0);

                self.tile_path_affine(path, transform);
            }
            PathTransform::Projective(transform) => {
                self.cull_segments = true;
                self.tile_path_projective(path, transform);
            }
        }

        self.generate_tiles(fill_rule, inverted, path_address, output);
    }

    fn tile_path_affine(
        &mut self,
        path: &Path,
        transform: &Transform2D<f32, LayoutPixel, DevicePixel>,
    ) {
        tracy_rs::profile_scope!("PathTiler::tile_path");

        // The last binned endpoint, in task space. Each point is transformed
        // once even though it is the endpoint of two consecutive segments.
        let mut from = DevicePoint::zero();
        let mut first = DevicePoint::zero();

        for evt in path.iter() {
            match evt {
                PathEvent::Begin { at } => {
                    from = transform.transform_point(at);
                    first = from;
                }
                PathEvent::End { .. } => {
                    // Open sub-paths are implicitly closed.
                    if from != first {
                        self.bin_line(from, first);
                    }
                }
                PathEvent::Line { to, .. } => {
                    self.add_line(&mut from, transform.transform_point(to));
                }
                PathEvent::Quadratic { ctrl, to, .. } => {
                    self.add_quadratic(
                        &mut from,
                        transform.transform_point(ctrl),
                        transform.transform_point(to),
                    );
                }
                PathEvent::Cubic { ctrl1, ctrl2, to, .. } => {
                    self.add_cubic(
                        &mut from,
                        transform.transform_point(ctrl1),
                        transform.transform_point(ctrl2),
                        transform.transform_point(to),
                    );
                }
            }
        }
    }

    /// Whether a segment with these bounds can be skipped because it does
    /// not affect any tile in the scissor rect.
    #[inline]
    fn is_culled(&self, min_x: f32, min_y: f32, max_y: f32) -> bool {
        min_y > self.culling_rect.max.y + 1.0
            || max_y < self.culling_rect.min.y - 1.0
            || min_x > self.culling_rect.max.x + 1.0
    }

    /// Whether a segment with this right edge is fully on the left side of
    /// the culling rect, where only its contribution to the backdrops matters.
    #[inline]
    fn is_on_the_left(&self, max_x: f32) -> bool {
        max_x < self.culling_rect.min.x - 1.0
    }

    fn add_line(&mut self, from: &mut DevicePoint, to: DevicePoint) {
        if self.cull_segments {
            let min_y = from.y.min(to.y);
            let max_y = from.y.max(to.y);
            let min_x = from.x.min(to.x);
            if self.is_culled(min_x, min_y, max_y) {
                *from = to;
                return;
            }
        }

        // Skip tiny edges without introducing gaps: `from` does not advance.
        if (to - *from).square_length() < self.tolerance * self.tolerance {
            return;
        }

        self.bin_line(*from, to);
        *from = to;
    }

    fn add_quadratic(&mut self, from: &mut DevicePoint, ctrl: DevicePoint, to: DevicePoint) {
        let min_x = from.x.min(ctrl.x).min(to.x);
        let max_x = from.x.max(ctrl.x).max(to.x);
        let min_y = from.y.min(ctrl.y).min(to.y);
        let max_y = from.y.max(ctrl.y).max(to.y);
        let sq_tolerance = self.tolerance * self.tolerance;

        if self.cull_segments {
            if self.is_culled(min_x, min_y, max_y) {
                *from = to;
                return;
            }
            // A curve and its chord form a closed loop that does not cover
            // anything on its right, so they contribute the same backdrop.
            if self.is_on_the_left(max_x) {
                self.bin_line(*from, to);
                *from = to;
                return;
            }
        }

        if (to - *from).square_length() < sq_tolerance {
            let center = from.lerp(to, 0.5);
            if (ctrl - center).square_length() < sq_tolerance {
                return;
            }
        }

        if max_x - min_x > MAX_CURVE_EXTENT || max_y - min_y > MAX_CURVE_EXTENT {
            self.bin_line(*from, to);
            *from = to;
            return;
        }

        let tolerance = self.tolerance;
        let mut prev = *from;
        bezier::flatten_quadratic(*from, ctrl, to, tolerance, &mut |p| {
            self.bin_line(prev, p);
            prev = p;
        });
        *from = to;
    }

    fn add_cubic(&mut self, from: &mut DevicePoint, ctrl1: DevicePoint, ctrl2: DevicePoint, to: DevicePoint) {
        let min_x = from.x.min(ctrl1.x).min(ctrl2.x).min(to.x);
        let max_x = from.x.max(ctrl1.x).max(ctrl2.x).max(to.x);
        let min_y = from.y.min(ctrl1.y).min(ctrl2.y).min(to.y);
        let max_y = from.y.max(ctrl1.y).max(ctrl2.y).max(to.y);
        let sq_tolerance = self.tolerance * self.tolerance;

        if self.cull_segments {
            if self.is_culled(min_x, min_y, max_y) {
                *from = to;
                return;
            }
            // See add_quadratic.
            if self.is_on_the_left(max_x) {
                self.bin_line(*from, to);
                *from = to;
                return;
            }
        }

        if (to - *from).square_length() < sq_tolerance {
            let center = from.lerp(to, 0.5);
            if (ctrl1 - center).square_length() < sq_tolerance
                && (ctrl2 - center).square_length() < sq_tolerance
            {
                return;
            }
        }

        if max_x - min_x > MAX_CURVE_EXTENT || max_y - min_y > MAX_CURVE_EXTENT {
            self.bin_line(*from, to);
            *from = to;
            return;
        }

        let tolerance = self.tolerance;
        let mut prev = *from;
        bezier::flatten_cubic(*from, ctrl1, ctrl2, to, tolerance, &mut |p| {
            self.bin_line(prev, p);
            prev = p;
        });
        *from = to;
    }

    fn tile_path_projective(
        &mut self,
        path: &Path,
        transform: &Transform3D<f32, LayoutPixel, DevicePixel>,
    ) {
        tracy_rs::profile_scope!("PathTiler::tile_path_projective");

        // Curves are flattened in the path's space and the resulting points
        // are projected, so the tolerance is scaled by an estimate of the
        // scale of the transform.
        let local_aabb = path.aabb();
        let mut tolerance = self.tolerance;
        if let Some(device_aabb) = transform.outer_transformed_box2d(&local_aabb) {
            let sx = device_aabb.width() / local_aabb.width();
            let sy = device_aabb.height() / local_aabb.height();
            let scale = sx.max(sy);
            if scale.is_finite() && scale > 0.0 {
                tolerance /= scale;
            }
        }
        let local_extent = local_aabb.width().max(local_aabb.height());
        let flatten_curves = local_extent / tolerance < MAX_CURVE_EXTENT;

        let mut state = ProjectedSubPath::new(transform);
        let mut first = LayoutPoint::zero();
        let mut from = LayoutPoint::zero();

        for evt in path.iter() {
            match evt {
                PathEvent::Begin { at } => {
                    state.begin(at);
                    first = at;
                    from = at;
                }
                PathEvent::End { .. } => {
                    state.line_to(first, self);
                    state.end(self);
                }
                PathEvent::Line { to, .. } => {
                    state.line_to(to, self);
                    from = to;
                }
                PathEvent::Quadratic { ctrl, to, .. } => {
                    if flatten_curves {
                        bezier::flatten_quadratic(from, ctrl, to, tolerance, &mut |p| {
                            state.line_to(p, self);
                        });
                    } else {
                        state.line_to(to, self);
                    }
                    from = to;
                }
                PathEvent::Cubic { ctrl1, ctrl2, to, .. } => {
                    if flatten_curves {
                        bezier::flatten_cubic(from, ctrl1, ctrl2, to, tolerance, &mut |p| {
                            state.line_to(p, self);
                        });
                    } else {
                        state.line_to(to, self);
                    }
                    from = to;
                }
            }
        }
    }

    /// Bin a line segment, after clipping it to the area that affects the
    /// tiles in the scissor rect.
    ///
    /// The contribution of each segment to the tiles is independent from the
    /// other segments, so the parts that are above, below or on the right side
    /// of the culling rect can be dropped. The parts on the left side only
    /// contribute to the backdrops and are replaced with a vertical segment
    /// that crosses the same tile rows.
    fn bin_line(&mut self, from: DevicePoint, to: DevicePoint) {
        if !self.cull_segments {
            self.tile_segment_f32(from, to);
            return;
        }

        let x_min = self.culling_rect.min.x - 1.0;
        let x_max = self.culling_rect.max.x + 1.0;
        let y_min = self.culling_rect.min.y - 1.0;
        let y_max = self.culling_rect.max.y + 1.0;

        let inside = from.x >= x_min && from.x <= x_max && to.x >= x_min && to.x <= x_max
            && from.y >= y_min && from.y <= y_max && to.y >= y_min && to.y <= y_max;
        if inside {
            self.tile_segment_f32(from, to);
            return;
        }

        if !(from.x.is_finite() && from.y.is_finite() && to.x.is_finite() && to.y.is_finite()) {
            return;
        }

        let (mut a, mut b) = (from, to);

        // Clip vertically.
        if a.y.max(b.y) < y_min || a.y.min(b.y) > y_max {
            return;
        }
        if a.y < y_min || b.y < y_min || a.y > y_max || b.y > y_max {
            let dy = b.y - a.y;
            let clip_y = |y: f32| -> DevicePoint {
                let t = (y - from.y) / dy;
                point2(from.x + (to.x - from.x) * t, y)
            };
            if a.y < y_min { a = clip_y(y_min); }
            if a.y > y_max { a = clip_y(y_max); }
            if b.y < y_min { b = clip_y(y_min); }
            if b.y > y_max { b = clip_y(y_max); }
        }

        // Drop the part on the right side.
        if a.x.min(b.x) > x_max {
            return;
        }
        if a.x > x_max || b.x > x_max {
            let t = (x_max - a.x) / (b.x - a.x);
            let p = point2(x_max, a.y + (b.y - a.y) * t);
            if a.x > x_max { a = p; } else { b = p; }
        }

        // Replace the part on the left side with a vertical segment.
        if a.x.max(b.x) < x_min {
            self.tile_segment_f32(point2(x_min, a.y), point2(x_min, b.y));
            return;
        }
        if a.x < x_min || b.x < x_min {
            let t = (x_min - a.x) / (b.x - a.x);
            let p = point2(x_min, a.y + (b.y - a.y) * t);
            if a.x < x_min {
                self.tile_segment_f32(point2(x_min, a.y), p);
                a = p;
            } else {
                self.tile_segment_f32(p, point2(x_min, b.y));
                b = p;
            }
        }

        self.tile_segment_f32(a, b);
    }

    #[inline]
    fn tile_segment_f32(&mut self, from: DevicePoint, to: DevicePoint) {
        self.tile_segment(
            from, to,
            (from.x * COORD_SCALE) as i32, (from.y * COORD_SCALE) as i32,
            (to.x * COORD_SCALE) as i32, (to.y * COORD_SCALE) as i32,
        );
    }

    fn tile_segment(
        &mut self,
        from: DevicePoint,
        to: DevicePoint,
        from_i32_x: i32, from_i32_y: i32,
        to_i32_x: i32, to_i32_y: i32,
    ) {
        let _panic = PanicLogger::new("Bug: tile_segment panic with ", (from, to));

        let mut src_tx = from_i32_x >> LOCAL_COORD_BITS;
        let mut src_ty = from_i32_y >> LOCAL_COORD_BITS;
        let dst_tx = to_i32_x >> LOCAL_COORD_BITS;
        let dst_ty = to_i32_y >> LOCAL_COORD_BITS;

        // Fast path for segments contained in a single tile (the vast
        // majority of flattened curve segments). No tile boundary is
        // crossed so there is no backdrop or auxiliary edge to add,
        // and the edge fits in a single event. The scissor tile test
        // below subsumes the culling rect check for the top, bottom and
        // left sides, but not the right side: the last tile column can
        // extend past the scissor rect, so segments there still need the
        // horizontal culling test.
        if src_tx == dst_tx && src_ty == dst_ty {
            if src_ty >= self.scissor_tiles.min.y && src_ty < self.scissor_tiles.max.y
                && src_tx >= self.scissor_tiles.min.x && src_tx < self.scissor_tiles.max.x
                && !(self.cull_segments
                    && from.x.min(to.x) > self.culling_rect.max.x + 1.0)
            {
                // Both endpoints are in this tile so the local coordinates
                // are the low bits of the fixed-point coordinates.
                let local_x0 = encode_edge_coord(from_i32_x & LOCAL_COORD_MASK);
                let local_y0 = encode_edge_coord(from_i32_y & LOCAL_COORD_MASK);
                let local_x1 = encode_edge_coord(to_i32_x & LOCAL_COORD_MASK);
                let local_y1 = encode_edge_coord(to_i32_y & LOCAL_COORD_MASK);
                self.events.push(Event::edge(
                    src_tx as u16, src_ty as u16,
                    [local_x0, local_y0, local_x1, local_y1],
                ));
            }
            return;
        }

        // Leave some margin around this early scissor test so that
        // we keep track of the previous tile near the boundary of the
        // scissor rect.
        let min_x = to.x.min(from.x);
        let max_x = to.x.max(from.x);
        if self.cull_segments {
            let min_y = to.y.min(from.y);
            let max_y = to.y.max(from.y);
            if self.is_culled(min_x, min_y, max_y) {
                return;
            }
        }

        let scissor_min_tx = self.scissor_tiles.min.x;

        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let dx_dy = dx / dy;
        let dy_dx = 1.0 / dx_dy;

        let x_step = (dst_tx - src_tx).signum();
        let y_step = (dst_ty - src_ty).signum();
        let h_tile_side = x_step.max(0);
        let v_tile_side = y_step.max(0);

        let mut v_x0 = from.x;
        let mut v_y0 = from.y;

        // x and y index of the current tile.
        let mut tx = src_tx;
        let mut ty = src_ty;
        loop {
            let v_y1;
            let v_x1;
            let h_dst_tx;
            if ty == dst_ty {
                v_y1 = to.y;
                v_x1 = to.x;
                h_dst_tx = dst_tx;
            } else {
                v_y1 = (ty + v_tile_side) as f32 * TILE_SIZE_F32;
                // The min and max operations prevent limited arithmetic precision from
                // allowing the computed vertex to fall on a tile outside of the expected
                // range.
                v_x1 = (from.x + dx_dy * (v_y1 - from.y)).max(min_x).min(max_x);
                h_dst_tx = if x_step == 0 {
                    dst_tx
                } else {
                    (v_x1 * COORD_SCALE) as i32 >> LOCAL_COORD_BITS
                };
            }

            let row_occluded = ty < self.scissor_tiles.min.y || ty >= self.scissor_tiles.max.y;

            // When moving to a different row of tiles, add backdrop events.
            if ty != src_ty {
                let positive = ty > src_ty;
                let row = ty + (!positive) as i32;
                if row >= self.scissor_tiles.min.y && row < self.scissor_tiles.max.y && tx + 1 < self.scissor_tiles.max.x {
                    self.events.push(Event::backdrop((tx + 1).max(scissor_min_tx) as u16, row as u16, positive));
                }
                src_ty = ty;
            }

            let mut h_x0 = v_x0;
            let mut h_y0 = v_y0;
            debug_assert!(x_step > 0 || h_dst_tx <= tx);
            debug_assert!(x_step < 0 || h_dst_tx >= tx);

            loop {
                let h_x1;
                let h_y1;
                if tx == h_dst_tx {
                    h_x1 = v_x1;
                    h_y1 = v_y1;
                } else {
                    debug_assert!(x_step != 0, "tx {}, h_dst_tx {}, src_tx {} dst_tx {}", tx, h_dst_tx, src_tx, dst_tx);
                    h_x1 = (tx + h_tile_side) as f32 * TILE_SIZE_F32;
                    h_y1 = v_y0 + dy_dx * (h_x1 - v_x0);
                }

                let occluded = row_occluded || tx < self.scissor_tiles.min.x || tx >= self.scissor_tiles.max.x;

                if !row_occluded {
                    let offset_y = (ty * UNITS_PER_TILE) as f32;
                    let local_y0 = encode_edge_coord_f32((h_y0 * COORD_SCALE) - offset_y);

                    if !occluded {
                        let offset_x = (tx * UNITS_PER_TILE) as f32;
                        let local_x0 = encode_edge_coord_f32((h_x0 * COORD_SCALE) - offset_x);
                        let local_x1 = encode_edge_coord_f32((h_x1 * COORD_SCALE) - offset_x);
                        let local_y1 = encode_edge_coord_f32((h_y1 * COORD_SCALE) - offset_y);

                        debug_assert!(tx < self.scissor_tiles.max.x);
                        self.events.push(Event::edge(
                            tx as u16, ty as u16,
                            [local_x0, local_y0, local_x1, local_y1]
                        ));
                    }

                    // Add an auxiliary edge when crossing a vertical boundary (tile x
                    // coordinate changes).
                    if tx != src_tx {
                        // Note, if the edge is going in the negative x orientation, then
                        // we are actually adding an auxiliary edge to the previous tile
                        // rather than the current one.
                        let aux_tx = tx.max(src_tx);
                        let aux_occluded = aux_tx < self.scissor_tiles.min.x || aux_tx >= self.scissor_tiles.max.x;

                        if !aux_occluded {
                            let (y0, y1) = if tx < src_tx {
                                (local_y0, MAX_EDGE_COORD)
                            } else {
                                (MAX_EDGE_COORD, local_y0)
                            };
                            self.events.push(Event::edge(
                                aux_tx as u16, ty as u16,
                                [0, y0, 0, y1]
                            ));
                        }
                    }
                }

                src_tx = tx;
                debug_assert!(x_step > 0 || tx >= h_dst_tx);
                debug_assert!(x_step < 0 || tx <= h_dst_tx);

                if tx == h_dst_tx {
                    break;
                }
                h_x0 = h_x1;
                h_y0 = h_y1;
                tx += x_step;
            }

            v_x0 = v_x1;
            v_y0 = v_y1;
            if ty == dst_ty {
                break;
            }
            ty += y_step;
        }
    }

    fn push_mask_tile(
        &mut self,
        tile: TilePoint,
        backdrop: i16,
        path_index: u32,
        output: &mut PathTilerOutput,
    ) {
        let edge_count = self.edge_buffer.len();
        // This limit isn't necessary but it is useful to catch bugs. In practice there should
        // never be this many edges in a single tile.
        const MAX_EDGES_PER_TILE: usize = 4096;
        debug_assert!(edge_count < MAX_EDGES_PER_TILE, "bad tile at {:?}, {} edges", tile, edge_count);

        let first_edge = output.edges.push_slice(&self.edge_buffer);
        let edge_count = usize::min(edge_count, MAX_EDGES_PER_TILE) as u16;

        self.edge_buffer.clear();

        output.tiles.push(TileInstance {
            position: TilePosition::new(tile.x as u32, tile.y as u32),
            backdrop,
            first_edge,
            edge_count,
            path_index,
        }.encode());
    }

    fn generate_tiles(&mut self, fill_rule: FillRule, inverted: bool, path_index: u32, output: &mut PathTilerOutput) {
        if inverted {
            self.generate_tiles_inverted(fill_rule, path_index, output);
            return;
        }

        tracy_rs::profile_scope!("PathTiler::generate_tiles");

        if self.events.is_empty() {
            return;
        }

        let mut events = std::mem::take(&mut self.events);

        events.sort_unstable();
        // Push a dummy backdrop out of view that will cause the current tile to be flushed
        // at the last iteration without having to replicate the logic out of the loop.
        events.push(Event::backdrop(0, std::u16::MAX, false));

        let mut current = TilePoint::new(0, 0);
        let mut backdrop: i16 = 0;

        for evt in events.iter() {
            let tile = evt.tile();

            if current != tile {
                if !self.edge_buffer.is_empty() {
                    self.push_mask_tile(current, backdrop, path_index, output);
                    current.x += 1;
                }

                let x_end = if current.y == tile.y {
                    tile.x.min(self.scissor_tiles.max.x as u16)
                } else {
                    self.scissor_tiles.max.x as u16
                };

                let inside = match fill_rule {
                    FillRule::Evenodd => backdrop % 2 != 0,
                    FillRule::Nonzero => backdrop != 0,
                };

                if current.x < x_end && inside {
                    Self::fill_span(current.x, x_end, current.y, backdrop, path_index, output);
                }

                if current.y != tile.y {
                    // We moved to a new row of tiles.
                    if current.y >= self.scissor_tiles.max.y as u16 {
                        break;
                    }

                    backdrop = 0;
                }
                current = tile;
            }

            if evt.is_edge() {
                self.edge_buffer.push(GpuBufferBlockEdge(evt.payload().to_ne_bytes()));
            } else {
                let winding = if evt.payload() == 0 { -1 } else { 1 };
                backdrop += winding;
            }
        }

        self.events = events;
    }

    fn generate_tiles_inverted(&mut self, fill_rule: FillRule, path_index: u32, output: &mut PathTilerOutput) {
        tracy_rs::profile_scope!("PathTiler::generate_tiles_inverted");

        let min_x = self.scissor_tiles.min.x as u16;
        let max_x = self.scissor_tiles.max.x as u16;

        let mut events = std::mem::take(&mut self.events);

        events.sort_unstable();
        // Push a dummy backdrop out of view that will cause the current tile to be flushed
        // at the last iteration without having to replicate the logic out of the loop.
        events.push(Event::backdrop(0, std::u16::MAX, false));

        let mut current = TilePoint::new(min_x, self.scissor_tiles.min.y as u16);
        let mut backdrop: i16 = 0;

        for evt in events.iter() {
            let tile = evt.tile();

            if current != tile && !self.edge_buffer.is_empty() {
                self.push_mask_tile(current, backdrop, path_index, output);
                current.x += 1;
            }

            while current != tile {
                let next_x = if current.y == tile.y {
                    tile.x.min(max_x)
                } else {
                    max_x
                };

                let inside = !match fill_rule {
                    FillRule::Evenodd => backdrop % 2 != 0,
                    FillRule::Nonzero => backdrop != 0,
                };

                // Fill solid tiles if any, up to the new tile or the end of the current row.
                if inside && current.x < next_x {
                    Self::fill_span(current.x, next_x, current.y, backdrop, path_index, output);
                }

                if next_x == max_x {
                    // Reached the end of a row, move to the next.
                    backdrop = 0;
                    current.x = min_x;
                    current.y += 1;
                    // The dummy tile is the only one expected to be outside the visible
                    // area. No need to fill solid tiles
                    if current.y >= self.scissor_tiles.max.y as u16 {
                        break;
                    }
                } else {
                    current.x = next_x;
                }
            }

            if evt.is_edge() {
                self.edge_buffer.push(GpuBufferBlockEdge(evt.payload().to_ne_bytes()));
            } else {
                let winding = if evt.payload() == 0 { -1 } else { 1 };
                backdrop += winding;
            }
        }

        self.events = events;
    }

    /// Emit a solid tile stretched over `x..x_end`.
    fn fill_span(
        x: u16,
        x_end: u16,
        y: u16,
        backdrop: i16,
        path_index: u32,
        output: &mut PathTilerOutput,
    ) {
        if x >= x_end {
            return;
        }

        output.tiles.push(TileInstance {
            position: TilePosition::extended(x as u32, y as u32, (x_end - x - 1) as u32),
            backdrop,
            first_edge: 0,
            edge_count: 0,
            path_index,
        }.encode());
    }
}

/// Clips the projected edges of a sub-path against the `w = MIN_W` plane.
///
/// The part of the sub-path that is behind the plane is replaced with
/// segments along the plane's projection, connecting each point where the
/// sub-path exits the visible half-space to the point where it re-enters it.
struct ProjectedSubPath<'l> {
    transform: &'l Transform3D<f32, LayoutPixel, DevicePixel>,
    prev: euclid::HomogeneousVector<f32, DevicePixel>,
    exit: Option<DevicePoint>,
    first_entry: Option<DevicePoint>,
}

impl<'l> ProjectedSubPath<'l> {
    fn new(transform: &'l Transform3D<f32, LayoutPixel, DevicePixel>) -> Self {
        ProjectedSubPath {
            transform,
            prev: euclid::HomogeneousVector::new(0.0, 0.0, 0.0, 1.0),
            exit: None,
            first_entry: None,
        }
    }

    fn project(&self, p: LayoutPoint) -> euclid::HomogeneousVector<f32, DevicePixel> {
        self.transform.transform_point2d_homogeneous(p)
    }

    fn begin(&mut self, at: LayoutPoint) {
        self.prev = self.project(at);
        self.exit = None;
        self.first_entry = None;
    }

    fn line_to(&mut self, to: LayoutPoint, tiler: &mut PathTiler) {
        let a = self.prev;
        let b = self.project(to);
        self.prev = b;

        let to_2d = |h: euclid::HomogeneousVector<f32, DevicePixel>| point2(h.x / h.w, h.y / h.w);
        let on_plane = |a: euclid::HomogeneousVector<f32, DevicePixel>, b: euclid::HomogeneousVector<f32, DevicePixel>| {
            let t = (MIN_W - a.w) / (b.w - a.w);
            point2(
                (a.x + (b.x - a.x) * t) / MIN_W,
                (a.y + (b.y - a.y) * t) / MIN_W,
            )
        };

        match (a.w >= MIN_W, b.w >= MIN_W) {
            (true, true) => {
                tiler.bin_line(to_2d(a), to_2d(b));
            }
            (true, false) => {
                let p = on_plane(a, b);
                tiler.bin_line(to_2d(a), p);
                self.exit = Some(p);
            }
            (false, true) => {
                let p = on_plane(a, b);
                match self.exit.take() {
                    Some(exit) => tiler.bin_line(exit, p),
                    None => self.first_entry = Some(p),
                }
                tiler.bin_line(p, to_2d(b));
            }
            (false, false) => {}
        }
    }

    fn end(&mut self, tiler: &mut PathTiler) {
        if let (Some(exit), Some(entry)) = (self.exit.take(), self.first_entry.take()) {
            tiler.bin_line(exit, entry);
        }
    }
}

/// A sortable compressed event that encodes either a binned edge or a backdrop update.
///
/// The sort key (tile coordinates and the edge/backdrop flag) lives in the
/// high 32 bits and the payload in the low 32 bits, so sorting events as
/// plain integers groups them by tile with backdrops before edges. The
/// payload takes part in the comparison, which is fine: the relative order
/// of edges within a tile and of backdrops on the same tile does not matter.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Event(u64);

impl Event {
    fn edge(tx: u16, ty: u16, edge: [u8; 4]) -> Self {
        let key = 1 | ((tx as u64) << 1) | ((ty as u64) << 11);
        Event((key << 32) | u32::from_ne_bytes(edge) as u64)
    }

    fn backdrop(tx: u16, ty: u16, positive: bool) -> Self {
        let key = ((tx as u64) << 1) | ((ty as u64) << 11);
        Event((key << 32) | positive as u64)
    }

    fn is_edge(&self) -> bool {
        self.0 & (1 << 32) != 0
    }

    fn payload(&self) -> u32 {
        self.0 as u32
    }

    fn tile(&self) -> TilePoint {
        TilePoint::new(
            ((self.0 >> 33) & 0x3FF) as u16,
            ((self.0 >> 43) & 0x3FF) as u16,
        )
    }
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let tile = self.tile();
        if self.is_edge() {
            write!(f, "Edge at {:?}", tile)
        } else {
            write!(f, "Backdrop at {:?}", tile)
        }
    }
}

struct PanicLogger<'l, T: std::fmt::Debug> {
    _msg: &'l str,
    _payload: T,
}

impl<'l, T: std::fmt::Debug> PanicLogger<'l, T> {
    #[inline]
    pub fn new(_msg: &'l str, _payload: T) -> Self {
        PanicLogger {
            _msg,
            _payload,
        }
    }
}

impl<'l, T: std::fmt::Debug> Drop for PanicLogger<'l, T> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        if std::thread::panicking() {
            println!("{} {:?}", self._msg, self._payload);
        }
    }
}

#[test]
fn tile_position() {
    let p0 = TilePosition::new(1, 2);
    assert_eq!(p0.x(), 1);
    assert_eq!(p0.y(), 2);
    assert_eq!(p0.extension(), 0);

    let p1 = TilePosition::extended(1023, 1022, 1021);
    assert_eq!(p1.x(), 1023);
    assert_eq!(p1.y(), 1022);
    assert_eq!(p1.extension(), 1021);
}

#[test]
fn event() {
    let e1 = Event::edge(1, 3, [0, 0, 255, 50]);
    let e2 = Event::edge(2, 3, [0, 0, 255, 50]);
    let e3 = Event::edge(0b1111111111, 0b1101010101, [0, 1, 2, 3]);
    let b1 = Event::backdrop(1, 3, true);
    let b2 = Event::backdrop(2, 3, true);
    assert_eq!(e1.tile(), TilePoint::new(1, 3));
    assert_eq!(e2.tile(), TilePoint::new(2, 3));
    assert_eq!(e3.tile(), TilePoint::new(0b1111111111, 0b1101010101));
    assert_eq!(b1.tile(), TilePoint::new(1, 3));
    assert_eq!(b2.tile(), TilePoint::new(2, 3));
    let mut v = vec![e3, e2, e1, b2, b1];
    v.sort_unstable();
    assert_eq!(v, vec![b1, e1, b2, e2, e3]);
}

#[cfg(test)]
mod raster_tests {
    //! Rasterize the tiler's output on the CPU with the same algorithm as the
    //! `cs_path_tile` shader, and compare it against a supersampled reference.

    use super::*;
    use api::PathBuilder;
    use crate::internal_types::FrameMemory;

    fn rasterize_edge_analytical(p0: (f32, f32), p1: (f32, f32)) -> f32 {
        let y0 = p0.1.max(0.0).min(1.0);
        let y1 = p1.1.max(0.0).min(1.0);

        if y0 == y1 {
            return 0.0;
        }

        let inv_dy = 1.0 / (p1.1 - p0.1);
        let t0 = (y0 - p0.1) * inv_dy;
        let t1 = (y1 - p0.1) * inv_dy;
        let x0 = p0.0 * (1.0 - t0) + p1.0 * t0;
        let x1 = p0.0 * (1.0 - t1) + p1.0 * t1;

        let jitter = 1e-5;

        let xmin = x1.min(x0).min(1.0) - jitter;
        let xmax = x1.max(x0);
        let b = xmax.min(1.0);
        let c = b.max(0.0);
        let d = xmin.max(0.0);
        let area = (b + 0.5 * (d * d - c * c) - xmin) / (xmax - xmin);

        area * (y1 - y0)
    }

    fn resolve_mask(winding_number: f32, fill_rule: i32) -> f32 {
        let mut mask = if fill_rule & 1 == 0 {
            1.0 - ((winding_number.abs() % 2.0) - 1.0).abs()
        } else {
            winding_number.abs().min(1.0)
        };

        if fill_rule & 2 != 0 {
            mask = 1.0 - mask;
        }

        mask
    }

    /// Returns the inverted (or not) coverage of each pixel.
    fn rasterize(
        path: &Path,
        transform: &PathTransform,
        fill_rule: FillRule,
        inverted: bool,
        width: i32,
        height: i32,
    ) -> Vec<f32> {
        let memory = FrameMemory::fallback();
        let mut edges = PathEdgeBufferBuilder::new(&memory, 0);
        let mut tiles = memory.new_vec();

        let mut tiler = PathTiler::new();
        tiler.begin_target(DeviceIntRect::from_size(DeviceIntSize::new(width, height)));
        tiler.fill_path(
            path,
            transform,
            fill_rule,
            inverted,
            0,
            &mut PathTilerOutput { edges: &mut edges, tiles: &mut tiles },
        );

        let edges = edges.finalize();
        let encoded_fill_rule = PathInfo {
            render_task_address: 0,
            fill_rule,
            inverted,
            opacity: 1.0,
        }.encode()[1];

        let mut pixels = vec![0.0; (width * height) as usize];
        let mut written = vec![false; (width * height) as usize];

        for tile in tiles.iter() {
            let tile = TileInstance::decode(*tile);
            let x0 = tile.position.x() as i32 * TILE_SIZE;
            let y0 = tile.position.y() as i32 * TILE_SIZE;
            let x1 = (x0 + TILE_SIZE * (1 + tile.position.extension() as i32)).min(width);
            let y1 = (y0 + TILE_SIZE).min(height);
            for y in y0..y1 {
                for x in x0..x1 {
                    let uv = ((x - x0) as f32 + 0.5, (y - y0) as f32 + 0.5);
                    let mut winding = tile.backdrop as f32;
                    let start = tile.first_edge as usize;
                    let end = start + tile.edge_count as usize;
                    for edge in &edges.data[start..end] {
                        let e = edge.0;
                        let s = 1.0 / EDGE_STEPS_PER_PIXEL as f32;
                        let p0 = (e[0] as f32 * s - uv.0 + 0.5, e[1] as f32 * s - uv.1 + 0.5);
                        let p1 = (e[2] as f32 * s - uv.0 + 0.5, e[3] as f32 * s - uv.1 + 0.5);
                        winding += rasterize_edge_analytical(p0, p1);
                    }
                    let idx = (y * width + x) as usize;
                    assert!(!written[idx], "overlapping tiles at {} {}", x, y);
                    written[idx] = true;
                    pixels[idx] = resolve_mask(winding, encoded_fill_rule);
                }
            }
        }

        pixels
    }

    fn flatten(path: &Path, transform: &PathTransform) -> Vec<Vec<DevicePoint>> {
        let map = |p: LayoutPoint| -> DevicePoint {
            match transform {
                PathTransform::Affine(t) => t.transform_point(p),
                PathTransform::Projective(t) => t.transform_point2d(p).unwrap(),
            }
        };
        let mut polygons = Vec::new();
        let mut current = Vec::new();
        for evt in path.iter() {
            match evt {
                PathEvent::Begin { at } => current.push(map(at)),
                PathEvent::Line { to, .. } => current.push(map(to)),
                PathEvent::Quadratic { from, ctrl, to } => {
                    bezier::flatten_quadratic(from, ctrl, to, 0.01, &mut |p| current.push(map(p)));
                }
                PathEvent::Cubic { from, ctrl1, ctrl2, to } => {
                    bezier::flatten_cubic(from, ctrl1, ctrl2, to, 0.01, &mut |p| current.push(map(p)));
                }
                PathEvent::End { .. } => polygons.push(std::mem::take(&mut current)),
            }
        }
        polygons
    }

    fn winding_number(polygons: &[Vec<DevicePoint>], p: (f32, f32)) -> i32 {
        let mut winding = 0;
        for polygon in polygons {
            for i in 0..polygon.len() {
                let a = polygon[i];
                let b = polygon[(i + 1) % polygon.len()];
                if (a.y <= p.1) != (b.y <= p.1) {
                    let x = a.x + (p.1 - a.y) * (b.x - a.x) / (b.y - a.y);
                    if x > p.0 {
                        winding += if b.y > a.y { 1 } else { -1 };
                    }
                }
            }
        }
        winding
    }

    fn reference(
        path: &Path,
        transform: &PathTransform,
        fill_rule: FillRule,
        inverted: bool,
        width: i32,
        height: i32,
    ) -> Vec<f32> {
        const N: i32 = 8;
        let polygons = flatten(path, transform);
        let mut pixels = vec![0.0; (width * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let mut covered = 0;
                for sy in 0..N {
                    for sx in 0..N {
                        let p = (
                            x as f32 + (sx as f32 + 0.5) / N as f32,
                            y as f32 + (sy as f32 + 0.5) / N as f32,
                        );
                        let wn = winding_number(&polygons, p);
                        let inside = match fill_rule {
                            FillRule::Evenodd => wn % 2 != 0,
                            FillRule::Nonzero => wn != 0,
                        };
                        if inside != inverted {
                            covered += 1;
                        }
                    }
                }
                pixels[(y * width + x) as usize] = covered as f32 / (N * N) as f32;
            }
        }
        pixels
    }

    fn check(path: &Path, transform: &PathTransform, fill_rule: FillRule, width: i32, height: i32) {
        for &inverted in &[false, true] {
            let actual = rasterize(path, transform, fill_rule, inverted, width, height);
            let expected = reference(path, transform, fill_rule, inverted, width, height);
            let mut total_error = 0.0;
            for y in 0..height {
                for x in 0..width {
                    let idx = (y * width + x) as usize;
                    let error = (actual[idx] - expected[idx]).abs();
                    total_error += error;
                    assert!(
                        error < 0.4,
                        "pixel {} {}: got {} expected {} (inverted: {}, fill rule: {:?})",
                        x, y, actual[idx], expected[idx], inverted, fill_rule,
                    );
                }
            }
            let average_error = total_error / (width * height) as f32;
            assert!(average_error < 0.01, "average error {}", average_error);
        }
    }

    fn star() -> Path {
        let mut builder = PathBuilder::new();
        builder.begin(point2(50.0, 2.0));
        builder.line_to(point2(80.0, 95.0));
        builder.line_to(point2(3.0, 37.0));
        builder.line_to(point2(97.0, 37.0));
        builder.line_to(point2(20.0, 95.0));
        builder.end(true);
        builder.build()
    }

    fn circle(center: LayoutPoint, radius: f32) -> Path {
        const K: f32 = 0.5522848;
        let r = radius;
        let k = r * K;
        let c = center;
        let mut builder = PathBuilder::new();
        builder.begin(point2(c.x + r, c.y));
        builder.cubic_bezier_to(point2(c.x + r, c.y + k), point2(c.x + k, c.y + r), point2(c.x, c.y + r));
        builder.cubic_bezier_to(point2(c.x - k, c.y + r), point2(c.x - r, c.y + k), point2(c.x - r, c.y));
        builder.cubic_bezier_to(point2(c.x - r, c.y - k), point2(c.x - k, c.y - r), point2(c.x, c.y - r));
        builder.quadratic_bezier_to(point2(c.x + r, c.y - r), point2(c.x + r, c.y));
        builder.end(true);
        builder.build()
    }

    fn identity() -> PathTransform {
        PathTransform::Affine(Transform2D::identity())
    }

    #[test]
    fn star_even_odd() {
        check(&star(), &identity(), FillRule::Evenodd, 100, 100);
    }

    #[test]
    fn star_non_zero() {
        check(&star(), &identity(), FillRule::Nonzero, 100, 100);
    }

    #[test]
    fn circle_inside() {
        check(&circle(point2(40.0, 35.0), 30.0), &identity(), FillRule::Nonzero, 80, 70);
    }

    #[test]
    fn partially_outside() {
        // Overlaps the four sides of the target, including tiles on the left
        // and top sides that only contribute to backdrops.
        let path = circle(point2(30.0, 25.0), 40.0);
        check(&path, &identity(), FillRule::Nonzero, 50, 45);
        let path = circle(point2(60.0, 20.0), 50.0);
        check(&path, &identity(), FillRule::Evenodd, 70, 50);
    }

    #[test]
    fn edge_coord_encoding() {
        assert_eq!(encode_edge_coord(0), 0);
        assert_eq!(encode_edge_coord(UNITS_PER_TILE), MAX_EDGE_COORD);
        assert_eq!(encode_edge_coord_f32(0.0), 0);
        assert_eq!(encode_edge_coord_f32(-1.0), 0);
        assert_eq!(encode_edge_coord_f32(UNITS_PER_TILE_F32), MAX_EDGE_COORD);
        assert_eq!(encode_edge_coord_f32(1000.0), MAX_EDGE_COORD);
        for units in 0..UNITS_PER_TILE {
            assert_eq!(encode_edge_coord(units), encode_edge_coord_f32(units as f32));
        }
    }

    #[test]
    fn axis_aligned_precision() {
        // Edges on integer pixel coordinates are encoded exactly.
        let mut builder = PathBuilder::new();
        builder.begin(point2(3.0, 5.0));
        builder.line_to(point2(29.0, 5.0));
        builder.line_to(point2(29.0, 27.0));
        builder.line_to(point2(3.0, 27.0));
        builder.end(true);
        builder.begin(point2(9.0, 11.0));
        builder.line_to(point2(9.0, 21.0));
        builder.line_to(point2(23.0, 21.0));
        builder.line_to(point2(23.0, 11.0));
        builder.end(true);
        let path = builder.build();

        for &inverted in &[false, true] {
            let actual = rasterize(&path, &identity(), FillRule::Nonzero, inverted, 40, 40);
            let expected = reference(&path, &identity(), FillRule::Nonzero, inverted, 40, 40);
            for (idx, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (a - e).abs() < 0.001,
                    "pixel {} {}: got {} expected {}", idx % 40, idx / 40, a, e,
                );
            }
        }
    }

    #[test]
    fn open_sub_path() {
        let mut builder = PathBuilder::new();
        builder.begin(point2(5.0, 5.0));
        builder.line_to(point2(60.0, 10.0));
        builder.line_to(point2(30.0, 50.0));
        builder.end(false);
        check(&builder.build(), &identity(), FillRule::Nonzero, 64, 64);
    }

    #[test]
    fn transformed() {
        let transform = PathTransform::Affine(
            Transform2D::rotation(euclid::Angle::degrees(30.0))
                .then_scale(0.7, 1.3)
                .then_translate(euclid::vec2(40.0, -10.0))
        );
        check(&star(), &transform, FillRule::Evenodd, 120, 120);
    }

    #[test]
    fn large_coordinates() {
        // Segments spanning far outside of the target on every side.
        let mut builder = PathBuilder::new();
        builder.begin(point2(-5000.0, -3000.0));
        builder.line_to(point2(40.0, 30.0));
        builder.line_to(point2(6000.0, -2000.0));
        builder.line_to(point2(3000.0, 9000.0));
        builder.line_to(point2(-4000.0, 7000.0));
        builder.end(true);
        check(&builder.build(), &identity(), FillRule::Nonzero, 64, 64);
    }

    #[test]
    fn huge_coordinates() {
        // f32 has no precision left to interpolate the edges of such a path
        // in the target, so stick to axis-aligned edges.
        let mut builder = PathBuilder::new();
        builder.begin(point2(-1.0e9, -1.0e9));
        builder.line_to(point2(30.0, -1.0e9));
        builder.line_to(point2(30.0, 1.0e9));
        builder.line_to(point2(-1.0e9, 1.0e9));
        builder.end(true);
        builder.begin(point2(-2.0e9, 20.0));
        builder.line_to(point2(3.0e9, 20.0));
        builder.line_to(point2(3.0e9, 1.0e10));
        builder.line_to(point2(-2.0e9, 1.0e10));
        builder.end(true);
        check(&builder.build(), &identity(), FillRule::Evenodd, 64, 64);
    }

    #[test]
    fn huge_curves() {
        // Only checks that this terminates without panicking.
        let mut builder = PathBuilder::new();
        builder.begin(point2(-1.0e8, 20.0));
        builder.cubic_bezier_to(point2(0.0, -1.0e8), point2(100.0, 1.0e8), point2(1.0e8, 30.0));
        builder.quadratic_bezier_to(point2(1.0e20, 1.0e20), point2(1.0e8, 50.0));
        builder.line_to(point2(-1.0e8, 50.0));
        builder.end(true);
        let path = builder.build();
        for &inverted in &[false, true] {
            let pixels = rasterize(&path, &identity(), FillRule::Nonzero, inverted, 64, 64);
            assert!(pixels.iter().all(|p| *p >= 0.0 && *p <= 1.0));
        }
    }

    #[test]
    fn perspective() {
        let transform = Transform3D::<f32, LayoutPixel, DevicePixel>::perspective(200.0)
            .pre_rotate(0.0, 1.0, 0.0, euclid::Angle::degrees(30.0))
            .then_translate(euclid::vec3(10.0, 5.0, 0.0));
        let transform = PathTransform::new(&transform);
        assert!(matches!(transform, PathTransform::Projective(..)));
        check(&circle(point2(40.0, 40.0), 35.0), &transform, FillRule::Nonzero, 100, 100);
    }

    #[test]
    fn non_finite() {
        let mut builder = PathBuilder::new();
        builder.begin(point2(0.0, 0.0));
        builder.line_to(point2(f32::NAN, 10.0));
        builder.line_to(point2(10.0, 10.0));
        builder.end(true);
        let path = builder.build();
        let pixels = rasterize(&path, &identity(), FillRule::Nonzero, true, 32, 32);
        assert!(pixels.iter().all(|p| *p == 1.0));
        let pixels = rasterize(&path, &identity(), FillRule::Nonzero, false, 32, 32);
        assert!(pixels.iter().all(|p| *p == 0.0));
    }
}
