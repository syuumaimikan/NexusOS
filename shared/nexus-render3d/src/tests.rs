//! What the renderer does, checked where it is easy to be subtly wrong.

use alloc::vec;

use super::{
    divide, multiply, project, whole, Canvas, Colour, Fixed, Point, Transform, Vertex, ONE,
};

const BLACK: Colour = 0x0000_0000;
const RED: Colour = 0x00FF_0000;
const GREEN: Colour = 0x0000_FF00;

fn vertex(x: i32, y: i32, depth: i32) -> Vertex {
    Vertex {
        x,
        y,
        depth: whole(depth),
    }
}

// -- the arithmetic ----------------------------------------------------------

#[test]
fn one_times_anything_is_anything() {
    assert_eq!(multiply(ONE, whole(7)), whole(7));
    assert_eq!(multiply(whole(-3), ONE), whole(-3));
}

#[test]
fn a_half_times_a_half_is_a_quarter() {
    let half = ONE / 2;
    assert_eq!(multiply(half, half), ONE / 4);
}

/// The products are what overflow, not the results. Two numbers near one
/// another in a 16.16 format have thirty-two bits of fraction between them, so
/// the multiply has to go through a wider type or this is wrong for ordinary
/// values rather than extreme ones.
#[test]
fn large_values_multiply_without_wrapping() {
    assert_eq!(multiply(whole(200), whole(150)), whole(30000));
}

#[test]
fn division_round_trips() {
    for value in [1, 2, 7, 100, -5] {
        let there = divide(whole(value), whole(4));
        assert_eq!(multiply(there, whole(4)), whole(value), "for {value}");
    }
}

/// Zero rather than a panic. This is reached once per vertex on geometry that
/// came from a program, and a renderer that stopped the machine over a point
/// landing exactly on the eye is one nobody could hand untrusted data to.
#[test]
fn dividing_by_nothing_gives_nothing() {
    assert_eq!(divide(whole(5), 0), 0);
}

// -- transforms --------------------------------------------------------------

#[test]
fn the_identity_changes_nothing() {
    let point = Point::at(3, -4, 5);
    assert_eq!(Transform::identity().apply(point), point);
}

#[test]
fn translation_moves() {
    let move_it = Transform::translation(Point::at(1, 2, 3));
    assert_eq!(move_it.apply(Point::at(10, 10, 10)), Point::at(11, 12, 13));
}

/// A quarter turn about the vertical axis sends the x axis onto the z axis.
#[test]
fn a_quarter_turn() {
    let turn = Transform::turn_y(ONE, 0);
    let moved = turn.apply(Point::at(1, 0, 0));
    assert_eq!(moved.x, 0);
    assert_eq!(moved.z, whole(-1));
}

/// Two quarter turns are a half turn, which is what composition has to mean.
#[test]
fn turns_compose() {
    let quarter = Transform::turn_y(ONE, 0);
    let half = quarter.then(quarter);
    let moved = half.apply(Point::at(1, 0, 0));
    assert_eq!(moved.x, whole(-1));
    assert_eq!(moved.z, 0);
}

/// Order matters, and the name says which: `a.then(b)` is a first.
#[test]
fn composition_is_in_the_order_it_reads() {
    let turn = Transform::turn_y(ONE, 0);
    let move_it = Transform::translation(Point::at(10, 0, 0));

    // Turned, then moved: the turn sends x onto -z, and the move adds ten to x.
    let turn_then_move = turn.then(move_it).apply(Point::at(1, 0, 0));
    assert_eq!(turn_then_move.x, whole(10));
    assert_eq!(turn_then_move.z, whole(-1));

    // Moved, then turned: x becomes eleven and the turn takes it to -z.
    let move_then_turn = move_it.then(turn).apply(Point::at(1, 0, 0));
    assert_eq!(move_then_turn.x, 0);
    assert_eq!(move_then_turn.z, whole(-11));
}

// -- projection --------------------------------------------------------------

#[test]
fn further_away_is_smaller() {
    let near = project(Point::at(1, 0, 2), 100, 100, whole(100)).unwrap();
    let far = project(Point::at(1, 0, 8), 100, 100, whole(100)).unwrap();
    let centre = 50;
    assert!(
        near.x - centre > far.x - centre,
        "near {} should be further from the centre than far {}",
        near.x,
        far.x
    );
}

#[test]
fn the_middle_of_the_world_is_the_middle_of_the_screen() {
    let middle = project(Point::at(0, 0, 5), 80, 60, whole(100)).unwrap();
    assert_eq!((middle.x, middle.y), (40, 30));
}

/// Up in the world is up the screen, which is down in memory.
#[test]
fn up_is_up() {
    let above = project(Point::at(0, 1, 5), 80, 60, whole(100)).unwrap();
    assert!(above.y < 30, "a point above the eye drew at y={}", above.y);
}

/// Refused rather than drawn wrong. Clipping a triangle against the near plane
/// properly means cutting it in two and making new corners; this renderer does
/// not, and says so by refusing the vertex instead of projecting a point behind
/// the eye onto the screen in front of it.
#[test]
fn nothing_behind_the_eye_is_projected() {
    assert!(project(Point::at(0, 0, 0), 80, 60, whole(100)).is_none());
    assert!(project(Point::at(0, 0, -3), 80, 60, whole(100)).is_none());
}

// -- the rasteriser ----------------------------------------------------------

fn canvas(width: usize, height: usize) -> (vec::Vec<Colour>, usize, usize) {
    (vec![BLACK; width * height], width, height)
}

#[test]
fn a_triangle_covers_what_it_should() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    // A right triangle over the top-left quarter.
    let written = target.triangle([vertex(0, 0, 1), vertex(8, 0, 1), vertex(0, 8, 1)], RED);

    assert!(written > 0, "nothing was drawn");
    assert_eq!(target.pixel(1, 1), Some(RED), "inside");
    assert_eq!(target.pixel(7, 7), Some(BLACK), "outside the hypotenuse");
    assert_eq!(target.pixel(15, 15), Some(BLACK), "far outside");
}

/// The far side of a solid is fed to the same call as the near side. Without
/// this every surface is painted twice and the depth buffer has to sort out an
/// interior nobody can see.
#[test]
fn a_triangle_facing_away_is_dropped() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    let facing = [vertex(0, 0, 1), vertex(8, 0, 1), vertex(0, 8, 1)];
    let away = [facing[0], facing[2], facing[1]];

    assert!(target.triangle(facing, RED) > 0);
    assert_eq!(target.triangle(away, GREEN), 0, "the back face was drawn");
}

#[test]
fn a_nearer_triangle_wins() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    let shape = [vertex(0, 0, 9), vertex(15, 0, 9), vertex(0, 15, 9)];
    assert!(target.triangle(shape, RED) > 0);

    let nearer = [vertex(0, 0, 2), vertex(15, 0, 2), vertex(0, 15, 2)];
    assert!(target.triangle(nearer, GREEN) > 0);
    assert_eq!(target.pixel(2, 2), Some(GREEN));
}

#[test]
fn a_further_triangle_does_not() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    let near = [vertex(0, 0, 2), vertex(15, 0, 2), vertex(0, 15, 2)];
    assert!(target.triangle(near, GREEN) > 0);

    let far = [vertex(0, 0, 9), vertex(15, 0, 9), vertex(0, 15, 9)];
    target.triangle(far, RED);
    assert_eq!(target.pixel(2, 2), Some(GREEN), "the far one painted over");
}

/// Order must not matter. A renderer that only looks right when the furthest
/// thing is drawn first is a renderer with no depth buffer at all, and the two
/// are indistinguishable in a scene that happens to be sorted.
#[test]
fn depth_does_not_depend_on_the_order_they_arrive_in() {
    let shape_near = [vertex(0, 0, 2), vertex(15, 0, 2), vertex(0, 15, 2)];
    let shape_far = [vertex(0, 0, 9), vertex(15, 0, 9), vertex(0, 15, 9)];

    let (mut first, w, h) = canvas(16, 16);
    let mut one = Canvas::new(&mut first, w, h).unwrap();
    one.triangle(shape_far, RED);
    one.triangle(shape_near, GREEN);

    let (mut second, w, h) = canvas(16, 16);
    let mut two = Canvas::new(&mut second, w, h).unwrap();
    two.triangle(shape_near, GREEN);
    two.triangle(shape_far, RED);

    assert_eq!(one.pixel(3, 3), two.pixel(3, 3));
    assert_eq!(one.pixel(3, 3), Some(GREEN));
}

/// The seam test. Two triangles sharing an edge must cover the quad exactly
/// once: a gap shows as a line of background through a solid, and a double
/// write is invisible until the two have different colours and one flickers.
#[test]
fn two_triangles_sharing_an_edge_leave_no_gap() {
    let (mut pixels, w, h) = canvas(8, 8);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    let a = [vertex(0, 0, 1), vertex(7, 0, 1), vertex(0, 7, 1)];
    let b = [vertex(7, 0, 1), vertex(7, 7, 1), vertex(0, 7, 1)];
    target.triangle(a, RED);
    target.triangle(b, RED);

    // Every pixel on the shared edge is painted.
    for step in 1..7 {
        assert_eq!(
            target.pixel(7 - step, step),
            Some(RED),
            "a gap on the shared edge at ({}, {step})",
            7 - step
        );
    }
}

#[test]
fn a_triangle_off_the_edge_is_clipped_not_refused() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    // Most of it is to the left of the canvas.
    let written = target.triangle(
        [vertex(-20, 0, 1), vertex(4, 0, 1), vertex(-20, 15, 1)],
        RED,
    );
    assert!(written > 0, "the part on screen was not drawn");
    assert_eq!(target.pixel(1, 1), Some(RED));
}

#[test]
fn a_triangle_entirely_off_screen_draws_nothing() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();
    assert_eq!(
        target.triangle(
            [
                vertex(100, 100, 1),
                vertex(120, 100, 1),
                vertex(100, 120, 1)
            ],
            RED
        ),
        0
    );
}

/// Both buffers, always. A frame drawn on top of the last frame's depths shows
/// this frame's triangles vanishing behind the previous frame's, which reads as
/// broken geometry and is not.
#[test]
fn clearing_forgets_the_depths_as_well_as_the_colours() {
    let (mut pixels, w, h) = canvas(16, 16);
    let mut target = Canvas::new(&mut pixels, w, h).unwrap();

    let near = [vertex(0, 0, 2), vertex(15, 0, 2), vertex(0, 15, 2)];
    target.triangle(near, GREEN);
    target.clear(BLACK);

    let far = [vertex(0, 0, 9), vertex(15, 0, 9), vertex(0, 15, 9)];
    assert!(
        target.triangle(far, RED) > 0,
        "a far triangle was hidden by a cleared frame's depths"
    );
    assert_eq!(target.pixel(2, 2), Some(RED));
    assert_eq!(target.depth_at(2, 2), Some(whole(9) as Fixed));
}

#[test]
fn a_canvas_of_the_wrong_size_is_refused() {
    let mut pixels = vec![BLACK; 10];
    assert!(Canvas::new(&mut pixels, 4, 4).is_none());
    assert!(Canvas::new(&mut pixels, 0, 0).is_none());
    assert!(Canvas::new(&mut pixels, 10, 1).is_some());
}
