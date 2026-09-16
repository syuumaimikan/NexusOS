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

use nexus_render3d::{project, whole, Canvas, Colour, Point, Transform};
use nexus_user::Handle;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor, which is also this program's parent.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

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

fn read_u32(buffer: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("solid: FAILED: could not get a heap");
        finish();
    }

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

    // The depth buffer lives here, beside the colours, and is made once. A
    // renderer that allocated one per frame would spend more on the allocator
    // than on the triangles.
    //
    // SAFETY: the compositor lent this memory writable and the size was checked
    // against the mapping above. Nothing else has it: a surface belongs to one
    // client.
    let pixels =
        unsafe { core::slice::from_raw_parts_mut(SURFACE_AT as *mut Colour, width * height) };
    let Some(mut canvas) = Canvas::new(pixels, width, height) else {
        failed("solid: FAILED: the surface is not the shape it says it is");
        finish();
    };

    let mut drawn = 0usize;
    let mut turned_away = 0usize;
    let mut painted = 0usize;
    let mut refused = 0usize;
    let mut disagreed = 0usize;

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
            let edge_on = -64;
            if should_show && wrote == 0 && towards < edge_on {
                disagreed += 1;
            }
            if !should_show && wrote > 0 {
                disagreed += 1;
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
        "solid: {FRAMES} frames, {drawn} faces drawn and {turned_away} turned away, \
         {painted} pixels by the processor"
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
    } else if disagreed != 0 {
        failed(&format!(
            "solid: FAILED: {disagreed} face(s) where the geometry and the rasteriser disagreed about which way they point"
        ));
    } else if refused != 0 {
        failed(&format!(
            "solid: FAILED: {refused} faces had a corner behind the eye"
        ));
    } else {
        nexus_user::log(
            "solid: every face the geometry called visible was drawn, and every face it called hidden was refused",
        )
        .ok();
    }

    // Left as it was found, so the compositor's surface is not still mapped in a
    // process that has finished with it.
    nexus_user::memory_unmap(surface, SURFACE_AT).ok();
    finish()
}
