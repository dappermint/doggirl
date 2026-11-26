/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::OnceLock;

use crate::geometry::{FloatPoint, FloatSize};
use crate::matrix::AffineTransform;
use crate::paint_enums::WindingRule;
use geometry::PathGeometry;
pub use geometry::PathVerb;
pub use measure::PathVertex;

mod arcs;
mod curves;
mod encoding;
pub mod ffi;
mod geometry;
mod hit_test;
mod measure;
mod stroke;
mod svg_text;

unsafe extern "C" {
    fn ladybird_gfx_process_next_path_identity() -> u64;
    fn ladybird_gfx_font_glyph_outlines(
        font: *const c_void,
        glyphs: *const crate::text_layout::DrawGlyph,
        glyph_count: usize,
        visit: unsafe extern "C" fn(*mut c_void, usize, *const ffi::PathGeometryData),
        context: *mut c_void,
    );
}

/// A shaped run of glyphs in one font, as the path operations below take one.
pub struct GlyphRun {
    pub font: crate::font::FontHandle,
    pub glyphs: crate::text_layout::GlyphBuffer,
}

const GLYPH_OUTLINE_CACHE_SIZE: usize = 1024;

#[derive(Clone, Default)]
struct CachedGlyphOutline {
    font_id: u64,
    glyph_id: u16,
    verbs: Vec<PathVerb>,
    points: Vec<FloatPoint>,
    conic_weights: Vec<f32>,
}

thread_local! {
    static GLYPH_OUTLINE_CACHE: RefCell<Box<[CachedGlyphOutline]>> = RefCell::new(
        vec![CachedGlyphOutline::default(); GLYPH_OUTLINE_CACHE_SIZE].into_boxed_slice()
    );
}

/// Clears the calling thread's cached glyph outlines, e.g. on memory pressure or cache flush.
pub fn clear_glyph_outline_cache() {
    GLYPH_OUTLINE_CACHE.with(|cache_cell| {
        if let Ok(mut cache) = cache_cell.try_borrow_mut() {
            for entry in cache.iter_mut() {
                *entry = CachedGlyphOutline::default();
            }
        }
    });
}

#[inline(always)]
fn glyph_outline_cache_slot(font_id: u64, glyph_id: u16) -> usize {
    let hash = font_id.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(glyph_id).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    ((hash ^ (hash >> 32)) as usize) & (GLYPH_OUTLINE_CACHE_SIZE - 1)
}

struct MissingGlyphOutlineSink<'a> {
    font_id: u64,
    glyphs: &'a [crate::text_layout::DrawGlyph],
    fetched: &'a mut Vec<CachedGlyphOutline>,
}

unsafe extern "C" fn collect_missing_glyph_outline(
    context: *mut c_void,
    glyph_index: usize,
    outline: *const ffi::PathGeometryData,
) {
    // SAFETY: `context` is the sink passed by `append_glyph_outlines_cached()` and `outline` is valid for this call.
    unsafe {
        let sink = &mut *context.cast::<MissingGlyphOutlineSink<'_>>();
        let data = &*outline;
        let verbs = ak::slice_from_raw_parts(data.verbs.cast::<PathVerb>(), data.verb_count);
        let points = ak::slice_from_raw_parts(data.points, data.point_count);
        let conic_weights = ak::slice_from_raw_parts(data.conic_weights, data.conic_weight_count);
        sink.fetched.push(CachedGlyphOutline {
            font_id: sink.font_id,
            glyph_id: sink.glyphs[glyph_index].glyph_id,
            verbs: verbs.to_vec(),
            points: points.to_vec(),
            conic_weights: conic_weights.to_vec(),
        });
    }
}

/// Core cache lookup and append helper, factored out so collision behavior can be unit tested without FFI.
fn append_glyph_outlines_with_cache(
    cache: &mut [CachedGlyphOutline],
    geometry: &mut PathGeometry,
    font_id: u64,
    glyphs: &[crate::text_layout::DrawGlyph],
    fetch_missing: impl FnOnce(&[crate::text_layout::DrawGlyph], &mut Vec<CachedGlyphOutline>),
    mut append_one: impl FnMut(&mut PathGeometry, usize, &[PathVerb], &[FloatPoint], &[f32]),
) {
    if glyphs.is_empty() {
        return;
    }

    // Fast path: check whether every glyph in this run is already cached so we avoid any allocations or FFI.
    let mut all_hit = true;
    for glyph in glyphs {
        let slot = glyph_outline_cache_slot(font_id, glyph.glyph_id);
        let entry = &cache[slot];
        if entry.font_id != font_id || entry.glyph_id != glyph.glyph_id {
            all_hit = false;
            break;
        }
    }
    if all_hit {
        for (index, glyph) in glyphs.iter().enumerate() {
            let entry = &cache[glyph_outline_cache_slot(font_id, glyph.glyph_id)];
            append_one(geometry, index, &entry.verbs, &entry.points, &entry.conic_weights);
        }
        return;
    }

    // Collect unique missing glyphs in order and fetch them in one batch.
    let mut missing_glyphs: Vec<crate::text_layout::DrawGlyph> = Vec::new();
    for glyph in glyphs {
        let slot = glyph_outline_cache_slot(font_id, glyph.glyph_id);
        let entry = &cache[slot];
        if (entry.font_id != font_id || entry.glyph_id != glyph.glyph_id)
            && !missing_glyphs
                .iter()
                .any(|existing| existing.glyph_id == glyph.glyph_id)
        {
            missing_glyphs.push(*glyph);
        }
    }
    let mut fetched = Vec::with_capacity(missing_glyphs.len());
    if !missing_glyphs.is_empty() {
        fetch_missing(&missing_glyphs, &mut fetched);
    }

    // Append every glyph BEFORE writing `fetched` into `cache`, so a newly fetched glyph that hashes to the same
    // direct-mapped slot as an already-cached glyph in this run cannot evict it before it is read.
    for (index, glyph) in glyphs.iter().enumerate() {
        let slot = glyph_outline_cache_slot(font_id, glyph.glyph_id);
        let entry = &cache[slot];
        if entry.font_id == font_id && entry.glyph_id == glyph.glyph_id {
            append_one(geometry, index, &entry.verbs, &entry.points, &entry.conic_weights);
        } else if let Some(item) = fetched.iter().find(|item| item.glyph_id == glyph.glyph_id) {
            append_one(geometry, index, &item.verbs, &item.points, &item.conic_weights);
        } else {
            debug_assert!(
                false,
                "glyph {} missing from both cache and fetched outlines",
                glyph.glyph_id
            );
            append_one(geometry, index, &[], &[], &[]);
        }
    }

    // Move fetched entries into the direct-mapped cache without cloning.
    for item in fetched {
        let slot = glyph_outline_cache_slot(font_id, item.glyph_id);
        cache[slot] = item;
    }
}

/// Fetches uncached outlines for `glyphs` in a single FFI call and invokes `append_one` for every glyph in order.
fn append_glyph_outlines_cached(
    geometry: &mut PathGeometry,
    font: &crate::font::FontHandle,
    glyphs: &[crate::text_layout::DrawGlyph],
    append_one: impl FnMut(&mut PathGeometry, usize, &[PathVerb], &[FloatPoint], &[f32]),
) {
    if glyphs.is_empty() {
        return;
    }
    let font_id = font.id().0;
    GLYPH_OUTLINE_CACHE.with(|cache_cell| {
        let mut cache = cache_cell.borrow_mut();
        append_glyph_outlines_with_cache(
            &mut cache,
            geometry,
            font_id,
            glyphs,
            |missing_glyphs, fetched| {
                let mut sink = MissingGlyphOutlineSink {
                    font_id,
                    glyphs: missing_glyphs,
                    fetched,
                };
                // SAFETY: `font` keeps the Gfx::Font live and `missing_glyphs`/`sink` stay valid for the synchronous call.
                unsafe {
                    ladybird_gfx_font_glyph_outlines(
                        font.as_raw(),
                        missing_glyphs.as_ptr(),
                        missing_glyphs.len(),
                        collect_missing_glyph_outline,
                        (&raw mut sink).cast(),
                    );
                }
            },
            append_one,
        );
    });
}

/// Adds the outline of each glyph, in pixels with its origin on the baseline, mapped by the transform at the same
/// index.
fn append_glyph_outlines(
    geometry: &mut PathGeometry,
    font: &crate::font::FontHandle,
    glyphs: &[crate::text_layout::DrawGlyph],
    transforms: &[AffineTransform],
) {
    debug_assert_eq!(glyphs.len(), transforms.len());
    append_glyph_outlines_cached(
        geometry,
        font,
        glyphs,
        |geometry, index, verbs, points, conic_weights| {
            let transform = transforms[index];
            geometry.append_mapped_contours(verbs, points, conic_weights, |point| transform.map_point(point));
        },
    );
}

/// How a path is stroked, as `Path::stroke_to_fill()` takes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StrokeStyle {
    pub thickness: f32,
    pub cap_style: crate::CapStyle,
    pub join_style: crate::JoinStyle,
    pub miter_limit: f32,
    pub dash_array: Vec<f32>,
    pub dash_offset: f32,
}

pub struct Path {
    geometry: PathGeometry,
    // A process-unique, never-reused identity, so that consumers that hold a copied snapshot can
    // recognize an unchanged path without comparing contents. It is taken when first asked for, and a
    // change to the path drops it.
    identity: OnceLock<u64>,
}

impl Default for Path {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Path {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Path")
            .field("identity", &self.identity.get())
            .finish()
    }
}

impl Path {
    pub fn new() -> Self {
        Self::from_geometry(PathGeometry::new())
    }

    fn from_geometry(geometry: PathGeometry) -> Self {
        Self {
            geometry,
            identity: OnceLock::new(),
        }
    }

    fn geometry_mut(&mut self) -> &mut PathGeometry {
        self.identity.take();
        &mut self.geometry
    }

    #[inline]
    pub fn identity(&self) -> u64 {
        // The counter is LibGfx's: this crate is compiled into more than one library, and a counter
        // here would let each of them hand out the same identities. See `LibGfx/RustProcessState.cpp`.
        // SAFETY: The counter is an atomic on the other side of the boundary.
        *self
            .identity
            .get_or_init(|| unsafe { ladybird_gfx_process_next_path_identity() })
    }

    pub fn move_to(&mut self, point: FloatPoint) {
        self.geometry_mut().move_to(point);
    }

    pub fn line_to(&mut self, point: FloatPoint) {
        self.geometry_mut().line_to(point);
    }

    pub fn quadratic_bezier_curve_to(&mut self, through: FloatPoint, point: FloatPoint) {
        self.geometry_mut().quadratic_bezier_curve_to(through, point);
    }

    pub fn cubic_bezier_curve_to(&mut self, c1: FloatPoint, c2: FloatPoint, point: FloatPoint) {
        self.geometry_mut().cubic_bezier_curve_to(c1, c2, point);
    }

    /// An arc of an ellipse, with the parameters of the SVG path command. The rotation is in radians.
    pub fn elliptical_arc_to(
        &mut self,
        point: FloatPoint,
        radii: FloatSize,
        x_axis_rotation: f32,
        large_arc: bool,
        sweep: bool,
    ) {
        self.geometry_mut()
            .elliptical_arc_to(point, (radii.width, radii.height), x_axis_rotation, large_arc, sweep);
    }

    pub fn arc_to(&mut self, point: FloatPoint, radius: f32, large_arc: bool, sweep: bool) {
        self.geometry_mut().arc_to(point, radius, large_arc, sweep);
    }

    pub fn close(&mut self) {
        self.geometry_mut().close();
    }

    /// Adds all the subpaths of the other path to this one.
    pub fn append_path(&mut self, other: &Path) {
        self.geometry_mut().append_path(&other.geometry);
    }

    pub fn set_fill_rule(&mut self, fill_rule: WindingRule) {
        self.geometry_mut().set_fill_rule(fill_rule);
    }

    /// Whether the path has no current point.
    pub fn is_empty(&self) -> bool {
        self.geometry.is_empty()
    }

    pub fn last_point(&self) -> FloatPoint {
        self.geometry.last_point()
    }

    pub fn bounding_box(&self) -> [f32; 4] {
        self.geometry.bounding_box()
    }

    pub fn length(&self) -> f32 {
        measure::length(&self.geometry)
    }

    /// The point at the given distance along the path, followed by the path's tangent there.
    pub fn point_at_length(&self, distance: f32) -> Option<[f32; 4]> {
        measure::point_at_length(&self.geometry, distance)
            .map(|(point, tangent)| [point.x, point.y, tangent.x, tangent.y])
    }

    /// The vertices of polylines through the path, with their distance along it (see [`measure::vertices`]).
    pub fn vertices(&self) -> Vec<PathVertex> {
        measure::vertices(&self.geometry)
    }

    pub fn contains(&self, x: f32, y: f32, winding_rule: WindingRule) -> bool {
        hit_test::contains(&self.geometry, x, y, winding_rule)
    }

    /// The outline of the area that stroking this path with the given style covers, as a path to fill with the
    /// nonzero winding rule.
    pub fn stroke_to_fill(&self, style: &StrokeStyle) -> Path {
        Self::from_geometry(stroke::stroke_to_fill(
            &self.geometry,
            &stroke::StrokeStyle {
                thickness: style.thickness,
                cap_style: style.cap_style,
                join_style: style.join_style,
                miter_limit: style.miter_limit,
                dash_array: &style.dash_array,
                dash_offset: style.dash_offset,
            },
        ))
    }

    pub fn copy_transformed(&self, affine_values: [f32; 6]) -> Path {
        Self::from_geometry(
            self.geometry
                .copy_transformed(AffineTransform { values: affine_values }),
        )
    }

    pub fn to_svg_string(&self) -> String {
        svg_text::to_svg_string(&self.geometry)
    }

    pub fn clear(&mut self) {
        self.geometry_mut().clear();
    }

    pub fn serialize_to_bytes(&self) -> Vec<u8> {
        encoding::encode(&self.geometry)
    }

    pub fn serialize_into(&self, bytes: &mut Vec<u8>) {
        encoding::encode_into(&self.geometry, bytes);
    }

    /// The path that the bytes describe. Bytes that describe no path give an empty path.
    pub fn from_serialized_bytes(bytes: &[u8]) -> Path {
        Self::from_geometry(encoding::decode(bytes).unwrap_or_default())
    }

    /// The outlines of the given glyphs, as one path.
    pub fn from_glyph_runs(runs: &[GlyphRun]) -> Path {
        let mut geometry = PathGeometry::new();
        for run in runs {
            if run.font.is_invisible() || run.glyphs.is_empty() {
                continue;
            }
            let ascent = run.font.facts().ascent;
            let mut placed = run.glyphs.placed();
            append_glyph_outlines_cached(
                &mut geometry,
                &run.font,
                &run.glyphs,
                |geometry, _index, verbs, points, conic_weights| {
                    let placed_glyph = placed.next().expect("placed glyphs match glyph count");
                    let dx = placed_glyph.x;
                    let dy = placed_glyph.y + ascent;
                    geometry.append_mapped_contours(verbs, points, conic_weights, |point| FloatPoint {
                        x: point.x + dx,
                        y: point.y + dy,
                    });
                },
            );
        }
        geometry.update_state_from_contours();
        Self::from_geometry(geometry)
    }

    /// The outlines of the given glyphs, laid along the first subpath of this path that has length, as SVG lays out
    /// text on a path: Each glyph is centered on the point at the distance of the middle of its advance (its x position
    /// plus the offset, plus half its advance), turned to the direction of the path there, and moved off the path, at
    /// a right angle to it, by as much as its baseline is shifted. A glyph whose middle is off either end isn't drawn.
    /// https://svgwg.org/svg2-draft/text.html#TextLayoutPath
    pub fn place_glyph_runs_along(&self, runs: &[GlyphRun], offset: f32) -> Path {
        let mut geometry = PathGeometry::new();
        let Some(contour) = measure::ContourMeasure::contours_of(&self.geometry).next() else {
            return Self::from_geometry(geometry);
        };
        let path_length = contour.length();
        for run in runs {
            if run.font.is_invisible() {
                continue;
            }
            let ascent = run.font.facts().ascent;
            let mut glyphs = Vec::with_capacity(run.glyphs.len());
            let mut transforms = Vec::with_capacity(run.glyphs.len());
            for placed in run.glyphs.placed() {
                let half_advance = placed.glyph.advance / 2.0;
                let midpoint_distance = offset + placed.x + half_advance;
                // "Glyphs whose midpoint-on-the-path are off the path are not rendered."
                if !(0.0..=path_length).contains(&midpoint_distance) {
                    continue;
                }
                // AD-HOC: The spec turns each glyph to the line through the start and the end of its advance on the
                //         path. Like other engines, this turns it to the direction of the path at its midpoint.
                let (midpoint, tangent) = contour.position_and_tangent(midpoint_distance);
                // A glyph's y is the top of its em box, an ascent above its baseline.
                let baseline_shift = placed.y + ascent;
                let normal = (-tangent.y, tangent.x);
                transforms.push(AffineTransform {
                    values: [
                        tangent.x,
                        tangent.y,
                        normal.0,
                        normal.1,
                        midpoint.x - tangent.x * half_advance + normal.0 * baseline_shift,
                        midpoint.y - tangent.y * half_advance + normal.1 * baseline_shift,
                    ],
                });
                glyphs.push(placed.glyph);
            }
            append_glyph_outlines(&mut geometry, &run.font, &glyphs, &transforms);
        }
        geometry.update_state_from_contours();
        Self::from_geometry(geometry)
    }

    /// The verbs of the contours of the path, each of which takes as many of the points as it stores.
    pub fn verbs(&self) -> &[PathVerb] {
        self.geometry.verbs()
    }

    pub fn points(&self) -> &[FloatPoint] {
        self.geometry.points()
    }

    /// The weight of each conic of the contours, in order.
    pub fn conic_weights(&self) -> &[f32] {
        self.geometry.conic_weights()
    }

    /// The contours of the path, as C++ reads them, valid until the path changes or is dropped.
    pub fn geometry_data(&self) -> ffi::PathGeometryData {
        ffi::PathGeometryData::of(&self.geometry)
    }
}

/// A copy of the path, which has an identity of its own.
impl Clone for Path {
    fn clone(&self) -> Self {
        Self::from_geometry(self.geometry.clone())
    }
}

impl PartialEq for Path {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other) || self.geometry.has_same_contours(&other.geometry)
    }
}

// The contours are compared bit for bit, which is reflexive even for NaN.
impl Eq for Path {}

#[cfg(test)]
mod tests {
    use super::*;
    use ak::math;

    fn point(x: f32, y: f32) -> FloatPoint {
        FloatPoint { x, y }
    }

    fn assert_approximately(actual: f32, expected: f32, tolerance: f32) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    fn assert_point_along_path(path: &Path, distance: f32, expected_point: FloatPoint, expected_tangent: FloatPoint) {
        let [x, y, tangent_x, tangent_y] = path
            .point_at_length(distance)
            .expect("a path with a point has a point at every distance");
        for (actual, expected) in [
            (x, expected_point.x),
            (y, expected_point.y),
            (tangent_x, expected_tangent.x),
            (tangent_y, expected_tangent.y),
        ] {
            assert_approximately(actual, expected, 0.000_000_5);
        }
    }

    #[test]
    fn point_at_length_along_a_line() {
        let mut path = Path::new();
        path.move_to(point(0.0, 0.0));
        path.line_to(point(100.0, 0.0));

        assert_point_along_path(&path, 25.0, point(25.0, 0.0), point(1.0, 0.0));
        assert_point_along_path(&path, -10.0, point(0.0, 0.0), point(1.0, 0.0));
        assert_point_along_path(&path, 1000.0, point(100.0, 0.0), point(1.0, 0.0));
    }

    #[test]
    fn point_at_length_skips_the_gaps_between_subpaths() {
        let mut path = Path::new();
        path.move_to(point(0.0, 0.0));
        path.line_to(point(10.0, 0.0));
        path.move_to(point(50.0, 50.0));
        path.move_to(point(100.0, 100.0));
        path.line_to(point(100.0, 110.0));

        assert_point_along_path(&path, 5.0, point(5.0, 0.0), point(1.0, 0.0));
        assert_point_along_path(&path, 15.0, point(100.0, 105.0), point(0.0, 1.0));
        assert_point_along_path(&path, 25.0, point(100.0, 110.0), point(0.0, 1.0));
    }

    #[test]
    fn point_at_length_along_a_closed_path() {
        let mut path = Path::new();
        path.move_to(point(0.0, 0.0));
        path.line_to(point(10.0, 0.0));
        path.line_to(point(10.0, 10.0));
        path.line_to(point(0.0, 10.0));
        path.close();

        assert_point_along_path(&path, 15.0, point(10.0, 5.0), point(0.0, 1.0));
        assert_point_along_path(&path, 35.0, point(0.0, 5.0), point(0.0, -1.0));
    }

    #[test]
    fn point_at_length_of_a_path_without_length() {
        let mut path = Path::new();
        path.move_to(point(10.0, 20.0));
        assert_point_along_path(&path, 5.0, point(10.0, 20.0), point(0.0, 0.0));

        assert!(Path::new().point_at_length(0.0).is_none());
    }

    fn vertex(x: f32, y: f32, distance: f32, direction_x: f32, direction_y: f32) -> PathVertex {
        PathVertex {
            point: point(x, y),
            distance,
            direction: point(direction_x, direction_y),
        }
    }

    fn assert_vertices(path: &Path, expected_vertices: &[PathVertex]) {
        let vertices = path.vertices();
        assert_eq!(vertices.len(), expected_vertices.len());
        for (actual, expected) in vertices.iter().zip(expected_vertices) {
            for (actual, expected) in [
                (actual.point.x, expected.point.x),
                (actual.point.y, expected.point.y),
                (actual.distance, expected.distance),
                (actual.direction.x, expected.direction.x),
                (actual.direction.y, expected.direction.y),
            ] {
                assert_approximately(actual, expected, 0.0001);
            }
        }
    }

    #[test]
    fn vertices_of_subpaths_of_lines() {
        let mut path = Path::new();
        path.move_to(point(0.0, 0.0));
        path.line_to(point(10.0, 0.0));
        path.line_to(point(10.0, 10.0));
        path.close();
        path.move_to(point(50.0, 50.0));
        path.line_to(point(60.0, 50.0));

        let diagonal = 200.0_f32.sqrt();
        let inverse_sqrt_2 = 1.0 / 2.0_f32.sqrt();
        assert_vertices(
            &path,
            &[
                vertex(0.0, 0.0, 0.0, 1.0, 0.0),
                vertex(10.0, 0.0, 10.0, 1.0, 0.0),
                vertex(10.0, 0.0, 10.0, 0.0, 1.0),
                vertex(10.0, 10.0, 20.0, 0.0, 1.0),
                vertex(10.0, 10.0, 20.0, -inverse_sqrt_2, -inverse_sqrt_2),
                vertex(0.0, 0.0, 20.0 + diagonal, -inverse_sqrt_2, -inverse_sqrt_2),
                vertex(50.0, 50.0, 20.0 + diagonal, 1.0, 0.0),
                vertex(60.0, 50.0, 30.0 + diagonal, 1.0, 0.0),
            ],
        );
    }

    #[test]
    fn vertices_along_curves() {
        let mut path = Path::new();
        path.move_to(point(0.0, 0.0));
        path.quadratic_bezier_curve_to(point(50.0, 0.0), point(100.0, 0.0));
        assert_vertices(
            &path,
            &[vertex(0.0, 0.0, 0.0, 1.0, 0.0), vertex(100.0, 0.0, 100.0, 1.0, 0.0)],
        );

        let mut long_path = Path::new();
        long_path.move_to(point(0.0, 0.0));
        long_path.quadratic_bezier_curve_to(point(10_000_000.0, 0.0), point(20_000_000.0, 0.0));
        assert_vertices(
            &long_path,
            &[
                vertex(0.0, 0.0, 0.0, 1.0, 0.0),
                vertex(20_000_000.0, 0.0, 20_000_000.0, 1.0, 0.0),
            ],
        );

        let mut circle = Path::new();
        circle.move_to(point(10.0, 0.0));
        circle.arc_to(point(-10.0, 0.0), 10.0, false, true);
        circle.arc_to(point(10.0, 0.0), 10.0, false, true);
        let vertices = circle.vertices();
        assert!(vertices.len() > 40);
        for vertex in &vertices {
            assert_approximately(math::hypot(vertex.point.x, vertex.point.y), 10.0, 0.001);
            assert_approximately(math::hypot(vertex.direction.x, vertex.direction.y), 1.0, 0.001);
            assert_approximately(
                vertex.direction.x * vertex.point.x + vertex.direction.y * vertex.point.y,
                0.0,
                0.01,
            );
        }
        for pair in vertices.windows(2) {
            let midpoint = point(
                (pair[0].point.x + pair[1].point.x) / 2.0,
                (pair[0].point.y + pair[1].point.y) / 2.0,
            );
            assert!(math::hypot(midpoint.x, midpoint.y) > 10.0 - 0.5 / 16.0);
        }
        assert_approximately(vertices.first().unwrap().point.x, 10.0, 0.000_000_5);
        assert_approximately(vertices.last().unwrap().point.x, 10.0, 0.000_000_5);
        assert_approximately(vertices.last().unwrap().distance, 20.0 * std::f32::consts::PI, 0.05);
    }

    #[test]
    fn vertices_of_a_path_without_length() {
        let mut path = Path::new();
        path.move_to(point(10.0, 20.0));
        assert_vertices(&path, &[vertex(10.0, 20.0, 0.0, 0.0, 0.0)]);

        let mut zero_length_line = Path::new();
        zero_length_line.move_to(point(10.0, 20.0));
        zero_length_line.line_to(point(10.0, 20.0));
        assert_vertices(&zero_length_line, &[vertex(10.0, 20.0, 0.0, 0.0, 0.0)]);

        assert_vertices(&Path::new(), &[]);
    }

    #[test]
    fn glyph_outline_cache_collision_does_not_drop_cached_glyph() {
        let font_id = 42u64;
        let glyph_a = 1u16;
        let slot_a = glyph_outline_cache_slot(font_id, glyph_a);
        let glyph_b = (2u16..=u16::MAX)
            .find(|&candidate| glyph_outline_cache_slot(font_id, candidate) == slot_a)
            .expect("direct-mapped 1024-entry cache has a colliding glyph id");

        let make_outline = |glyph_id: u16, x: f32| CachedGlyphOutline {
            font_id,
            glyph_id,
            verbs: vec![PathVerb::Move],
            points: vec![point(x, 0.0)],
            conic_weights: Vec::new(),
        };

        let mut cache = vec![CachedGlyphOutline::default(); GLYPH_OUTLINE_CACHE_SIZE];
        cache[slot_a] = make_outline(glyph_a, 10.0);

        let glyphs = [
            crate::text_layout::DrawGlyph::new(10.0, u32::from(glyph_a), 1, true),
            crate::text_layout::DrawGlyph::new(10.0, u32::from(glyph_b), 1, true),
        ];

        let mut geometry = PathGeometry::new();
        let mut appended_xs = Vec::new();
        let mut fetched_ids = Vec::new();
        append_glyph_outlines_with_cache(
            &mut cache,
            &mut geometry,
            font_id,
            &glyphs,
            |missing, fetched| {
                for g in missing {
                    fetched_ids.push(g.glyph_id);
                    fetched.push(make_outline(g.glyph_id, 20.0));
                }
            },
            |_geometry, index, _verbs, points, _weights| {
                appended_xs.push((index, points[0].x));
            },
        );

        assert_eq!(fetched_ids, vec![glyph_b]);
        assert_eq!(appended_xs, vec![(0, 10.0), (1, 20.0)]);
        assert_eq!(cache[slot_a].glyph_id, glyph_b);
    }
}
