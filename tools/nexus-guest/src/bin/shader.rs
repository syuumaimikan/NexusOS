//! A program built for Linux that runs a SPIR-V shader and shows the result.
//!
//! No Vulkan, no OpenGL, no driver and no graphics hardware. What it has is the
//! *format*: a SPIR-V module, of the kind `glslang` emits and
//! `vkCreateShaderModule` takes, executed once per fragment by
//! `nexus_spirv::run`, with the colours written into a window.
//!
//! # What the shader is
//!
//! The one every tutorial starts with, and it is worth reading as GLSL before
//! reading it as instructions:
//!
//! ```glsl
//! layout(location = 0) out vec4 colour;
//! void main() {
//!     vec2 uv = gl_FragCoord.xy / vec2(WIDTH, HEIGHT);
//!     float ring = 1.0 - clamp(length(uv - vec2(0.5, 0.5)) * 2.4, 0.0, 1.0);
//!     colour = vec4(uv.x * 0.25 + ring, uv.y * 0.35 + ring * 0.2, ring, 1.0);
//! }
//! ```
//!
//! A gradient with a soft disc in the middle of it. It uses the things that
//! make a shader a shader rather than a loop: a built-in input it did not
//! declare the contents of, component arithmetic, and two functions out of
//! `GLSL.std.450`.
//!
//! There is no shader compiler on this machine, so the module is assembled here
//! word by word, the same way the ELF fixtures next door are. That is a weaker
//! claim than "a shader `glslang` produced runs here", and it is the one being
//! made: what is proved is that the reader and the interpreter agree with the
//! specification as written down in `nexus-spirv`.
//!
//! # Why it draws at a quarter size
//!
//! `nexus_spirv::run` is an interpreter: it looks at every instruction every
//! time it reaches it, through a map from identifier to value. A real driver
//! compiles the module once and runs machine code. So this shades a small grid
//! and scales it up, which is what a game does when it cannot afford native
//! resolution -- for the same reason, at a different order of magnitude.
//!
//! | 260 | `/dev/nexus/display` |
//! | 261 | the window's size |
//! | 262 | mapping the window |
//! | 263 | the arena for the allocator |
//! | 264 | the module this program assembled does not read back |
//! | 265 | the module is not one that can be run |
//! | 266 | the shader failed on a fragment |
//! | 267 | the shader wrote no colour |
//! | 268 | presenting |

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use nexus_spirv::assemble::{joined, text, Builder};
use nexus_spirv::module::Module;
use nexus_spirv::run::{run, Inputs, Value};
use nexus_spirv::{built_in, decoration, glsl, op, storage};

use nexus_guest::{call, expect, fail, heap, say, syscall3, syscall4, syscall6};

nexus_guest::guest_main!(start);
nexus_guest::guest_heap!();

/// The device this program draws through.
const DISPLAY_DEVICE: &[u8] = b"/dev/nexus/display\0";
const DISPLAY_INFO: u32 = 0x4E58_0001;
const DISPLAY_PRESENT: u32 = 0x4E58_0002;

/// How large the shaded grid is. Scaled up to fill the window.
const GRID_WIDTH: u32 = 160;
const GRID_HEIGHT: u32 = 100;

/// How much memory the allocator gets.
///
/// One invocation of this shader allocates a few hundred bytes, and the bump
/// pointer is wound back after each one -- so this has to hold the module and
/// one invocation, not sixteen thousand of them. Four megabytes is far more
/// than either and small enough that a mapping failure would be a real one.
const ARENA: usize = 4 * 1024 * 1024;

/// The pieces of the module a caller needs afterwards.
struct Shader {
    words: Vec<u32>,
}

fn start() -> ! {
    // The allocator first: everything below this line allocates.
    expect(heap::start(ARENA), 263);

    let display = syscall4(
        call::OPENAT,
        (-100i64) as u64,
        DISPLAY_DEVICE.as_ptr() as u64,
        2, // O_RDWR
        0,
    );
    expect(display >= 0, 260);

    let mut info = [0u32; 4];
    expect(
        syscall3(
            call::IOCTL,
            display as u64,
            u64::from(DISPLAY_INFO),
            info.as_mut_ptr() as u64,
        ) == 0,
        261,
    );
    let (width, height, stride) = (info[0], info[1], info[2]);
    expect(width > 0 && height > 0, 261);

    let pixels = syscall6(
        call::MMAP,
        0,
        u64::from(stride) * u64::from(height),
        3, // PROT_READ | PROT_WRITE
        1, // MAP_SHARED
        display as u64,
        0,
    );
    expect(pixels >= 0, 262);
    say("shader: a window, and a SPIR-V module to fill it with");

    // ---- the shader -------------------------------------------------------
    let shader = assemble();
    let module = match Module::parse(&shader.words) {
        Ok(module) => module,
        Err(why) => {
            report("shader: the module did not read back: ", why);
            fail(264)
        }
    };
    expect(module.is_runnable(), 265);
    nexus_guest::fmt::say_with(
        "shader: a fragment shader read, instructions in its body: ",
        module.body.len() as i64,
    );

    // ---- one invocation per point on the grid -----------------------------
    //
    // Somewhere to put the colours first, and the mark *after* it. Winding back
    // past something still in use hands the same memory out twice, and the two
    // things that must survive the wind-back are the module and this vector --
    // so both are allocated before the mark is taken. The capacity is asked for
    // up front for the same reason: a `push` that grew the vector would
    // allocate below the mark and be thrown away on the next fragment.
    let mut shaded: Vec<u32> = Vec::with_capacity((GRID_WIDTH * GRID_HEIGHT) as usize);
    let mark = heap::mark();
    let mut down = 0;
    while down < GRID_HEIGHT {
        let mut across = 0;
        while across < GRID_WIDTH {
            let colour = {
                // `gl_FragCoord` is in window coordinates, with a half-pixel
                // offset: a fragment's position is its centre, not its corner.
                // A shader that sampled a texture would see the difference; this
                // one would not, and it is written correctly anyway because the
                // next shader might.
                let inputs = Inputs::new().built_in(
                    built_in::FRAG_COORD,
                    Value::vec4(across as f32 + 0.5, down as f32 + 0.5, 0.0, 1.0),
                );
                let outputs = match run(&module, &inputs) {
                    Ok(outputs) => outputs,
                    Err(why) => {
                        report("shader: the shader stopped on a fragment: ", why);
                        fail(266)
                    }
                };
                let Some(value) = outputs.at(0) else {
                    fail(267)
                };
                let Ok(parts) = value.floats() else { fail(267) };
                pack(&parts)
            };
            shaded.push(colour);
            // Everything the invocation allocated has gone out of scope above.
            // SAFETY: the only things allocated after `mark` were the values of
            // that one invocation, and the block they were in has ended.
            unsafe { heap::reset_to(mark) };
            across += 1;
        }
        down += 1;
    }
    // What the wind-back is worth: the arena still holds only the module and the
    // colours, after sixteen thousand invocations that each allocated hundreds
    // of times. Without it this program would have wanted gigabytes.
    let (used, arena) = heap::used();
    nexus_guest::fmt::Line::new()
        .text("shader: shaded ")
        .number(i64::from(GRID_WIDTH * GRID_HEIGHT))
        .text(" fragments, and the arena still holds ")
        .number(used as i64)
        .text(" bytes of ")
        .number(arena as i64)
        .say();

    // ---- and onto the screen ---------------------------------------------
    //
    // Nearest neighbour, which is the honest way to scale something shaded at a
    // quarter of the size: it shows the grid rather than blurring it into
    // looking like a full-resolution picture.
    let mut row = 0;
    while row < height {
        let source_row = (row * GRID_HEIGHT) / height;
        let target = pixels as u64 + u64::from(row) * u64::from(stride);
        let mut column = 0;
        while column < width {
            let source_column = (column * GRID_WIDTH) / width;
            let colour = shaded[(source_row * GRID_WIDTH + source_column) as usize];
            // SAFETY: inside the mapping this program made, bounded by the size
            // the device reported.
            unsafe {
                core::ptr::write_volatile((target + u64::from(column) * 4) as *mut u32, colour);
            }
            column += 1;
        }
        row += 1;
    }

    let rectangle = [0u32, 0, width, height];
    if syscall3(
        call::IOCTL,
        display as u64,
        u64::from(DISPLAY_PRESENT),
        rectangle.as_ptr() as u64,
    ) != 0
    {
        fail(268)
    }
    say("shader: a SPIR-V shader drew every pixel of this window");

    // The window stays. A program that exited would give it back, and the
    // picture would go with it before anybody could look.
    loop {
        let _ = syscall3(
            call::IOCTL,
            display as u64,
            u64::from(DISPLAY_PRESENT),
            rectangle.as_ptr() as u64,
        );
        sleep_briefly();
    }
}

/// Four floats as a pixel: eight bits each, blue in the low byte.
///
/// Clamped, because a shader is free to write any number at all and a colour
/// channel is a byte. A shader that wrote 4.0 and was not clamped would wrap
/// round to something dark, which looks like a bug in the shader rather than in
/// the code that packed it.
fn pack(parts: &[f32]) -> u32 {
    let channel = |index: usize| -> u32 {
        let value = parts.get(index).copied().unwrap_or(0.0);
        // `clamp` rather than two comparisons, and it carries a not-a-number
        // straight through -- which then casts to zero below, so a shader that
        // divided by zero paints black rather than something enormous.
        let clamped = value.clamp(0.0, 1.0);
        (clamped * 255.0 + 0.5) as u32
    };
    (channel(0) << 16) | (channel(1) << 8) | channel(2)
}

/// Assemble the module described at the top of this file.
///
/// Written out in the order SPIR-V requires -- capabilities, entry point,
/// decorations, types, constants, and only then the function -- because a
/// consumer reads the stream once and everything must be declared before it is
/// used.
fn assemble() -> Shader {
    /// Capability `Shader`; `Logical` addressing with the `GLSL450` memory
    /// model; execution model `Fragment`; and the origin every window-space
    /// shader declares.
    const SHADER: u32 = 1;
    const LOGICAL: u32 = 0;
    const GLSL450: u32 = 1;
    const FRAGMENT: u32 = 4;
    const ORIGIN_UPPER_LEFT: u32 = 7;
    const NO_CONTROL: u32 = 0;

    let mut builder = Builder::new();
    let glsl_set = builder.id();
    let void = builder.id();
    let signature = builder.id();
    let float = builder.id();
    let v2 = builder.id();
    let v4 = builder.id();
    let pointer_in = builder.id();
    let pointer_out = builder.id();
    let frag_coord = builder.id();
    let colour_out = builder.id();
    let main = builder.id();

    builder.op(op::CAPABILITY, &[SHADER]);
    builder.op(
        op::EXT_INST_IMPORT,
        &joined(&[&[glsl_set], &text(glsl::NAME)]),
    );
    builder.op(op::MEMORY_MODEL, &[LOGICAL, GLSL450]);
    builder.op(
        op::ENTRY_POINT,
        &joined(&[&[FRAGMENT, main], &text("main"), &[frag_coord, colour_out]]),
    );
    builder.op(op::EXECUTION_MODE, &[main, ORIGIN_UPPER_LEFT]);
    builder.op(
        op::DECORATE,
        &[frag_coord, decoration::BUILT_IN, built_in::FRAG_COORD],
    );
    builder.op(op::DECORATE, &[colour_out, decoration::LOCATION, 0]);

    builder.op(op::TYPE_VOID, &[void]);
    builder.op(op::TYPE_FUNCTION, &[signature, void]);
    builder.op(op::TYPE_FLOAT, &[float, 32]);
    builder.op(op::TYPE_VECTOR, &[v2, float, 2]);
    builder.op(op::TYPE_VECTOR, &[v4, float, 4]);
    builder.op(op::TYPE_POINTER, &[pointer_in, storage::INPUT, v4]);
    builder.op(op::TYPE_POINTER, &[pointer_out, storage::OUTPUT, v4]);
    builder.op(op::VARIABLE, &[pointer_in, frag_coord, storage::INPUT]);
    builder.op(op::VARIABLE, &[pointer_out, colour_out, storage::OUTPUT]);

    // The constants, all at module scope, where SPIR-V puts every one of them.
    let constant = |builder: &mut Builder, value: f32| {
        let id = builder.id();
        builder.op(op::CONSTANT, &[float, id, value.to_bits()]);
        id
    };
    let grid_width = constant(&mut builder, GRID_WIDTH as f32);
    let grid_height = constant(&mut builder, GRID_HEIGHT as f32);
    let half = constant(&mut builder, 0.5);
    let zero = constant(&mut builder, 0.0);
    let one = constant(&mut builder, 1.0);
    let spread = constant(&mut builder, 2.4);
    let red_mix = constant(&mut builder, 0.25);
    let green_mix = constant(&mut builder, 0.35);
    let blue_mix = constant(&mut builder, 0.2);

    // ---- the body ---------------------------------------------------------
    let label = builder.id();
    let position = builder.id();
    let across = builder.id();
    let down = builder.id();
    let u = builder.id();
    let v = builder.id();
    let centred_u = builder.id();
    let centred_v = builder.id();
    let centred = builder.id();
    let distance = builder.id();
    let scaled = builder.id();
    let clamped = builder.id();
    let ring = builder.id();
    let red_part = builder.id();
    let red = builder.id();
    let green_part = builder.id();
    let green_base = builder.id();
    let green_lift = builder.id();
    let green = builder.id();
    let colour = builder.id();

    builder.op(op::FUNCTION, &[void, main, NO_CONTROL, signature]);
    builder.op(op::LABEL, &[label]);

    // `vec2 uv = gl_FragCoord.xy / vec2(width, height);`
    builder.op(op::LOAD, &[v4, position, frag_coord]);
    builder.op(op::COMPOSITE_EXTRACT, &[float, across, position, 0]);
    builder.op(op::COMPOSITE_EXTRACT, &[float, down, position, 1]);
    builder.op(op::F_DIV, &[float, u, across, grid_width]);
    builder.op(op::F_DIV, &[float, v, down, grid_height]);

    // `float ring = 1.0 - clamp(length(uv - vec2(0.5)) * 2.4, 0.0, 1.0);`
    builder.op(op::F_SUB, &[float, centred_u, u, half]);
    builder.op(op::F_SUB, &[float, centred_v, v, half]);
    builder.op(
        op::COMPOSITE_CONSTRUCT,
        &[v2, centred, centred_u, centred_v],
    );
    builder.op(
        op::EXT_INST,
        &[float, distance, glsl_set, glsl::LENGTH, centred],
    );
    builder.op(op::F_MUL, &[float, scaled, distance, spread]);
    builder.op(
        op::EXT_INST,
        &[float, clamped, glsl_set, glsl::F_CLAMP, scaled, zero, one],
    );
    builder.op(op::F_SUB, &[float, ring, one, clamped]);

    // `colour = vec4(u * 0.25 + ring, v * 0.35 + ring * 0.2, ring, 1.0);`
    builder.op(op::F_MUL, &[float, red_part, u, red_mix]);
    builder.op(op::F_ADD, &[float, red, red_part, ring]);
    builder.op(op::F_MUL, &[float, green_part, v, green_mix]);
    builder.op(op::F_MUL, &[float, green_lift, ring, blue_mix]);
    builder.op(op::F_ADD, &[float, green_base, green_part, green_lift]);
    builder.op(op::F_ADD, &[float, green, green_base, zero]);
    builder.op(
        op::COMPOSITE_CONSTRUCT,
        &[v4, colour, red, green, ring, one],
    );
    builder.op(op::STORE, &[colour_out, colour]);
    builder.op(op::RETURN, &[]);
    builder.op(op::FUNCTION_END, &[]);

    Shader {
        words: builder.finish(),
    }
}

/// Say what went wrong, by name.
///
/// A number would do, but the errors this reader produces are the kind somebody
/// has to act on -- "instruction 144 is not implemented" is a thing to go and
/// add, and "the module is not SPIR-V" is a thing to go and look at.
fn report(before: &str, why: nexus_spirv::Error) {
    use nexus_spirv::Error;
    let mut line = nexus_guest::fmt::Line::new();
    line.text(before);
    match why {
        Error::NoHeader => line.text("it is shorter than a header"),
        Error::NotSpirv(word) => line
            .text("its first word is not the magic, it is ")
            .number(i64::from(word)),
        Error::WrongEndian => line.text("it is byte-swapped"),
        Error::BadLength { at, words } => line
            .text("an instruction at word ")
            .number(at as i64)
            .text(" says it is ")
            .number(i64::from(words))
            .text(" words long"),
        Error::TooFewOperands {
            opcode,
            wanted,
            had,
        } => line
            .text("instruction ")
            .number(i64::from(opcode))
            .text(" wanted ")
            .number(wanted as i64)
            .text(" operands and had ")
            .number(had as i64),
        Error::UnterminatedString => line.text("a string runs off the end of its instruction"),
        Error::BadId(id) => line
            .text("identifier ")
            .number(i64::from(id))
            .text(" is out of range"),
        Error::Undefined(id) => line
            .text("identifier ")
            .number(i64::from(id))
            .text(" was used before it was defined"),
        Error::NotAType(id) => line
            .text("identifier ")
            .number(i64::from(id))
            .text(" is not a type"),
        Error::Unimplemented(opcode) => line
            .text("instruction ")
            .number(i64::from(opcode))
            .text(" is not implemented here"),
        Error::UnimplementedExtended(which) => line
            .text("GLSL.std.450 function ")
            .number(i64::from(which))
            .text(" is not implemented here"),
        Error::NoEntryPoint => line.text("it has no entry point, or more than one"),
        Error::WrongShape => line.text("an operation was given a value of the wrong width"),
        Error::MissingInput { location, .. } => line
            .text("nothing supplied the input at location ")
            .number(i64::from(location.unwrap_or(0))),
        Error::TooLong => line.text("it ran too long without returning"),
    };
    line.say();
}

/// A tenth of a second.
fn sleep_briefly() {
    let word: u32 = 0;
    let timeout: [u64; 2] = [0, 100_000_000];
    let _ = nexus_guest::syscall4(
        call::FUTEX,
        core::ptr::addr_of!(word) as u64,
        128, // FUTEX_WAIT_PRIVATE
        0,
        timeout.as_ptr() as u64,
    );
}
