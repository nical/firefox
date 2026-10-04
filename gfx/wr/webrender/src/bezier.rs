/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub type Point<U> = crate::euclid::Point2D<f32, U>;
type Vector<U> = crate::euclid::Vector2D<f32, U>;

/// Approximate the curve with a sequence of line segments.
pub fn flatten_cubic<U>(
    from: Point<U>,
    ctrl1: Point<U>,
    ctrl2: Point<U>,
    to: Point<U>,
    tolerance: f32,
    callback: &mut impl FnMut(Point<U>)
) {
    let n = num_cubic_segments_wang(from, ctrl1, ctrl2, to, tolerance);

    let step = 1.0 / n;
    let mut t = step;

    let curve = CubicBezierPolynomial::new(from, ctrl1, ctrl2, to);

    let n = n as u32 - 1;
    for _ in 0..n {
        let next = curve.sample(t);
        callback(next);
        t += step;
    }

    callback(to);
}

/// Approximate the curve with a sequence of line segments.
pub fn flatten_quadratic<U>(
    from: Point<U>,
    ctrl: Point<U>,
    to: Point<U>,
    tolerance: f32,
    callback: &mut impl FnMut(Point<U>)
) {
    let n = num_quadratic_segments_wang(from, ctrl, to, tolerance);

    let step = 1.0 / n;
    let mut t = step;

    let curve = QuadraticBezierPolynomial::new(from, ctrl, to);

    let n = n as u32 - 1;
    for _ in 0..n {
        let next = curve.sample(t);
        callback(next);
        t += step;
    }

    callback(to);
}

/// Use Wang's formula to compute the number of line segments required
/// to build a flattened approximation of the curve with segments placed
/// at regular `t` intervals.
fn num_cubic_segments_wang<U>(
    from: Point<U>,
    ctrl1: Point<U>,
    ctrl2: Point<U>,
    to: Point<U>,
    tolerance: f32
) -> f32 {
    let from = from.to_vector();
    let ctrl1 = ctrl1.to_vector();
    let ctrl2 = ctrl2.to_vector();
    let to = to.to_vector();
    let v1 = (from - ctrl1 * 2.0 + ctrl2) * 6.0;
    let v2 = (ctrl1 - ctrl2 * 2.0 + to) * 6.0;
    let l = v1.dot(v1).max(v2.dot(v2));
    let d = 1.0 / (8.0 * tolerance);
    let err4 = l * d * d;

    // Avoid two square roots using a lookup table that contains
    // i^4 for  i in 1..25.
    const N: usize = 24;
    const LUT: [f32; N] = [
        1.0, 16.0, 81.0, 256.0, 625.0, 1296.0, 2401.0, 4096.0, 6561.0,
        10000.0, 14641.0, 20736.0, 28561.0, 38416.0, 50625.0, 65536.0,
        83521.0, 104976.0, 130321.0, 160000.0, 194481.0, 234256.0,
        279841.0, 331776.0
    ];

    // If the value we are looking for is within the LUT, take the fast path
    if err4 <= 331776.0 {
        #[allow(clippy::needless_range_loop)]
        for i in 0..N {
            if err4 <= LUT[i] {
                return i as f32 + 1.0;
            }
        }
    }

    // Otherwise fall back to computing via two square roots.
    err4.sqrt().sqrt().ceil().max(1.0)
}

/// Use Wang's formula to compute the number of line segments required
/// to build a flattened approximation of the curve with segments placed
/// at regular `t` intervals.
fn num_quadratic_segments_wang<U>(from: Point<U>, ctrl: Point<U>, to: Point<U>, tolerance: f32) -> f32 {
    let from = from.to_vector();
    let ctrl = ctrl.to_vector();
    let to = to.to_vector();
    let l = (from - ctrl * 2.0 + to) * 2.0;
    let d = 1.0 / (8.0 * tolerance);
    let err4 = l.dot(l) * d * d;

    // Avoid two square roots using a lookup table that contains
    // i^4 for  i in 1..25.
    const N: usize = 24;
    const LUT: [f32; N] = [
        1.0, 16.0, 81.0, 256.0, 625.0, 1296.0, 2401.0, 4096.0, 6561.0,
        10000.0, 14641.0, 20736.0, 28561.0, 38416.0, 50625.0, 65536.0,
        83521.0, 104976.0, 130321.0, 160000.0, 194481.0, 234256.0,
        279841.0, 331776.0
    ];

    // If the value we are looking for is within the LUT, take the fast path
    if err4 <= 331776.0 {
        #[allow(clippy::needless_range_loop)]
        for i in 0..N {
            if err4 <= LUT[i] {
                return i as f32 + 1.0;
            }
        }
    }

    // Otherwise fall back to computing via two square roots.
    err4.sqrt().sqrt().ceil().max(1.0)
}


/// The polynomial form of a cubic bézier segment.
///
/// The `sample` implementation uses Horner's method and is faster than the
/// classic way of sampling cubic bézier curves.
struct CubicBezierPolynomial<U> {
    a0: Vector<U>,
    a1: Vector<U>,
    a2: Vector<U>,
    a3: Vector<U>,
}

impl<U> CubicBezierPolynomial<U> {
    #[inline(always)]
    fn new(from: Point<U>, ctrl1: Point<U>, ctrl2: Point<U>, to: Point<U>) -> Self {
        CubicBezierPolynomial {
            a0: from.to_vector(),
            a1: (ctrl1 - from) * 3.0,
            a2: from * 3.0 - ctrl1 * 6.0 + ctrl2.to_vector() * 3.0,
            a3: to - from + (ctrl1 - ctrl2) * 3.0
        }
    }

    #[inline(always)]
    fn sample(&self, t: f32) -> Point<U> {
        // Horner's method.
        let mut v = self.a0;
        let mut t2 = t;
        v += self.a1 * t2;
        t2 *= t;
        v += self.a2 * t2;
        t2 *= t;
        v += self.a3 * t2;

        v.to_point()
    }
}

struct QuadraticBezierPolynomial<U> {
    a0: Vector<U>,
    a1: Vector<U>,
    a2: Vector<U>,
}

impl<U> QuadraticBezierPolynomial<U> {
    #[inline(always)]
    fn new(from: Point<U>, ctrl: Point<U>, to: Point<U>) -> QuadraticBezierPolynomial<U> {
        let from = from.to_vector();
        let ctrl = ctrl.to_vector();
        let to = to.to_vector();
        QuadraticBezierPolynomial {
            a0: from,
            a1: (ctrl - from) * 2.0,
            a2: from + to - ctrl * 2.0,
        }
    }

    #[inline(always)]
    fn sample(&self, t: f32) -> Point<U> {
        // Horner's method.
        let mut v = self.a0;
        let mut t2 = t;
        v += self.a1 * t2;
        t2 *= t;
        v += self.a2 * t2;

        v.to_point()
    }
}

#[cfg(test)]
fn max_flattening_error(points: &[Point<()>], sample: impl Fn(f32) -> Point<()>) -> f32 {
    // Distance from samples of the curve to the closest segment.
    let mut max_error: f32 = 0.0;
    for i in 0..=1000 {
        let p = sample(i as f32 / 1000.0);
        let mut error = f32::MAX;
        for segment in points.windows(2) {
            let (a, b) = (segment[0], segment[1]);
            let ab = b - a;
            let t = ((p - a).dot(ab) / ab.square_length()).max(0.0).min(1.0);
            error = error.min((a + ab * t - p).length());
        }
        max_error = max_error.max(error);
    }
    max_error
}

#[test]
fn flattening_tolerance() {
    use euclid::point2;

    // Curves that need fewer and more segments than the lookup table covers.
    for &scale in &[1.0, 10.0, 100.0] {
        let from = point2(0.0, 0.0);
        let ctrl1 = point2(100.0 * scale, 50.0 * scale);
        let ctrl2 = point2(70.0 * scale, 200.0 * scale);
        let to = point2(-60.0 * scale, 190.0 * scale);
        let mut points = vec![from];
        flatten_cubic(from, ctrl1, ctrl2, to, 0.25, &mut |p| points.push(p));
        let cubic = CubicBezierPolynomial::new(from, ctrl1, ctrl2, to);
        assert!(max_flattening_error(&points, |t| cubic.sample(t)) <= 0.25);

        let mut points = vec![from];
        flatten_quadratic(from, ctrl1, to, 0.25, &mut |p| points.push(p));
        let quadratic = QuadraticBezierPolynomial::new(from, ctrl1, to);
        assert!(max_flattening_error(&points, |t| quadratic.sample(t)) <= 0.25);
    }
}
