/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The byte form in which paths cross process boundaries and live in display lists.
//!
//! All values are little-endian: a u8 version, a u8 fill rule, two zero bytes, then the u32 counts of
//! verbs, points and conic weights, then the verbs as u8, zeros up to a multiple of four bytes, the
//! points as pairs of f32, and the conic weights as f32. A path without geometry is written as an
//! empty path.

use super::geometry::{PathGeometry, PathVerb};
use crate::geometry::FloatPoint;
use crate::paint_enums::WindingRule;

const VERSION: u8 = 1;
const HEADER_SIZE: usize = 16;

fn padded_to_four(size: usize) -> usize {
    size.div_ceil(4) * 4
}

pub fn encode(geometry: &PathGeometry) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode_into(geometry, &mut bytes);
    bytes
}

pub fn encode_into(geometry: &PathGeometry, bytes: &mut Vec<u8>) {
    bytes.clear();
    encode_append_to(geometry, bytes);
}

pub fn encode_append_to(geometry: &PathGeometry, bytes: &mut Vec<u8>) {
    let start = bytes.len();
    let empty = PathGeometry::new();
    let geometry = if geometry.has_geometry() { geometry } else { &empty };
    let verbs = geometry.verbs();
    let points = geometry.points();
    let conic_weights = geometry.conic_weights();

    let padded_verbs = padded_to_four(verbs.len());
    let total_len = HEADER_SIZE + padded_verbs + points.len() * 8 + conic_weights.len() * 4;
    bytes.reserve(total_len);
    bytes.extend_from_slice(&[VERSION, geometry.fill_rule() as u8, 0, 0]);
    for count in [verbs.len(), points.len(), conic_weights.len()] {
        bytes.extend_from_slice(&(count as u32).to_le_bytes());
    }
    #[cfg(target_endian = "little")]
    {
        const _: () = assert!(size_of::<PathVerb>() == 1);
        const _: () = assert!(size_of::<FloatPoint>() == 8);
        const _: () = assert!(core::mem::offset_of!(FloatPoint, x) == 0 && core::mem::offset_of!(FloatPoint, y) == 4);
        // SAFETY: `PathVerb` is `#[repr(u8)]`, `FloatPoint` is `#[repr(C)]` `{ x: f32, y: f32 }`,
        // and on little-endian targets their in-memory layout matches the wire format exactly.
        unsafe {
            bytes.extend_from_slice(ak::slice_from_raw_parts(verbs.as_ptr().cast::<u8>(), verbs.len()));
            bytes.resize(start + HEADER_SIZE + padded_verbs, 0);
            bytes.extend_from_slice(ak::slice_from_raw_parts(points.as_ptr().cast::<u8>(), points.len() * 8));
            bytes.extend_from_slice(ak::slice_from_raw_parts(
                conic_weights.as_ptr().cast::<u8>(),
                conic_weights.len() * 4,
            ));
        }
    }
    #[cfg(not(target_endian = "little"))]
    {
        bytes.extend(verbs.iter().map(|verb| *verb as u8));
        bytes.resize(start + HEADER_SIZE + padded_verbs, 0);
        for point in points {
            bytes.extend_from_slice(&point.x.to_le_bytes());
            bytes.extend_from_slice(&point.y.to_le_bytes());
        }
        for weight in conic_weights {
            bytes.extend_from_slice(&weight.to_le_bytes());
        }
    }
}

/// Encodes the outline of a rectangle directly onto `bytes` in the exact wire format of a `Path`
/// built with `move_to(x, y)`, four `line_to` calls clockwise around `(width, height)`, and an
/// optional `close()`.
pub fn encode_rect_append_to(x: f32, y: f32, width: f32, height: f32, closed: bool, bytes: &mut Vec<u8>) {
    let x2 = x + width;
    let y2 = y + height;
    if !(x.is_finite() && y.is_finite() && x2.is_finite() && y2.is_finite()) {
        bytes.extend_from_slice(&[VERSION, WindingRule::Nonzero as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        return;
    }

    let (verb_count, point_count): (u32, u32) = if closed { (7, 6) } else { (5, 5) };
    let verbs_padded: [u8; 8] = if closed {
        [
            PathVerb::Move as u8,
            PathVerb::Line as u8,
            PathVerb::Line as u8,
            PathVerb::Line as u8,
            PathVerb::Line as u8,
            PathVerb::Close as u8,
            PathVerb::Move as u8,
            0,
        ]
    } else {
        [
            PathVerb::Move as u8,
            PathVerb::Line as u8,
            PathVerb::Line as u8,
            PathVerb::Line as u8,
            PathVerb::Line as u8,
            0,
            0,
            0,
        ]
    };
    let all_points = [
        FloatPoint { x, y },
        FloatPoint { x: x2, y },
        FloatPoint { x: x2, y: y2 },
        FloatPoint { x, y: y2 },
        FloatPoint { x, y },
        FloatPoint { x, y },
    ];
    let points = &all_points[..point_count as usize];

    bytes.reserve(HEADER_SIZE + verbs_padded.len() + points.len() * 8);
    bytes.extend_from_slice(&[VERSION, WindingRule::Nonzero as u8, 0, 0]);
    bytes.extend_from_slice(&verb_count.to_le_bytes());
    bytes.extend_from_slice(&point_count.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&verbs_padded);
    #[cfg(target_endian = "little")]
    {
        const _: () = assert!(size_of::<FloatPoint>() == 8);
        const _: () = assert!(core::mem::offset_of!(FloatPoint, x) == 0 && core::mem::offset_of!(FloatPoint, y) == 4);
        // SAFETY: `FloatPoint` is `#[repr(C)]` `{ x: f32, y: f32 }`, matching little-endian wire format.
        unsafe {
            bytes.extend_from_slice(ak::slice_from_raw_parts(points.as_ptr().cast::<u8>(), points.len() * 8));
        }
    }
    #[cfg(not(target_endian = "little"))]
    {
        for point in points {
            bytes.extend_from_slice(&point.x.to_le_bytes());
            bytes.extend_from_slice(&point.y.to_le_bytes());
        }
    }
}

/// The path that the bytes describe, or None if they do not describe one: if the sizes do not match
/// the counts, if a contour does not start with a move, if the verbs need other numbers of points or
/// weights than there are, or if a point or weight is not finite or a weight is not positive.
pub fn decode(bytes: &[u8]) -> Option<PathGeometry> {
    let header = bytes.get(..HEADER_SIZE)?;
    if header[0] != VERSION || header[2] != 0 || header[3] != 0 {
        return None;
    }
    let fill_rule = match header[1] {
        0 => WindingRule::Nonzero,
        1 => WindingRule::EvenOdd,
        _ => return None,
    };
    let read_count =
        |offset: usize| u32::from_le_bytes(header[offset..offset + 4].try_into().expect("four bytes")) as usize;
    let (verb_count, point_count, conic_weight_count) = (read_count(4), read_count(8), read_count(12));

    let verbs_end = HEADER_SIZE.checked_add(padded_to_four(verb_count))?;
    let points_end = verbs_end.checked_add(point_count.checked_mul(8)?)?;
    let weights_end = points_end.checked_add(conic_weight_count.checked_mul(4)?)?;
    if bytes.len() != weights_end {
        return None;
    }

    let mut verbs = Vec::with_capacity(verb_count);
    let mut needed_points = 0usize;
    let mut needed_conic_weights = 0usize;
    for &byte in &bytes[HEADER_SIZE..HEADER_SIZE + verb_count] {
        let verb = PathVerb::from_u8(byte)?;
        let starts_contour = matches!(verbs.last(), None | Some(PathVerb::Close));
        if starts_contour && verb != PathVerb::Move {
            return None;
        }
        needed_points += verb.stored_point_count();
        if verb == PathVerb::Conic {
            needed_conic_weights += 1;
        }
        verbs.push(verb);
    }
    if bytes[HEADER_SIZE + verb_count..verbs_end].iter().any(|byte| *byte != 0) {
        return None;
    }
    if needed_points != point_count || needed_conic_weights != conic_weight_count {
        return None;
    }

    let read_f32 = |offset: usize| f32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"));
    let points: Vec<FloatPoint> = (0..point_count)
        .map(|index| {
            let offset = verbs_end + index * 8;
            FloatPoint {
                x: read_f32(offset),
                y: read_f32(offset + 4),
            }
        })
        .collect();
    let conic_weights: Vec<f32> = (0..conic_weight_count)
        .map(|index| read_f32(points_end + index * 4))
        .collect();
    if !points.iter().all(|point| point.x.is_finite() && point.y.is_finite())
        || !conic_weights.iter().all(|weight| weight.is_finite() && *weight > 0.0)
    {
        return None;
    }
    Some(PathGeometry::from_checked_contours(
        verbs,
        points,
        conic_weights,
        fill_rule,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_rect_append_to_matches_path_geometry() {
        for closed in [false, true] {
            let mut geom = PathGeometry::new();
            geom.move_to(FloatPoint { x: 1.5, y: 2.5 });
            geom.line_to(FloatPoint { x: 11.5, y: 2.5 });
            geom.line_to(FloatPoint { x: 11.5, y: 22.5 });
            geom.line_to(FloatPoint { x: 1.5, y: 22.5 });
            geom.line_to(FloatPoint { x: 1.5, y: 2.5 });
            if closed {
                geom.close();
            }

            let mut expected = vec![0xAA, 0xBB, 0xCC, 0xDD];
            encode_append_to(&geom, &mut expected);

            let mut actual = vec![0xAA, 0xBB, 0xCC, 0xDD];
            encode_rect_append_to(1.5, 2.5, 10.0, 20.0, closed, &mut actual);
            assert_eq!(actual, expected);
            assert!(decode(&actual[4..]).is_some());
        }

        let mut empty_expected = Vec::new();
        let mut bad_geom = PathGeometry::new();
        bad_geom.move_to(FloatPoint { x: f32::NAN, y: 0.0 });
        encode_append_to(&bad_geom, &mut empty_expected);

        let mut bad_actual = Vec::new();
        encode_rect_append_to(f32::NAN, 0.0, 10.0, 20.0, false, &mut bad_actual);
        assert_eq!(bad_actual, empty_expected);
    }

    #[test]
    fn append_full_ellipse_matches_two_half_arcs() {
        let cases = [
            (FloatPoint { x: 10.0, y: 20.0 }, (5.0f32, 5.0f32), 0.0f32, 0.0f32, false),
            (FloatPoint { x: 10.0, y: 20.0 }, (5.0, 5.0), 0.0, 0.0, true),
            (FloatPoint { x: -3.0, y: 4.5 }, (12.0, 7.0), 0.0, 0.0, false),
            (FloatPoint { x: 1.0, y: 2.0 }, (8.0, 8.0), 0.0, 1.0, false),
            (FloatPoint { x: 1.0, y: 2.0 }, (8.0, 4.0), 0.5, 0.75, false),
            (FloatPoint { x: 1.0, y: 2.0 }, (8.0, 4.0), 0.5, 0.75, true),
        ];

        for (center, radii, rotation, start_angle, ccw) in cases {
            let mut fast = PathGeometry::new();
            fast.append_full_ellipse(center, radii, rotation, start_angle, ccw);

            // Reference: resolve start & opposite points, emit two 180-degree elliptical arcs.
            let (sin_r, cos_r) = ak::math::sincos(f64::from(rotation));
            let resolve = |angle: f32| {
                let tan = ak::math::tan(angle);
                let ab = radii.0 * radii.1;
                let sqrt = (radii.1 * radii.1 + radii.0 * radii.0 * tan * tan).sqrt();
                let sn = if ak::math::cos(angle) >= 0.0 { 1.0 } else { -1.0 };
                let rx = ab / sqrt * sn;
                let ry = ab * tan / sqrt * sn;
                FloatPoint {
                    x: (f64::from(rx) * cos_r - f64::from(ry) * sin_r) as f32 + center.x,
                    y: (f64::from(rx) * sin_r + f64::from(ry) * cos_r) as f32 + center.y,
                }
            };
            let start_pt = resolve(start_angle);
            let half_turn = if ccw { -core::f32::consts::PI } else { core::f32::consts::PI };
            let opp_pt = resolve(start_angle + half_turn);

            let mut reference = PathGeometry::new();
            reference.move_to(start_pt);
            reference.elliptical_arc_to(opp_pt, radii, rotation, false, !ccw);
            reference.elliptical_arc_to(start_pt, radii, rotation, false, !ccw);

            assert_eq!(fast.verbs(), reference.verbs());
            assert_eq!(fast.points().len(), reference.points().len());
            assert_eq!(fast.conic_weights().len(), reference.conic_weights().len());
            for (a, b) in fast.points().iter().zip(reference.points().iter()) {
                assert!((a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4, "{a:?} vs {b:?}");
            }
            for (a, b) in fast.conic_weights().iter().zip(reference.conic_weights().iter()) {
                assert!((a - b).abs() < 1e-5, "{a} vs {b}");
            }
        }
    }
}
