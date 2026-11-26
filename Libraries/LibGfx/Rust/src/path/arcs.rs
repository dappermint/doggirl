/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! Elliptical arcs as conics.
//!
//! https://www.w3.org/TR/SVG2/implnote.html#ArcConversionEndpointToCenter
//!
//! The arc goes from the endpoint parameterization of the SVG path command to a center, radii and
//! a range of angles on the ellipse. A conic is exactly an arc of an ellipse: on the unit circle, the
//! arc from angle a to angle a + d has its ends as the end points, the point where the tangents at
//! the ends meet, (cos(a + d / 2), sin(a + d / 2)) / cos(d / 2), as the control point, and cos(d / 2)
//! as the weight. Mapping the three points onto the ellipse keeps the weight.

use super::geometry::PathGeometry;
use crate::geometry::FloatPoint;
use ak::math;
use std::f64::consts::PI;

/// The largest angle of the ellipse that one conic covers.
const LARGEST_ANGLE_OF_ONE_CONIC: f64 = 2.0 * PI / 3.0;

pub(super) fn append_elliptical_arc(
    geometry: &mut PathGeometry,
    end: FloatPoint,
    radii: (f32, f32),
    x_axis_rotation: f32,
    large_arc: bool,
    sweep: bool,
) {
    if ![end.x, end.y, radii.0, radii.1, x_axis_rotation]
        .iter()
        .all(|value| value.is_finite())
    {
        return;
    }

    let start = geometry.last_point();
    let (mut radius_x, mut radius_y) = (f64::from(radii.0.abs()), f64::from(radii.1.abs()));
    if start == end || radius_x == 0.0 || radius_y == 0.0 {
        geometry.contour_line_to(end);
        return;
    }

    let (sin_rotation, cos_rotation) = if x_axis_rotation == 0.0 {
        (0.0, 1.0)
    } else {
        math::sincos(f64::from(x_axis_rotation))
    };
    let (start_x, start_y) = (f64::from(start.x), f64::from(start.y));
    let (end_x, end_y) = (f64::from(end.x), f64::from(end.y));

    // The start point in a frame centered between the end points, with the axes of the ellipse.
    let half_difference_x = (start_x - end_x) / 2.0;
    let half_difference_y = (start_y - end_y) / 2.0;
    let start_in_frame_x = cos_rotation * half_difference_x + sin_rotation * half_difference_y;
    let start_in_frame_y = -sin_rotation * half_difference_x + cos_rotation * half_difference_y;

    // Radii too small for the ellipse to reach both points grow until it just does.
    let radii_scale = (start_in_frame_x * start_in_frame_x) / (radius_x * radius_x)
        + (start_in_frame_y * start_in_frame_y) / (radius_y * radius_y);
    if radii_scale > 1.0 {
        let scale = radii_scale.sqrt();
        radius_x *= scale;
        radius_y *= scale;
    }

    let rx_squared = radius_x * radius_x;
    let ry_squared = radius_y * radius_y;
    let x_squared = start_in_frame_x * start_in_frame_x;
    let y_squared = start_in_frame_y * start_in_frame_y;
    let numerator = rx_squared * ry_squared - rx_squared * y_squared - ry_squared * x_squared;
    let denominator = rx_squared * y_squared + ry_squared * x_squared;
    let mut center_factor = (numerator / denominator).max(0.0).sqrt();
    if large_arc == sweep {
        center_factor = -center_factor;
    }
    let center_in_frame_x = center_factor * radius_x * start_in_frame_y / radius_y;
    let center_in_frame_y = -center_factor * radius_y * start_in_frame_x / radius_x;
    let center_x = cos_rotation * center_in_frame_x - sin_rotation * center_in_frame_y + (start_x + end_x) / 2.0;
    let center_y = sin_rotation * center_in_frame_x + cos_rotation * center_in_frame_y + (start_y + end_y) / 2.0;

    // The start and end points on the unit circle that the ellipse is a mapping of.
    let mut start_on_circle = (
        (start_in_frame_x - center_in_frame_x) / radius_x,
        (start_in_frame_y - center_in_frame_y) / radius_y,
    );
    let end_on_circle = (
        (-start_in_frame_x - center_in_frame_x) / radius_x,
        (-start_in_frame_y - center_in_frame_y) / radius_y,
    );
    let cross = start_on_circle.0 * end_on_circle.1 - start_on_circle.1 * end_on_circle.0;
    let dot = start_on_circle.0 * end_on_circle.0 + start_on_circle.1 * end_on_circle.1;
    let mut sweep_angle = if cross == 0.0 && dot < 0.0 {
        PI
    } else {
        math::atan2(cross, dot)
    };
    if !sweep && sweep_angle > 0.0 {
        sweep_angle -= 2.0 * PI;
    } else if sweep && sweep_angle < 0.0 {
        sweep_angle += 2.0 * PI;
    }
    if !(start_on_circle.0.is_finite()
        && start_on_circle.1.is_finite()
        && sweep_angle.is_finite()
        && center_x.is_finite()
        && center_y.is_finite())
    {
        geometry.contour_line_to(end);
        return;
    }

    let start_norm_squared = start_on_circle.0 * start_on_circle.0 + start_on_circle.1 * start_on_circle.1;
    if (start_norm_squared - 1.0).abs() > 1e-12 && start_norm_squared > 0.0 {
        let inverse_norm = start_norm_squared.sqrt().recip();
        start_on_circle.0 *= inverse_norm;
        start_on_circle.1 *= inverse_norm;
    }

    let to_ellipse = |unit_x: f64, unit_y: f64| {
        let x = radius_x * snap_to_axis(unit_x);
        let y = radius_y * snap_to_axis(unit_y);
        FloatPoint {
            x: (center_x + cos_rotation * x - sin_rotation * y) as f32,
            y: (center_y + sin_rotation * x + cos_rotation * y) as f32,
        }
    };

    let conic_count = ((sweep_angle.abs() / LARGEST_ANGLE_OF_ONE_CONIC) - 1e-9)
        .ceil()
        .max(1.0) as usize;
    let angle_per_conic = sweep_angle / conic_count as f64;
    let half_angle = angle_per_conic / 2.0;
    let (step_sin, step_cos) = if half_angle.abs() == std::f64::consts::FRAC_PI_4 {
        (
            std::f64::consts::FRAC_1_SQRT_2.copysign(half_angle),
            std::f64::consts::FRAC_1_SQRT_2,
        )
    } else {
        math::sincos(half_angle)
    };
    let weight = step_cos as f32;
    let inverse_step_cos = 1.0 / step_cos;
    let (mut unit_x, mut unit_y) = start_on_circle;
    for index in 0..conic_count {
        let mid_x = unit_x * step_cos - unit_y * step_sin;
        let mid_y = unit_x * step_sin + unit_y * step_cos;
        let control = to_ellipse(mid_x * inverse_step_cos, mid_y * inverse_step_cos);
        let conic_end = if index + 1 == conic_count {
            end
        } else {
            let next_x = mid_x * step_cos - mid_y * step_sin;
            let next_y = mid_x * step_sin + mid_y * step_cos;
            unit_x = next_x;
            unit_y = next_y;
            to_ellipse(next_x, next_y)
        };
        geometry.contour_conic_to(control, conic_end, weight);
    }
}

/// A coordinate on the unit circle that is 0, 1 or -1 but for rounding, as at the ends of quarter
/// circles, is made exact, so that arcs along the axes have exact points.
fn snap_to_axis(value: f64) -> f64 {
    const ROUNDING_TOLERANCE: f64 = 1e-12;
    for exact in [0.0, 1.0, -1.0] {
        if (value - exact).abs() < ROUNDING_TOLERANCE {
            return exact;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::geometry::PathVerb;

    fn reference_conics(
        start: FloatPoint,
        end: FloatPoint,
        radii: (f32, f32),
        x_axis_rotation: f32,
        large_arc: bool,
        sweep: bool,
    ) -> Vec<(FloatPoint, FloatPoint, f32)> {
        let (mut radius_x, mut radius_y) = (f64::from(radii.0.abs()), f64::from(radii.1.abs()));
        let (sin_rotation, cos_rotation) = math::sincos(f64::from(x_axis_rotation));
        let half_dx = (f64::from(start.x) - f64::from(end.x)) / 2.0;
        let half_dy = (f64::from(start.y) - f64::from(end.y)) / 2.0;
        let sx = cos_rotation * half_dx + sin_rotation * half_dy;
        let sy = -sin_rotation * half_dx + cos_rotation * half_dy;
        let scale_sq = (sx * sx) / (radius_x * radius_x) + (sy * sy) / (radius_y * radius_y);
        if scale_sq > 1.0 {
            let s = scale_sq.sqrt();
            radius_x *= s;
            radius_y *= s;
        }
        let rx2 = radius_x * radius_x;
        let ry2 = radius_y * radius_y;
        let num = rx2 * ry2 - rx2 * sy * sy - ry2 * sx * sx;
        let den = rx2 * sy * sy + ry2 * sx * sx;
        let mut factor = (num / den).max(0.0).sqrt();
        if large_arc == sweep {
            factor = -factor;
        }
        let cx_f = factor * radius_x * sy / radius_y;
        let cy_f = -factor * radius_y * sx / radius_x;
        let cx = cos_rotation * cx_f - sin_rotation * cy_f + (f64::from(start.x) + f64::from(end.x)) / 2.0;
        let cy = sin_rotation * cx_f + cos_rotation * cy_f + (f64::from(start.y) + f64::from(end.y)) / 2.0;
        let s_u = ((sx - cx_f) / radius_x, (sy - cy_f) / radius_y);
        let e_u = ((-sx - cx_f) / radius_x, (-sy - cy_f) / radius_y);
        let start_angle = math::atan2(s_u.1, s_u.0);
        let mut sweep_angle = math::atan2(s_u.0 * e_u.1 - s_u.1 * e_u.0, s_u.0 * e_u.0 + s_u.1 * e_u.1);
        if !sweep && sweep_angle > 0.0 {
            sweep_angle -= 2.0 * PI;
        } else if sweep && sweep_angle < 0.0 {
            sweep_angle += 2.0 * PI;
        }
        let to_ellipse = |ux: f64, uy: f64| {
            let x = radius_x * snap_to_axis(ux);
            let y = radius_y * snap_to_axis(uy);
            FloatPoint {
                x: (cx + cos_rotation * x - sin_rotation * y) as f32,
                y: (cy + sin_rotation * x + cos_rotation * y) as f32,
            }
        };
        let count = ((sweep_angle.abs() / LARGEST_ANGLE_OF_ONE_CONIC) - 1e-9)
            .ceil()
            .max(1.0) as usize;
        let step = sweep_angle / count as f64;
        let half_cos = math::cos(step / 2.0);
        let mut out = Vec::new();
        for i in 0..count {
            let from = start_angle + step * i as f64;
            let mid = from + step / 2.0;
            let to = from + step;
            let ctrl = to_ellipse(math::cos(mid) / half_cos, math::sin(mid) / half_cos);
            let ep = if i + 1 == count {
                end
            } else {
                to_ellipse(math::cos(to), math::sin(to))
            };
            out.push((ctrl, ep, half_cos as f32));
        }
        out
    }

    #[test]
    fn elliptical_arc_recurrence_matches_trigonometric_reference() {
        let cases = [
            (
                FloatPoint { x: 10.0, y: 0.0 },
                FloatPoint { x: -10.0, y: 0.0 },
                (10.0, 10.0),
                0.0,
                false,
                true,
            ),
            (
                FloatPoint { x: -10.0, y: 0.0 },
                FloatPoint { x: 10.0, y: 0.0 },
                (10.0, 10.0),
                0.0,
                false,
                true,
            ),
            (
                FloatPoint { x: 10.0, y: 0.0 },
                FloatPoint { x: -10.0, y: 0.0 },
                (10.0, 10.0),
                0.0,
                false,
                false,
            ),
            (
                FloatPoint { x: 20.0, y: 5.0 },
                FloatPoint { x: 5.0, y: 30.0 },
                (15.0, 25.0),
                0.6,
                true,
                true,
            ),
            (
                FloatPoint { x: 20.0, y: 5.0 },
                FloatPoint { x: 5.0, y: 30.0 },
                (15.0, 25.0),
                -0.4,
                false,
                false,
            ),
            (
                FloatPoint { x: 0.0, y: 0.0 },
                FloatPoint { x: 12.0, y: 0.0 },
                (3.0, 4.0),
                0.25,
                true,
                false,
            ),
        ];
        for (start, end, radii, rot, large_arc, sweep) in cases {
            let mut geometry = PathGeometry::default();
            geometry.move_to(start);
            append_elliptical_arc(&mut geometry, end, radii, rot, large_arc, sweep);
            let expected = reference_conics(start, end, radii, rot, large_arc, sweep);
            let conic_verbs: usize = geometry.verbs().iter().filter(|&&v| v == PathVerb::Conic).count();
            assert_eq!(conic_verbs, expected.len());
            assert_eq!(geometry.conic_weights().len(), expected.len());
            for (idx, &(exp_ctrl, exp_end, exp_w)) in expected.iter().enumerate() {
                let ctrl = geometry.points()[1 + idx * 2];
                let ep = geometry.points()[2 + idx * 2];
                let w = geometry.conic_weights()[idx];
                assert!((ctrl.x - exp_ctrl.x).abs() < 1e-4 && (ctrl.y - exp_ctrl.y).abs() < 1e-4);
                assert!((ep.x - exp_end.x).abs() < 1e-4 && (ep.y - exp_end.y).abs() < 1e-4);
                assert!((w - exp_w).abs() < 1e-5);
            }
        }
    }
}
