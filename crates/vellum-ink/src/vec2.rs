//! A private 2D vector, used where the geometry reads better in vector form.
//!
//! Deliberately not part of the public API. Ink geometry is expressed in
//! [`StrokePoint`](crate::StrokePoint)s, which carry pressure; a bare vector has no
//! business escaping the crate and being mistaken for one. The Catmull-Rom tangent
//! formula and the round-cone distance function are both unreadable written out
//! componentwise, which is the only reason this exists.

use core::ops::{Add, Mul, Sub};

use crate::StrokePoint;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Vec2 {
    pub x: f64,
    pub y: f64,
}

pub(crate) const fn v(x: f64, y: f64) -> Vec2 {
    Vec2 { x, y }
}

impl Vec2 {
    pub(crate) fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// Squared length. Preferred wherever a comparison or a ratio is all that is
    /// needed, because it avoids a square root and, more importantly, cannot lose
    /// precision on the very short segments a slow pen stroke is full of.
    pub(crate) fn length_squared(self) -> f64 {
        self.dot(self)
    }

    pub(crate) fn length(self) -> f64 {
        self.x.hypot(self.y)
    }

    /// Signed turn from `self` to `other`, in radians, in `-π..=π`.
    ///
    /// `atan2(cross, dot)` rather than `acos(dot / (|a||b|))`: `acos` loses all
    /// precision near 0 and π, which is exactly where corner detection has to make
    /// its decision.
    pub(crate) fn angle_to(self, other: Self) -> f64 {
        let cross = self.x * other.y - self.y * other.x;
        cross.atan2(self.dot(other))
    }

    pub(crate) fn from_point(p: StrokePoint) -> Self {
        Self { x: p.x, y: p.y }
    }

    /// Rebuilds a stroke point at this position, carrying `pressure` across.
    pub(crate) fn to_point(self, pressure: f64) -> StrokePoint {
        StrokePoint::with_pressure(self.x, self.y, pressure)
    }
}

impl Add for Vec2 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        v(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        v(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Mul<f64> for Vec2 {
    type Output = Self;
    fn mul(self, rhs: f64) -> Self {
        v(self.x * rhs, self.y * rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_is_componentwise() {
        assert_eq!(v(1.0, 2.0) + v(3.0, 4.0), v(4.0, 6.0));
        assert_eq!(v(1.0, 2.0) - v(3.0, 4.0), v(-2.0, -2.0));
        assert_eq!(v(1.0, 2.0) * 3.0, v(3.0, 6.0));
        assert_eq!(v(3.0, 4.0).length(), 5.0);
        assert_eq!(v(3.0, 4.0).length_squared(), 25.0);
        assert_eq!(v(1.0, 2.0).dot(v(3.0, 4.0)), 11.0);
    }

    #[test]
    fn angle_is_signed_and_covers_the_full_turn() {
        let east = v(1.0, 0.0);
        assert_eq!(east.angle_to(v(1.0, 0.0)), 0.0);
        assert!((east.angle_to(v(0.0, 1.0)) - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert!((east.angle_to(v(0.0, -1.0)) + std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert!((east.angle_to(v(-1.0, 0.0)).abs() - std::f64::consts::PI).abs() < 1e-12);
    }

    /// The reason for `atan2` over `acos`: a turn this small still has to come back
    /// with useful digits, because corner detection compares neighbouring turns.
    #[test]
    fn tiny_angles_keep_their_precision() {
        let angle = v(1.0, 0.0).angle_to(v(1.0, 1e-9));
        assert!((angle - 1e-9).abs() < 1e-18, "{angle}");
    }
}
