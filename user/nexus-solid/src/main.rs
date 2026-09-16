//! A solid, turning, drawn by the processor one pixel at a time.
//!
//! An octahedron: six points, eight triangular faces, each a different colour.
//! A shape rather than a picture of anything -- the point of it is that the
//! thing on screen is a *solid*, and the way you can tell is that the faces
//! behind it are not drawn and the ones in front cover the ones behind.
//!
//! # What this demonstrates, exactly
//!
//! That this system can draw in three dimensions. Not that it can do so
//! quickly, and not through any graphics interface: `nexus-render3d` computes
//! every pixel on the processor, and there is no Vulkan, no OpenGL and no
//! hardware acceleration anywhere beneath this. The GPU on this machine is a
//! *display* -- it shows what is put in front of it -- and this program puts
//! something in front of it that was worked out in software.
//!
//! # The check it does on itself
//!
//! For every face of every frame, this works out from the geometry whether the
//! face *should* be visible -- whether its outward normal leans towards the eye
//! -- and compares that against whether the rasteriser drew it.
//!
//! Counting drawn against turned away was the first version of this check, and
//! it is too weak to be worth having. A convex solid shows about half its faces
//! from any direction, so reversing *every* face swaps which half is drawn and
//! leaves both counts looking healthy -- while what is on screen is the inside
//! of the shape. The normal is what tells those two apart.

#![no_std]
#![no_main]

extern crate alloc;

mod turn;

use core::panic::PanicInfo;

use alloc::format;

use nexus_render3d::{project, whole, Canvas, Colour, Fixed, Point, Transform};
use nexus_user::Handle;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor, which is also this program's parent.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// And where the depths go, well clear of it.
///
/// Asked of the kernel rather than taken from the heap, because a depth buffer
/// is exactly as large as the window and this program's heap is a quarter of a
/// megabyte. The first version of this asked the allocator and died in
/// `handle_alloc_error` the moment it was given a real window -- which is the
/// sort of thing only running it finds.
const DEPTHS_AT: usize = 0x0000_0000_3000_0000;

/// How many frames to draw before saying what happened.
///
/// Bounded rather than endless, because this is a demonstration and a test
/// drives it: a program that never stopped would have to be killed to be
/// measured, and a killed program reports nothing.
const FRAMES: usize = 180;

/// How far the eye is from the middle of the solid.
const DISTANCE: i32 = 5;

/// How large the projection makes things. Bigger fills more of the window.
const SCALE: i32 = 420;

/// The six points of an octahedron: one on each end of each axis.
const POINTS: [Point; 6] = [
    Point::at(1, 0, 0),
    Point::at(-1, 0, 0),
    Point::at(0, 1, 0),
    Point::at(0, -1, 0),
    Point::at(0, 0, 1),
    Point::at(0, 0, -1),
];

/// The eight faces, each wound clockwise seen from outside the solid.
///
/// Clockwise, not counter-clockwise, and that deserves a sentence because it is
/// the thing a reader will take for a mistake. `project` turns the world the
/// right way up -- up in the world is up the screen, which is *down* in memory
/// -- and a reflection reverses the sense of a turn. A face wound
/// counter-clockwise from outside therefore arrives at the rasteriser wound
/// clockwise, and clockwise on screen is what it calls front-facing.
const FACES: [([usize; 3], Colour); 8] = [
    ([2, 0, 4], 0x00E8_6A5C),
    ([2, 5, 0], 0x00E8_B45C),
    ([2, 1, 5], 0x00B4_E85C),
    ([2, 4, 1], 0x005C_E86A),
    ([3, 4, 0], 0x005C_E8E8),
    ([3, 0, 5], 0x005C_8AE8),
    ([3, 5, 1], 0x008A_5CE8),
    ([3, 1, 4], 0x00E8_5CB4),
];

/// How near edge-on a face has to be before the two ways of asking which way
/// it points are allowed to disagree, as a fraction of a face seen square on.
///
/// A sixty-fourth, which is about a degree. Measured rather than guessed: the
/// solid drawn here spans roughly a hundred pixels a face, so the error in a
/// projected triangle's area is a few hundred square pixels against a few
/// thousand for a face seen square on -- and a foreshortened face falls below
/// that somewhere around a percent. The run reports both numbers, so if this
/// ever starts excusing something real the report says so before this constant
/// does.
const SLIVER: i64 = 64;

/// What is behind the solid.
const BACKGROUND: Colour = 0x0006_0D1A;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym main,
    )
}

static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn failed(message: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(message).ok();
}

fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit_with(0)
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("solid: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}

/// Twice the area vector of a triangle, from the order its corners are in.
///
/// `i64` because these are products of fixed-point differences, and the shift
/// is there so that the dot product of two of them does not need a third type
/// again.
fn normal(triangle: [Point; 3]) -> (i64, i64, i64) {
    let [p, q, r] = triangle;
    let ux = i64::from(q.x - p.x);
    let uy = i64::from(q.y - p.y);
    let uz = i64::from(q.z - p.z);
    let vx = i64::from(r.x - p.x);
    let vy = i64::from(r.y - p.y);
    let vz = i64::from(r.z - p.z);
    (
        (uy * vz - uz * vy) >> 20,
        (uz * vx - ux * vz) >> 20,
        (ux * vy - uy * vx) >> 20,
    )
}

/// How many faces are wound the wrong way round, judged against the solid.
///
/// # Why this is separate from the check the frames do
///
/// The per-frame check compares what the geometry says about a face against
/// what the rasteriser did with it -- and it is **blind to the winding**,
/// because both sides of that comparison are computed from this table. Reverse
/// every face and the computed normal reverses too, the two still agree, and
/// the program reports success while drawing the inside of the shape. That was
/// measured, not reasoned about: the table below was reversed on purpose and
/// the run passed.
///
/// So the outward direction has to come from something the table cannot move.
/// This solid is centred on the origin, so the direction from the centre to a
/// face *is* its outward direction, and the centroid of three corners is a
/// point on that ray. The winding is right when the normal the corner order
/// produces leans against it -- the table is wound clockwise seen from outside,
/// and `u x v` for such a face points into the solid.
///
/// Model space, before any transform, because the centre is the origin only
/// here. It is the same eight faces every frame, so this is answered once.
fn faces_wound_inside_out() -> usize {
    let mut wrong = 0;
    for (corners, _) in FACES {
        let triangle = [POINTS[corners[0]], POINTS[corners[1]], POINTS[corners[2]]];
        let (nx, ny, nz) = normal(triangle);
        // Three times the centroid, which is on the same ray from the origin.
        let cx = i64::from(triangle[0].x + triangle[1].x + triangle[2].x) >> 10;
        let cy = i64::from(triangle[0].y + triangle[1].y + triangle[2].y) >> 10;
        let cz = i64::from(triangle[0].z + triangle[1].z + triangle[2].z) >> 10;
        if nx * cx + ny * cy + nz * cz >= 0 {
            wrong += 1;
        }
    }
    wrong
}

fn read_u32(buffer: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("solid: FAILED: could not get a heap");
        finish();
    }

    // Before anything is drawn, and before a surface is even asked for: a table
    // wound inside out makes a picture of the inside of the solid, and that is
    // a picture, so nothing later in this program would notice.
    let inside_out = faces_wound_inside_out();
    if inside_out != 0 {
        failed(&format!(
            "solid: FAILED: {} of {} faces are wound inside out; that draws the inside",
            inside_out,
            FACES.len()
        ));
        finish();
    }
    nexus_user::log(&format!(
        "solid: all {} faces are wound so their front is the outside",
        FACES.len()
    ))
    .ok();

    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 1];
    let received = match nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(why) => {
            failed(&format!("solid: FAILED: nothing arrived to draw on: {why}"));
            finish();
        }
    };
    if received.handles != 1 || received.bytes < 16 {
        failed("solid: FAILED: no surface came with the message");
        finish();
    }

    let width = read_u32(&buffer, 0) as usize;
    let height = read_u32(&buffer, 4) as usize;
    let surface = handles[0];

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("solid: FAILED: could not map its surface");
        finish();
    };
    // Checked against the mapping and not against the message. Memory is handed
    // out in whole pages, so what is mapped is at least what was asked for and
    // usually more -- and a program that trusted the message would write past
    // the end of its own buffer the day the two stopped agreeing.
    if width * height * 4 > mapped {
        failed("solid: FAILED: the surface is smaller than the size it was given");
        finish();
    }

    // The depths, from the kernel and not from the heap. The same size as the
    // surface, because there is one depth per pixel, and made once: a renderer
    // that asked for one per frame would spend more on getting memory than on
    // the triangles.
    let Ok(depths) = nexus_user::memory_create(width * height * 4) else {
        failed("solid: FAILED: could not get memory for the depths");
        finish();
    };
    let Ok(depth_mapped) = nexus_user::memory_map(depths, DEPTHS_AT, true) else {
        failed("solid: FAILED: could not map the depths");
        finish();
    };
    if width * height * 4 > depth_mapped {
        failed("solid: FAILED: the depths are smaller than the surface");
        finish();
    }

    // SAFETY: the compositor lent the first of these writable and the second
    // was just mapped writable by the kernel; both were checked against what
    // the mapping actually gave back. Nothing else has either: a surface
    // belongs to one client, and the depths belong to this program.
    let pixels =
        unsafe { core::slice::from_raw_parts_mut(SURFACE_AT as *mut Colour, width * height) };
    let depth = unsafe { core::slice::from_raw_parts_mut(DEPTHS_AT as *mut Fixed, width * height) };
    let Some(mut canvas) = Canvas::with_depth(pixels, depth, width, height) else {
        failed("solid: FAILED: the surface is not the shape it says it is");
        finish();
    };

    let mut drawn = 0usize;
    let mut turned_away = 0usize;
    let mut painted = 0usize;
    let mut refused = 0usize;
    let mut disagreed = 0usize;
    // How hard the worst disagreement was leaning, and how hard the most
    // face-on face of the run was leaning, so that the first can be read as a
    // fraction of the second.
    //
    // A face seen edge-on projects to a sliver a pixel or two wide, and the
    // sign of a sliver's area is decided by where its three corners round to
    // whole pixels rather than by which way it faces. So the geometry and the
    // rasteriser *cannot* be required to agree there, and a check that demanded
    // it would fail on a renderer that was working.
    //
    // Two numbers rather than a constant because a constant would be a number
    // chosen until the test passed. The scale of `towards` depends on the size
    // of the solid, how far away it is and how the fixed-point values were
    // shifted; measuring the largest one the run actually produced is the only
    // way to say what a *small* one is.
    let mut worst = 0i64;
    let mut strongest = 0i64;

    for frame in 0..FRAMES {
        // Two turns at once, at different rates, so that every face comes round
        // and no axis stays still. A solid turning about one axis shows the
        // same silhouette every half turn and proves less.
        let about_y = Transform::turn_y(turn::sine(frame * 2), turn::cosine(frame * 2));
        let about_x = Transform::turn_x(turn::sine(frame), turn::cosine(frame));
        let spin = about_y.then(about_x);
        // And back, away from the eye, so the whole of it is in front.
        let place = spin.then(Transform::translation(Point::at(0, 0, DISTANCE)));

        canvas.clear(BACKGROUND);

        // Every point once, rather than once per face that uses it. Each point
        // of an octahedron is a corner of four faces, so this is four times less
        // arithmetic and -- more to the point -- it cannot produce two slightly
        // different answers for one corner, which is what makes a crack.
        let mut camera = [Point::new(0, 0, 0); POINTS.len()];
        let mut screen = [None; POINTS.len()];
        for (index, point) in POINTS.iter().enumerate() {
            camera[index] = place.apply(*point);
            screen[index] = project(camera[index], width, height, whole(SCALE));
        }

        for (corners, colour) in FACES {
            // What the geometry says, worked out before the rasteriser is
            // asked. A face is visible when its outward normal leans towards
            // the eye, which is the sign of the dot product of that normal with
            // the line from the eye to any point of the face.
            let (p, q, r) = (camera[corners[0]], camera[corners[1]], camera[corners[2]]);
            let ux = i64::from(q.x - p.x);
            let uy = i64::from(q.y - p.y);
            let uz = i64::from(q.z - p.z);
            let vx = i64::from(r.x - p.x);
            let vy = i64::from(r.y - p.y);
            let vz = i64::from(r.z - p.z);
            // Scaled down before the dot product: these are already products of
            // differences of fixed-point numbers, and the dot is two deeper.
            let nx = (uy * vz - uz * vy) >> 20;
            let ny = (uz * vx - ux * vz) >> 20;
            let nz = (ux * vy - uy * vx) >> 20;
            let towards =
                nx * i64::from(p.x >> 10) + ny * i64::from(p.y >> 10) + nz * i64::from(p.z >> 10);
            // The eye is at the origin looking along +z, so a face leaning
            // towards it has a normal pointing back against the line to it.
            let should_show = towards < 0;

            let (Some(a), Some(b), Some(c)) =
                (screen[corners[0]], screen[corners[1]], screen[corners[2]])
            else {
                // A corner behind the eye. This renderer does not cut triangles
                // against the near plane, so the face is dropped whole and
                // counted -- with the solid five units away it should never
                // happen, and a count above zero means the geometry moved.
                refused += 1;
                continue;
            };
            let wrote = canvas.triangle([a, b, c], colour);
            if wrote == 0 {
                turned_away += 1;
            } else {
                drawn += 1;
                painted += wrote;
            }

            // And the two have to agree. A face the geometry calls visible and
            // the rasteriser refused is a face wound the wrong way round; one
            // the geometry calls hidden and the rasteriser drew is the inside
            // of the solid painted over the outside. Both look like a shape.
            //
            // A face seen nearly edge-on covers no pixels while still, strictly,
            // facing the eye -- so the first of those is only counted when the
            // normal leans towards the eye by more than a hair.
            if towards.abs() > strongest {
                strongest = towards.abs();
            }
            if should_show != (wrote > 0) {
                disagreed += 1;
                if towards.abs() > worst {
                    worst = towards.abs();
                }
            }
        }

        if nexus_user::send(COMPOSITOR, b"damaged", &[]).is_err() {
            failed("solid: FAILED: the compositor stopped listening");
            finish();
        }
        // And wait to be told the frame is on screen, so the next one does not
        // start while this one is being read.
        let mut answer = [0u8; 16];
        let mut none = [Handle(0); 1];
        if nexus_user::receive(COMPOSITOR, &mut answer, &mut none).is_err() {
            failed("solid: FAILED: the compositor went away mid-frame");
            finish();
        }
    }

    nexus_user::log(&format!(
        "solid: {FRAMES} frames, {drawn} faces drawn and {turned_away} turned away, {painted} pixels by the processor"
    ))
    .ok();

    // The check this program exists to make. A convex solid shows about half its
    // faces from any direction: all of them drawn means the back is being
    // painted over the front, none means the winding is inside out, and both of
    // those still look like *something* on screen.
    let expected = FRAMES * FACES.len();
    if drawn + turned_away + refused != expected {
        failed(&format!(
            "solid: FAILED: {expected} faces were offered and {} accounted for",
            drawn + turned_away + refused
        ));
    } else if drawn == 0 || turned_away == 0 {
        failed(&format!(
            "solid: FAILED: {drawn} drawn and {turned_away} turned away; a solid shows about half"
        ));
    } else if strongest == 0 {
        failed("solid: FAILED: no face leaned any way at all, so nothing was checked");
    } else if worst * SLIVER > strongest {
        // A disagreement well away from edge-on. That is a face wound the wrong
        // way round, or the inside of the solid painted over the outside, and
        // both of those still look like a shape on screen.
        failed(&format!(
            "solid: FAILED: {disagreed} face(s) disagreed; the worst leaned by {worst} of {strongest}, too far from edge-on to be rounding"
        ));
    } else if refused != 0 {
        failed(&format!(
            "solid: FAILED: {refused} faces had a corner behind the eye"
        ));
    } else {
        nexus_user::log(&format!(
            "solid: geometry and rasteriser agreed on every face but {disagreed} seen edge-on, the worst leaning by {worst} of {strongest}"
        ))
        .ok();
    }

    // Left as it was found, so the compositor's surface is not still mapped in a
    // process that has finished with it.
    nexus_user::memory_unmap(surface, SURFACE_AT).ok();
    nexus_user::memory_unmap(depths, DEPTHS_AT).ok();
    nexus_user::close(depths).ok();
    finish()
}
