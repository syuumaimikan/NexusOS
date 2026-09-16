//! Cutting triangles against the near plane.
//!
//! Until this existed, a triangle with any corner behind the eye was dropped
//! whole. That is a defensible small version and it has a visible cost: you
//! cannot move the eye *into* a scene. A wall you walk up to vanishes the
//! moment one of its corners passes you, and what goes is the whole wall, not
//! the part behind you.
//!
//! So the triangle is cut instead. A plane divides a triangle into at most a
//! triangle and a quadrilateral, and a quadrilateral is two triangles -- which
//! is why this returns up to two and never more.
//!
//! # The part that has to be right
//!
//! **Winding.** The renderer decides which way a face points from the sign of
//! its area, so a cut that reordered the corners would turn a front face into a
//! back one and the wall you walked into would disappear again, for a different
//! reason. The pieces below are built by walking the original corners in their
//! original order and inserting crossings where they fall, which preserves it
//! by construction rather than by care.

use super::{divide, multiply, Fixed, Point};

/// How far in front of the eye the near plane sits.
///
/// Not zero. At exactly zero the projection divides by nothing, and a corner
/// that lands microscopically in front of the eye projects to somewhere far off
/// the screen -- which is a triangle stretched across the whole display for one
/// frame, and is the artifact this constant exists to prevent.
///
/// A quarter of a unit, where the shapes this draws are about a unit across.
pub const NEAR: Fixed = super::ONE / 4;

/// The near plane is in front of the eye, and nearer than the shapes.
///
/// Checked when this is compiled rather than when it is run. It is a fact
/// about two constants, and a test that asserted it would be a test that
/// can only fail on a build that already exists.
const _: () = assert!(NEAR > 0 && NEAR < super::ONE);

/// What was left of a triangle after the plane took its share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clipped {
    pieces: [[Point; 3]; 2],
    count: usize,
}

impl Clipped {
    /// The triangles to draw, which may be none, one or two.
    #[must_use]
    pub fn pieces(&self) -> &[[Point; 3]] {
        &self.pieces[..self.count]
    }

    /// How many there are.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }
}

/// Where the segment from `from` to `to` crosses `z = near`.
///
/// Both components are carried across by the same fraction, which is what makes
/// the cut a straight line on the surface rather than a kink.
fn crossing(from: Point, to: Point, near: Fixed) -> Point {
    // How far along the segment the plane is. `divide` answers zero when the
    // two ends are at the same depth -- which cannot happen here, because one
    // is in front of the plane and the other behind it, so they differ.
    let along = divide(near - from.z, to.z - from.z);
    Point::new(
        from.x + multiply(to.x - from.x, along),
        from.y + multiply(to.y - from.y, along),
        // Set rather than interpolated. The arithmetic would land within a
        // rounding error of `near`, and a corner a hair behind the plane is a
        // corner the projection refuses -- so the one value that is known
        // exactly is written exactly.
        near,
    )
}

/// Cut a triangle against the near plane, keeping what is in front of it.
///
/// The corners come back in the original winding, and every corner of every
/// piece is at or in front of `near`.
#[must_use]
pub fn near_plane(triangle: [Point; 3], near: Fixed) -> Clipped {
    let nothing = Clipped {
        pieces: [[Point::new(0, 0, 0); 3]; 2],
        count: 0,
    };

    let inside = [
        triangle[0].z >= near,
        triangle[1].z >= near,
        triangle[2].z >= near,
    ];
    let kept = usize::from(inside[0]) + usize::from(inside[1]) + usize::from(inside[2]);

    match kept {
        0 => nothing,
        3 => Clipped {
            pieces: [triangle, [Point::new(0, 0, 0); 3]],
            count: 1,
        },
        1 => {
            // One corner in front. Walking from it, the polygon is: the corner,
            // the crossing on the edge leaving it, the crossing on the edge
            // arriving at it.
            let a = inside.iter().position(|&keep| keep).unwrap_or(0);
            let b = (a + 1) % 3;
            let c = (a + 2) % 3;
            Clipped {
                pieces: [
                    [
                        triangle[a],
                        crossing(triangle[a], triangle[b], near),
                        crossing(triangle[c], triangle[a], near),
                    ],
                    [Point::new(0, 0, 0); 3],
                ],
                count: 1,
            }
        }
        _ => {
            // Two in front, so what is left is a quadrilateral: the two corners
            // that stayed, then the crossing leaving the second, then the
            // crossing arriving at the first. Split along the diagonal from the
            // first corner, which keeps both halves wound as the original was.
            let c = inside.iter().position(|&keep| !keep).unwrap_or(0);
            let a = (c + 1) % 3;
            let b = (c + 2) % 3;
            let leaving = crossing(triangle[b], triangle[c], near);
            let arriving = crossing(triangle[c], triangle[a], near);
            Clipped {
                pieces: [
                    [triangle[a], triangle[b], leaving],
                    [triangle[a], leaving, arriving],
                ],
                count: 2,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{near_plane, NEAR};
    use crate::{whole, Point};
    use alloc::vec;

    /// A triangle wound so the rasteriser calls it front-facing once projected.
    ///
    /// Bottom-left, top, bottom-right -- which reads as counter-clockwise on a
    /// page and is clockwise on the screen, because `project` turns the world
    /// the right way up and reverses the sense of a turn while it does. Getting
    /// this backwards is how the first version of this test failed: the
    /// reference triangle was culled, so there was nothing to compare against.
    ///
    /// Lifted clear of the eye, and that is not decoration either. Centred, the
    /// plane through these three corners passes through the origin for several
    /// of the depth sets below -- the triangle is then edge-on to the eye and
    /// projects to a *line*, with an area of exactly zero. A clipper could be
    /// perfect and still fail such a test.
    fn facing(z: [i32; 3]) -> [Point; 3] {
        const LIFT: i32 = 2;
        [
            Point::new(whole(-1), whole(LIFT - 1), whole(z[0])),
            Point::new(whole(0), whole(LIFT + 1), whole(z[1])),
            Point::new(whole(1), whole(LIFT - 1), whole(z[2])),
        ]
    }

    #[test]
    fn a_triangle_wholly_in_front_is_untouched() {
        let triangle = facing([5, 5, 5]);
        let cut = near_plane(triangle, NEAR);
        assert_eq!(cut.count(), 1);
        assert_eq!(cut.pieces()[0], triangle);
    }

    #[test]
    fn a_triangle_wholly_behind_is_gone() {
        let triangle = facing([-1, -2, -3]);
        assert_eq!(near_plane(triangle, NEAR).count(), 0);
    }

    /// One corner survives, and the two new ones sit exactly on the plane. Not
    /// near it: a corner a hair behind is a corner the projection refuses, and
    /// the whole piece would vanish for a rounding error.
    #[test]
    fn one_corner_in_front_makes_one_triangle_on_the_plane() {
        let triangle = facing([5, -2, -3]);
        let cut = near_plane(triangle, NEAR);
        assert_eq!(cut.count(), 1);

        let piece = cut.pieces()[0];
        assert_eq!(piece[0], triangle[0], "the surviving corner moved");
        assert_eq!(piece[1].z, NEAR);
        assert_eq!(piece[2].z, NEAR);
        for corner in piece {
            assert!(corner.z >= NEAR, "a corner stayed behind the plane");
        }
    }

    #[test]
    fn two_corners_in_front_make_two_triangles() {
        let triangle = facing([5, 6, -3]);
        let cut = near_plane(triangle, NEAR);
        assert_eq!(cut.count(), 2);
        for piece in cut.pieces() {
            for corner in *piece {
                assert!(corner.z >= NEAR, "a corner stayed behind the plane");
            }
        }
        // Both corners that were in front are still corners of something.
        let all: vec::Vec<Point> = cut.pieces().iter().flatten().copied().collect();
        assert!(all.contains(&triangle[0]));
        assert!(all.contains(&triangle[1]));
    }

    /// A corner exactly on the plane counts as in front, so a triangle resting
    /// on it is not cut at all -- and does not produce a piece of zero area,
    /// which would be a triangle the rasteriser walks and never fills.
    #[test]
    fn a_corner_exactly_on_the_plane_is_kept() {
        let triangle = [
            Point::new(whole(-1), whole(-1), NEAR),
            Point::new(whole(1), whole(-1), whole(5)),
            Point::new(whole(0), whole(1), whole(5)),
        ];
        let cut = near_plane(triangle, NEAR);
        assert_eq!(cut.count(), 1);
        assert_eq!(cut.pieces()[0], triangle);
    }

    /// The property the whole file turns on: a cut must not turn a face around.
    ///
    /// Checked against the triangle's own normal rather than against the eye.
    /// The first version of this test drew the pieces and demanded that each
    /// one covered some pixels -- which is a *different* claim, and a false
    /// one: a triangle tilted steeply enough genuinely faces away, and culling
    /// it is the renderer working. Two of the four depth sets below do exactly
    /// that, and the test failed on them while the clipper was correct.
    ///
    /// The normal says what was actually meant. If every piece points the same
    /// way as the triangle it came from, the cut preserved the winding, and
    /// whether any of them happens to face the eye is a separate question.
    #[test]
    fn cutting_does_not_turn_a_face_around() {
        /// Twice the area vector, in `i64` so the products cannot wrap.
        fn normal(triangle: &[Point; 3]) -> (i64, i64, i64) {
            let ux = i64::from(triangle[1].x - triangle[0].x);
            let uy = i64::from(triangle[1].y - triangle[0].y);
            let uz = i64::from(triangle[1].z - triangle[0].z);
            let vx = i64::from(triangle[2].x - triangle[0].x);
            let vy = i64::from(triangle[2].y - triangle[0].y);
            let vz = i64::from(triangle[2].z - triangle[0].z);
            (uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx)
        }

        for depths in [[3, -1, -1], [3, 4, -1], [-1, 3, 4], [4, -1, 3], [5, 5, 5]] {
            let whole_triangle = facing(depths);
            let facing_way = normal(&whole_triangle);

            let cut = near_plane(whole_triangle, NEAR);
            assert!(cut.count() > 0, "nothing survived for {depths:?}");

            for piece in cut.pieces() {
                let piece_way = normal(piece);
                // Scaled down before multiplying: these are products of
                // differences of fixed-point numbers, and the dot product of
                // two of them is four multiplications deep.
                let dot = (facing_way.0 >> 20) * (piece_way.0 >> 20)
                    + (facing_way.1 >> 20) * (piece_way.1 >> 20)
                    + (facing_way.2 >> 20) * (piece_way.2 >> 20);
                assert!(
                    dot > 0,
                    "a piece of {depths:?} points the other way: {piece_way:?} against {facing_way:?}"
                );
            }
        }
    }
}
