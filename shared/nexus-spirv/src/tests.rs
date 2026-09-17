//! What a shader does, checked against what it should do.
//!
//! Every module here is assembled word by word next door. The test names are
//! sentences because a failure prints the name and nothing else.
//!
//! One thing the modules have in common is the *order*: capabilities, the entry
//! point, decorations, types, constants, and only then the function. SPIR-V
//! requires that, and it is not arbitrary — a consumer reads the stream once,
//! so everything an instruction refers to has to have been declared already.
//! Declaring a type inside a function would be invalid, and the assembler here
//! makes it awkward to do by accident.

extern crate alloc;

use alloc::vec::Vec;

use crate::assemble::{joined, text, Builder};
use crate::module::{ExecutionModel, Module, Type};
use crate::run::{run, Inputs, Value};
use crate::{built_in, decoration, glsl, op, storage, Error, MAGIC};

/// Capability `Shader`, which every graphics module declares.
const SHADER: u32 = 1;
/// `Logical` addressing, `GLSL450` memory model.
const LOGICAL: u32 = 0;
const GLSL450: u32 = 1;
/// Execution model `Fragment`, and the mode every one of them declares.
const FRAGMENT: u32 = 4;
const ORIGIN_UPPER_LEFT: u32 = 7;
/// `OpFunction`'s control word: none of the hints.
const NO_CONTROL: u32 = 0;

/// A module with one input, one `vec4` output, and a place to put a body.
///
/// The declarations every one of these tests needs, so that a test is the four
/// or five instructions it is actually about.
struct Frame {
    builder: Builder,
    glsl: u32,
    void: u32,
    signature: u32,
    boolean: u32,
    float: u32,
    v4: u32,
    v2: u32,
    input: u32,
    output: u32,
    main: u32,
}

impl Frame {
    /// Everything up to the constants: types, the two variables and their
    /// decorations. The input is `input_width` floats wide, and is addressed by
    /// `BuiltIn` if one is given and by `Location 0` otherwise.
    fn new(input_width: u32, input_built_in: Option<u32>) -> Self {
        let mut builder = Builder::new();
        let glsl = builder.id();
        let void = builder.id();
        let signature = builder.id();
        let boolean = builder.id();
        let float = builder.id();
        let v4 = builder.id();
        let v2 = builder.id();
        let pointer_in = builder.id();
        let pointer_out = builder.id();
        let input = builder.id();
        let output = builder.id();
        let main = builder.id();
        let input_type = if input_width == 2 { v2 } else { v4 };

        builder.op(op::CAPABILITY, &[SHADER]);
        builder.op(op::EXT_INST_IMPORT, &joined(&[&[glsl], &text(glsl::NAME)]));
        builder.op(op::MEMORY_MODEL, &[LOGICAL, GLSL450]);
        builder.op(
            op::ENTRY_POINT,
            &joined(&[&[FRAGMENT, main], &text("main"), &[input, output]]),
        );
        builder.op(op::EXECUTION_MODE, &[main, ORIGIN_UPPER_LEFT]);
        match input_built_in {
            Some(which) => builder.op(op::DECORATE, &[input, decoration::BUILT_IN, which]),
            None => builder.op(op::DECORATE, &[input, decoration::LOCATION, 0]),
        }
        builder.op(op::DECORATE, &[output, decoration::LOCATION, 0]);

        builder.op(op::TYPE_VOID, &[void]);
        builder.op(op::TYPE_FUNCTION, &[signature, void]);
        builder.op(op::TYPE_BOOL, &[boolean]);
        builder.op(op::TYPE_FLOAT, &[float, 32]);
        builder.op(op::TYPE_VECTOR, &[v4, float, 4]);
        builder.op(op::TYPE_VECTOR, &[v2, float, 2]);
        builder.op(op::TYPE_POINTER, &[pointer_in, storage::INPUT, input_type]);
        builder.op(op::TYPE_POINTER, &[pointer_out, storage::OUTPUT, v4]);
        builder.op(op::VARIABLE, &[pointer_in, input, storage::INPUT]);
        builder.op(op::VARIABLE, &[pointer_out, output, storage::OUTPUT]);

        Self {
            builder,
            glsl,
            void,
            signature,
            boolean,
            float,
            v4,
            v2,
            input,
            output,
            main,
        }
    }

    /// A `float` constant, at module scope where they all live.
    fn constant(&mut self, value: f32) -> u32 {
        let id = self.builder.id();
        self.builder
            .op(op::CONSTANT, &[self.float, id, value.to_bits()]);
        id
    }

    /// A constant vector, built out of constants already declared.
    fn composite(&mut self, kind: u32, parts: &[u32]) -> u32 {
        let id = self.builder.id();
        let mut operands = alloc::vec![kind, id];
        operands.extend_from_slice(parts);
        self.builder.op(op::CONSTANT_COMPOSITE, &operands);
        id
    }

    /// Open the function and its first block. Nothing declared after this is a
    /// declaration any more: it is code.
    fn begin(&mut self) -> u32 {
        let label = self.builder.id();
        self.builder.op(
            op::FUNCTION,
            &[self.void, self.main, NO_CONTROL, self.signature],
        );
        self.builder.op(op::LABEL, &[label]);
        label
    }

    fn id(&mut self) -> u32 {
        self.builder.id()
    }

    fn op(&mut self, opcode: u16, operands: &[u32]) {
        self.builder.op(opcode, operands);
    }

    fn finish(mut self) -> Vec<u32> {
        self.builder.op(op::RETURN, &[]);
        self.builder.op(op::FUNCTION_END, &[]);
        self.builder.finish()
    }
}

// ---- the header -----------------------------------------------------------

#[test]
fn a_module_that_is_not_spirv_is_refused_by_its_first_word() {
    let words = [0x1234_5678u32, 0, 0, 0, 0];
    assert_eq!(
        Module::parse(&words).unwrap_err(),
        Error::NotSpirv(0x1234_5678)
    );
}

#[test]
fn a_module_from_a_machine_of_the_other_endianness_is_named_as_such() {
    // Not "not SPIR-V": the magic is there, byte for byte reversed, and saying
    // so is the difference between a fixable problem and a mystery.
    let words = [MAGIC.swap_bytes(), 0, 0, 0, 0];
    assert_eq!(Module::parse(&words).unwrap_err(), Error::WrongEndian);
}

#[test]
fn a_module_shorter_than_its_header_is_refused() {
    assert_eq!(Module::parse(&[MAGIC, 0, 0]).unwrap_err(), Error::NoHeader);
}

#[test]
fn an_instruction_longer_than_what_is_left_stops_the_walk() {
    // A header, then an instruction claiming to be ten words long with one word
    // after it. A reader that believed it would read past the end.
    let words = [MAGIC, 0x0001_0000, 0, 4, 0, (10 << 16) | 17, 1];
    let mut walk = crate::instructions(&words);
    assert!(matches!(
        walk.next(),
        Some(Err(Error::BadLength { at: 5, words: 10 }))
    ));
    // And nothing after it: there is no way to find the next header in a stream
    // whose only framing is the lengths.
    assert!(walk.next().is_none());
}

#[test]
fn an_instruction_claiming_to_be_no_words_long_stops_the_walk() {
    // Zero would never advance, so a reader without this check loops for ever
    // on it rather than failing.
    let words = [MAGIC, 0x0001_0000, 0, 4, 0, 17];
    let mut walk = crate::instructions(&words);
    assert!(matches!(
        walk.next(),
        Some(Err(Error::BadLength { at: 5, words: 0 }))
    ));
}

// ---- strings, which are where the offsets go wrong ------------------------

#[test]
fn a_string_whose_length_is_a_multiple_of_four_takes_a_whole_extra_word() {
    // "main" is four bytes, so it occupies two words: one of characters and one
    // of terminator. Writing it in one is the bug that puts every operand after
    // a string one place out.
    assert_eq!(text("main").len(), 2);
    assert_eq!(text("main")[1], 0);
    assert_eq!(text("ab").len(), 1);
    assert_eq!(text("").len(), 1);
}

#[test]
fn the_entry_point_name_and_the_interface_after_it_are_both_read() {
    let mut frame = Frame::new(4, None);
    frame.begin();
    let words = frame.finish();
    let module = Module::parse(&words).expect("the module should read");
    assert_eq!(module.entry.name, "main");
    assert_eq!(module.entry.model, ExecutionModel::Fragment);
    // Two interface variables, and they are the ones declared -- which is only
    // true if the name's length was counted right.
    assert_eq!(module.entry.interface.len(), 2);
    assert_eq!(module.inputs().len(), 1);
    assert_eq!(module.outputs().len(), 1);
}

// ---- types and constants --------------------------------------------------

#[test]
fn a_float_constant_is_its_bits_and_not_its_number() {
    let mut frame = Frame::new(4, None);
    let half = frame.constant(0.5);
    frame.begin();
    let words = frame.finish();
    let module = Module::parse(&words).expect("the module should read");
    // Read as an integer, 0.5's encoding is 1,056,964,608. This is the check
    // that it was not.
    assert_eq!(module.constants.get(&half), Some(&Value::Float(0.5)));
}

#[test]
fn a_vector_type_says_what_it_is_made_of_and_how_many() {
    let mut frame = Frame::new(4, None);
    let (v4, float) = (frame.v4, frame.float);
    frame.begin();
    let words = frame.finish();
    let module = Module::parse(&words).expect("the module should read");
    assert_eq!(
        module.types.get(&v4),
        Some(&Type::Vector {
            component: float,
            count: 4
        })
    );
}

// ---- running one ----------------------------------------------------------

#[test]
fn a_shader_that_halves_its_input_halves_it() {
    let mut frame = Frame::new(4, None);
    let half = frame.constant(0.5);
    frame.begin();
    let (v4, input, output) = (frame.v4, frame.input, frame.output);
    let loaded = frame.id();
    let scaled = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::VECTOR_TIMES_SCALAR, &[v4, scaled, loaded, half]);
    frame.op(op::STORE, &[output, scaled]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    assert!(module.is_runnable());
    let outputs = run(
        &module,
        &Inputs::new().at(0, Value::vec4(1.0, 0.5, 0.25, 1.0)),
    )
    .expect("the shader should run");
    assert_eq!(outputs.at(0), Some(&Value::vec4(0.5, 0.25, 0.125, 0.5)));
}

#[test]
fn a_shader_can_take_its_input_apart_and_put_it_back_together() {
    // The swizzle every shader does: read `.x` and `.y`, compute, and build a
    // colour out of the results.
    let mut frame = Frame::new(4, None);
    let one = frame.constant(1.0);
    frame.begin();
    let (v4, float, input, output) = (frame.v4, frame.float, frame.input, frame.output);
    let loaded = frame.id();
    let red = frame.id();
    let green = frame.id();
    let sum = frame.id();
    let built = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, red, loaded, 0]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, green, loaded, 1]);
    frame.op(op::F_ADD, &[float, sum, red, green]);
    frame.op(op::COMPOSITE_CONSTRUCT, &[v4, built, sum, red, green, one]);
    frame.op(op::STORE, &[output, built]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    let outputs = run(
        &module,
        &Inputs::new().at(0, Value::vec4(0.25, 0.5, 0.0, 0.0)),
    )
    .expect("the shader should run");
    assert_eq!(outputs.at(0), Some(&Value::vec4(0.75, 0.25, 0.5, 1.0)));
}

#[test]
fn a_shader_reads_the_fragment_position_from_a_built_in() {
    // What every gradient is: divide `gl_FragCoord` by the size of the thing
    // being drawn. The input has no `Location` at all -- it is addressed by
    // `BuiltIn`, and a reader that only understood locations would report it
    // missing.
    let mut frame = Frame::new(4, Some(built_in::FRAG_COORD));
    let width = frame.constant(320.0);
    let one = frame.constant(1.0);
    frame.begin();
    let (v4, float, input, output) = (frame.v4, frame.float, frame.input, frame.output);
    let loaded = frame.id();
    let across = frame.id();
    let ratio = frame.id();
    let built = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, across, loaded, 0]);
    frame.op(op::F_DIV, &[float, ratio, across, width]);
    frame.op(
        op::COMPOSITE_CONSTRUCT,
        &[v4, built, ratio, ratio, ratio, one],
    );
    frame.op(op::STORE, &[output, built]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    let outputs = run(
        &module,
        &Inputs::new().built_in(built_in::FRAG_COORD, Value::vec4(160.0, 40.0, 0.0, 1.0)),
    )
    .expect("the shader should run");
    assert_eq!(outputs.at(0), Some(&Value::vec4(0.5, 0.5, 0.5, 1.0)));
}

#[test]
fn an_input_the_shader_needs_and_nobody_supplied_is_named_not_defaulted() {
    let mut frame = Frame::new(4, None);
    frame.begin();
    let (v4, input, output) = (frame.v4, frame.input, frame.output);
    let loaded = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::STORE, &[output, loaded]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    // Zero would be a black pixel and no complaint, which is the failure that
    // takes an afternoon to find.
    assert_eq!(
        run(&module, &Inputs::new()).unwrap_err(),
        Error::MissingInput {
            location: Some(0),
            builtin: None
        }
    );
}

// ---- the extended instruction set ----------------------------------------

#[test]
fn clamp_comes_from_the_set_the_module_imported() {
    let mut frame = Frame::new(4, None);
    let low = frame.constant(0.0);
    let high = frame.constant(1.0);
    frame.begin();
    let (v4, glsl_set, input, output) = (frame.v4, frame.glsl, frame.input, frame.output);
    let float = frame.float;
    let loaded = frame.id();
    let red = frame.id();
    let clamped = frame.id();
    let built = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, red, loaded, 0]);
    frame.op(
        op::EXT_INST,
        &[float, clamped, glsl_set, glsl::F_CLAMP, red, low, high],
    );
    frame.op(
        op::COMPOSITE_CONSTRUCT,
        &[v4, built, clamped, clamped, clamped, high],
    );
    frame.op(op::STORE, &[output, built]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    // Above the top, and below the bottom.
    let bright = run(
        &module,
        &Inputs::new().at(0, Value::vec4(4.0, 0.0, 0.0, 0.0)),
    )
    .expect("the shader should run");
    assert_eq!(bright.at(0), Some(&Value::vec4(1.0, 1.0, 1.0, 1.0)));
    let dark = run(
        &module,
        &Inputs::new().at(0, Value::vec4(-3.0, 0.0, 0.0, 0.0)),
    )
    .expect("the shader should run");
    assert_eq!(dark.at(0), Some(&Value::vec4(0.0, 0.0, 0.0, 1.0)));
}

#[test]
fn an_extended_instruction_that_is_not_implemented_is_reported_by_number() {
    // 13 is `Sin`, which needs a series this crate does not have. A shader
    // using it must fail loudly: silently returning its argument would draw
    // something plausible and wrong.
    const SIN: u32 = 13;
    let mut frame = Frame::new(4, None);
    frame.begin();
    let (v4, glsl_set, input, output) = (frame.v4, frame.glsl, frame.input, frame.output);
    let float = frame.float;
    let loaded = frame.id();
    let red = frame.id();
    let waved = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, red, loaded, 0]);
    frame.op(op::EXT_INST, &[float, waved, glsl_set, SIN, red]);
    frame.op(op::STORE, &[output, loaded]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    assert_eq!(
        run(
            &module,
            &Inputs::new().at(0, Value::vec4(1.0, 0.0, 0.0, 0.0))
        )
        .unwrap_err(),
        Error::UnimplementedExtended(SIN)
    );
}

// ---- control flow ---------------------------------------------------------

#[test]
fn a_branch_and_a_phi_choose_a_value_by_the_way_control_came() {
    // `colour = x < 0.5 ? red : green`, as a compiler emits it: two blocks that
    // branch to a third, and an `OpPhi` in the third saying which value each of
    // them contributes. Nothing else in single-assignment form can express it,
    // because there is only ever one assignment to a name.
    let mut frame = Frame::new(4, None);
    let half = frame.constant(0.5);
    let zero = frame.constant(0.0);
    let one = frame.constant(1.0);
    let v4 = frame.v4;
    let red = frame.composite(v4, &[one, zero, zero, one]);
    let green = frame.composite(v4, &[zero, one, zero, one]);
    frame.begin();
    let (float, boolean, input, output) = (frame.float, frame.boolean, frame.input, frame.output);

    let loaded = frame.id();
    let across = frame.id();
    let is_left = frame.id();
    let left_label = frame.id();
    let right_label = frame.id();
    let join_label = frame.id();
    let chosen = frame.id();

    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, across, loaded, 0]);
    frame.op(op::F_ORD_LESS_THAN, &[boolean, is_left, across, half]);
    frame.op(op::SELECTION_MERGE, &[join_label, 0]);
    frame.op(op::BRANCH_CONDITIONAL, &[is_left, left_label, right_label]);
    frame.op(op::LABEL, &[left_label]);
    frame.op(op::BRANCH, &[join_label]);
    frame.op(op::LABEL, &[right_label]);
    frame.op(op::BRANCH, &[join_label]);
    frame.op(op::LABEL, &[join_label]);
    frame.op(op::PHI, &[v4, chosen, red, left_label, green, right_label]);
    frame.op(op::STORE, &[output, chosen]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    let left = run(
        &module,
        &Inputs::new().at(0, Value::vec4(0.1, 0.0, 0.0, 0.0)),
    )
    .expect("the shader should run");
    assert_eq!(left.at(0), Some(&Value::vec4(1.0, 0.0, 0.0, 1.0)));
    let right = run(
        &module,
        &Inputs::new().at(0, Value::vec4(0.9, 0.0, 0.0, 0.0)),
    )
    .expect("the shader should run");
    assert_eq!(right.at(0), Some(&Value::vec4(0.0, 1.0, 0.0, 1.0)));
}

#[test]
fn a_shader_that_never_returns_is_stopped_rather_than_left_to_run() {
    let mut frame = Frame::new(4, None);
    frame.begin();
    let (v4, input, output) = (frame.v4, frame.input, frame.output);
    let again = frame.id();
    let loaded = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::STORE, &[output, loaded]);
    frame.op(op::BRANCH, &[again]);
    frame.op(op::LABEL, &[again]);
    // Straight back to itself, for ever.
    frame.op(op::BRANCH, &[again]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    assert_eq!(
        run(
            &module,
            &Inputs::new().at(0, Value::vec4(1.0, 1.0, 1.0, 1.0))
        )
        .unwrap_err(),
        Error::TooLong
    );
}

// ---- the instructions that are not here -----------------------------------

#[test]
fn an_instruction_that_is_not_implemented_is_reported_by_number() {
    // 144 is `OpVectorTimesMatrix`, which is not here because there are no
    // matrices here. The number is in the error so that adding it is a matter
    // of looking it up rather than guessing which instruction was reached.
    const VECTOR_TIMES_MATRIX: u16 = 144;
    let mut frame = Frame::new(4, None);
    frame.begin();
    let (v4, input, output) = (frame.v4, frame.input, frame.output);
    let loaded = frame.id();
    let product = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(VECTOR_TIMES_MATRIX, &[v4, product, loaded, loaded]);
    frame.op(op::STORE, &[output, loaded]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    assert_eq!(
        run(
            &module,
            &Inputs::new().at(0, Value::vec4(1.0, 0.0, 0.0, 0.0))
        )
        .unwrap_err(),
        Error::Unimplemented(VECTOR_TIMES_MATRIX)
    );
}

// ---- the arithmetic `core` does not have ----------------------------------

#[test]
fn the_square_root_this_crate_carries_agrees_with_the_numbers() {
    // `core` has no `sqrt`: it is one instruction on this processor and a
    // library call elsewhere, so the standard library owns it.
    for (value, expected) in [
        (0.0f32, 0.0f32),
        (1.0, 1.0),
        (4.0, 2.0),
        // The root of two, written as the constant rather than as digits.
        (2.0, core::f32::consts::SQRT_2),
        (10_000.0, 100.0),
    ] {
        let got = crate::run::square_root(value);
        assert!(
            (got - expected).abs() < 1e-4,
            "the square root of {value} came out as {got}, not {expected}"
        );
    }
    assert!(crate::run::square_root(-1.0).is_nan());
}

#[test]
fn normalising_a_vector_of_no_length_gives_no_direction_rather_than_a_nan() {
    let mut frame = Frame::new(4, None);
    frame.begin();
    let (v4, glsl_set, input, output) = (frame.v4, frame.glsl, frame.input, frame.output);
    let loaded = frame.id();
    let unit = frame.id();
    frame.op(op::LOAD, &[v4, loaded, input]);
    frame.op(op::EXT_INST, &[v4, unit, glsl_set, glsl::NORMALIZE, loaded]);
    frame.op(op::STORE, &[output, unit]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    let outputs = run(
        &module,
        &Inputs::new().at(0, Value::vec4(0.0, 0.0, 0.0, 0.0)),
    )
    .expect("the shader should run");
    // A not-a-number here would spread into every pixel downstream and show up
    // as a hole in the middle of a picture.
    assert_eq!(outputs.at(0), Some(&Value::vec4(0.0, 0.0, 0.0, 0.0)));

    let along = run(
        &module,
        &Inputs::new().at(0, Value::vec4(0.0, 3.0, 0.0, 4.0)),
    )
    .expect("the shader should run");
    let parts = along
        .at(0)
        .expect("it wrote a colour")
        .floats()
        .expect("floats");
    assert!((parts[1] - 0.6).abs() < 1e-5, "{parts:?}");
    assert!((parts[3] - 0.8).abs() < 1e-5, "{parts:?}");
}

#[test]
fn a_two_wide_input_is_read_as_two_and_not_as_four() {
    let mut frame = Frame::new(2, None);
    let one = frame.constant(1.0);
    frame.begin();
    let (v2, v4, float, input, output) =
        (frame.v2, frame.v4, frame.float, frame.input, frame.output);
    let loaded = frame.id();
    let across = frame.id();
    let down = frame.id();
    let built = frame.id();
    frame.op(op::LOAD, &[v2, loaded, input]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, across, loaded, 0]);
    frame.op(op::COMPOSITE_EXTRACT, &[float, down, loaded, 1]);
    frame.op(
        op::COMPOSITE_CONSTRUCT,
        &[v4, built, across, down, across, one],
    );
    frame.op(op::STORE, &[output, built]);
    let words = frame.finish();

    let module = Module::parse(&words).expect("the module should read");
    let outputs =
        run(&module, &Inputs::new().at(0, Value::vec2(0.25, 0.75))).expect("the shader should run");
    assert_eq!(outputs.at(0), Some(&Value::vec4(0.25, 0.75, 0.25, 1.0)));
}
