//! Running a shader: one invocation, on the processor that asked.
//!
//! This is an interpreter, not a compiler. Every instruction is looked at each
//! time it is reached, which for a screen's worth of fragments is thousands of
//! times slower than the code a real driver would have generated. That is the
//! honest shape of it: `lavapipe` gets its speed from LLVM, and the point here
//! is that a shader somebody else compiled produces the right colour, not that
//! it does so quickly.
//!
//! # What an invocation is
//!
//! A shader runs once per fragment, or once per vertex. It is given its
//! `Input` variables, it writes its `Output` variables, and the two are
//! addressed by `Location` — a slot number the pipeline matches up — or by
//! `BuiltIn`, for the ones the system provides rather than the pipeline.
//!
//! There is no state between invocations. That is not a simplification: it is
//! what a shader *is*, and it is the reason a graphics processor can run
//! thousands of them at once.
//!
//! # Blocks
//!
//! A function body is a sequence of blocks, each starting at an `OpLabel` and
//! ending in a branch or a return. Control flow is by identifier, not by
//! offset, so this builds a map from label to position and jumps through it.
//!
//! `OpPhi` is the part that looks strange and is not: in single-assignment form
//! a value that depends on which way control came needs to say so, and `OpPhi`
//! lists a value for each block it could have come from. So the interpreter has
//! to remember which block it was in a moment ago, which is the one piece of
//! history it keeps.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::module::{ExecutionModel, Module, Step, Type};
use crate::{glsl, op, storage, Error, Id};

/// How many instructions one invocation may execute.
///
/// A shader is not allowed to take unbounded time here. This interpreter runs
/// inside whatever called it -- a compositor, a test -- and a loop whose
/// condition never goes false would be that program hanging with no way out.
/// A real driver has the same problem and solves it with a watchdog that resets
/// the device; this is the same idea with a smaller hammer.
pub const MOST_STEPS: usize = 100_000;

/// A value a shader computes.
///
/// Floats are `f32` because that is what `OpTypeFloat 32` is, which is what
/// essentially every shader uses. Doubles need a capability most do not
/// declare, and are not here.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i32),
    Float(f32),
    /// A vector, whose parts are all the same kind.
    Vector(Vec<Value>),
    /// Somewhere a value lives: a variable, and a path of constant indices into
    /// it. SPIR-V's `Logical` addressing model has no pointer arithmetic, so a
    /// pointer is exactly this and can never be anything else -- which is why
    /// a shader cannot read memory it was not given.
    Pointer(Id, Vec<u32>),
    Void,
}

impl Value {
    /// A four-wide vector of floats, which is what a colour is.
    #[must_use]
    pub fn vec4(x: f32, y: f32, z: f32, w: f32) -> Self {
        Value::Vector(vec![
            Value::Float(x),
            Value::Float(y),
            Value::Float(z),
            Value::Float(w),
        ])
    }

    /// A two-wide vector of floats.
    #[must_use]
    pub fn vec2(x: f32, y: f32) -> Self {
        Value::Vector(vec![Value::Float(x), Value::Float(y)])
    }

    /// The float in a scalar, or the error that says it was not one.
    pub fn as_float(&self) -> Result<f32, Error> {
        match self {
            Value::Float(value) => Ok(*value),
            Value::Int(value) => Ok(*value as f32),
            _ => Err(Error::WrongShape),
        }
    }

    /// The truth of a scalar boolean.
    pub fn as_bool(&self) -> Result<bool, Error> {
        match self {
            Value::Bool(value) => Ok(*value),
            _ => Err(Error::WrongShape),
        }
    }

    /// The parts of a vector, or the one part a scalar is.
    ///
    /// Scalars answer as a vector of one so that the arithmetic below can be
    /// written once. A shader that adds a scalar to a vector is a different
    /// thing and is refused: GLSL allows it, SPIR-V does not, and the compiler
    /// will have emitted an `OpCompositeConstruct` to widen it.
    pub fn parts(&self) -> Result<&[Value], Error> {
        match self {
            Value::Vector(parts) => Ok(parts),
            Value::Float(_) | Value::Int(_) | Value::Bool(_) => Ok(core::slice::from_ref(self)),
            _ => Err(Error::WrongShape),
        }
    }

    /// The floats of a vector, in order.
    pub fn floats(&self) -> Result<Vec<f32>, Error> {
        self.parts()?.iter().map(Value::as_float).collect()
    }
}

/// What a shader is given.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    by_location: BTreeMap<u32, Value>,
    by_built_in: BTreeMap<u32, Value>,
}

impl Inputs {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The value in a `Location` slot.
    #[must_use]
    pub fn at(mut self, location: u32, value: Value) -> Self {
        self.by_location.insert(location, value);
        self
    }

    /// A built-in: `FragCoord`, `VertexIndex`, and so on.
    #[must_use]
    pub fn built_in(mut self, which: u32, value: Value) -> Self {
        self.by_built_in.insert(which, value);
        self
    }
}

/// What it wrote.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outputs {
    pub by_location: BTreeMap<u32, Value>,
    pub by_built_in: BTreeMap<u32, Value>,
}

impl Outputs {
    /// The value in a `Location` slot. For a fragment shader, slot zero is the
    /// colour.
    #[must_use]
    pub fn at(&self, location: u32) -> Option<&Value> {
        self.by_location.get(&location)
    }

    /// A built-in the shader wrote: `Position`, for a vertex shader.
    #[must_use]
    pub fn built_in(&self, which: u32) -> Option<&Value> {
        self.by_built_in.get(&which)
    }
}

/// Run one invocation of a module's entry point.
pub fn run(module: &Module, inputs: &Inputs) -> Result<Outputs, Error> {
    if !matches!(
        module.entry.model,
        ExecutionModel::Fragment | ExecutionModel::Vertex
    ) {
        // Compute shaders need workgroups, shared memory and barriers, none of
        // which is here. Said rather than attempted.
        return Err(Error::Unimplemented(op::ENTRY_POINT));
    }

    // ---- the variables ----------------------------------------------------
    //
    // Memory, such as it is: one slot per variable. `Input`s start with what
    // the caller supplied, `Output`s and locals start empty and are filled in
    // by stores.
    let mut memory: BTreeMap<Id, Value> = BTreeMap::new();
    for variable in module.variables.values() {
        if variable.storage != storage::INPUT {
            continue;
        }
        let supplied = variable
            .decoration
            .location
            .and_then(|slot| inputs.by_location.get(&slot))
            .or_else(|| {
                variable
                    .decoration
                    .built_in
                    .and_then(|which| inputs.by_built_in.get(&which))
            });
        match supplied {
            Some(value) => {
                memory.insert(variable.id, value.clone());
            }
            None => {
                // Only for the ones the entry point actually names. A module
                // may declare variables it does not use, and demanding those
                // would be demanding values that change nothing.
                if module.entry.interface.contains(&variable.id) {
                    return Err(Error::MissingInput {
                        location: variable.decoration.location,
                        builtin: variable.decoration.built_in,
                    });
                }
            }
        }
    }

    // ---- where the blocks are ---------------------------------------------
    let mut labels: BTreeMap<Id, usize> = BTreeMap::new();
    for (at, step) in module.body.iter().enumerate() {
        if step.opcode == op::LABEL {
            labels.insert(step.operand(0)?, at);
        }
    }

    let mut values: BTreeMap<Id, Value> = BTreeMap::new();
    let mut at = 0usize;
    let mut came_from: Id = 0;
    let mut here: Id = 0;
    let mut steps = 0usize;

    while at < module.body.len() {
        steps += 1;
        if steps > MOST_STEPS {
            return Err(Error::TooLong);
        }
        let step = &module.body[at];
        match step.opcode {
            op::LABEL => {
                came_from = here;
                here = step.operand(0)?;
                at += 1;
            }
            op::RETURN | op::UNREACHABLE => break,
            op::BRANCH => {
                let target = step.operand(0)?;
                at = *labels.get(&target).ok_or(Error::Undefined(target))?;
            }
            op::BRANCH_CONDITIONAL => {
                let condition = resolve(module, &values, step.operand(0)?)?.as_bool()?;
                let target = step.operand(if condition { 1 } else { 2 })?;
                at = *labels.get(&target).ok_or(Error::Undefined(target))?;
            }
            // Hints about where control flow comes back together. A compiler
            // needs them; an interpreter that follows the branches does not.
            op::SELECTION_MERGE | op::LOOP_MERGE | op::NOP | op::LINE | op::NAME => at += 1,
            _ => {
                perform(module, step, &mut values, &mut memory, came_from)?;
                at += 1;
            }
        }
    }

    // ---- what it wrote ----------------------------------------------------
    let mut outputs = Outputs::default();
    for variable in module.variables.values() {
        if variable.storage != storage::OUTPUT {
            continue;
        }
        let Some(value) = memory.get(&variable.id) else {
            // Written by nothing. Left out rather than defaulted: a fragment
            // shader that did not write its colour has a bug, and a black
            // pixel would hide it.
            continue;
        };
        if let Some(slot) = variable.decoration.location {
            outputs.by_location.insert(slot, value.clone());
        }
        if let Some(which) = variable.decoration.built_in {
            outputs.by_built_in.insert(which, value.clone());
        }
    }
    Ok(outputs)
}

/// One instruction that produces or stores a value.
fn perform(
    module: &Module,
    step: &Step,
    values: &mut BTreeMap<Id, Value>,
    memory: &mut BTreeMap<Id, Value>,
    came_from: Id,
) -> Result<(), Error> {
    match step.opcode {
        op::STORE => {
            let pointer = resolve(module, values, step.operand(0)?)?;
            let object = resolve(module, values, step.operand(1)?)?;
            let Value::Pointer(base, path) = pointer else {
                return Err(Error::WrongShape);
            };
            store_into(memory, base, &path, object)?;
        }
        op::LOAD => {
            let result = step.operand(1)?;
            let pointer = resolve(module, values, step.operand(2)?)?;
            let Value::Pointer(base, path) = pointer else {
                return Err(Error::WrongShape);
            };
            let value = memory.get(&base).cloned().ok_or(Error::Undefined(base))?;
            values.insert(result, follow(&value, &path)?);
        }
        op::ACCESS_CHAIN => {
            let result = step.operand(1)?;
            let base = resolve(module, values, step.operand(2)?)?;
            let Value::Pointer(id, mut path) = base else {
                return Err(Error::WrongShape);
            };
            for index in &step.operands[3..] {
                // Only constant indices. A variable index into a vector needs
                // `OpVectorExtractDynamic`, which a compiler emits instead --
                // so a non-constant here is a module this crate has not seen,
                // and guessing at it would be worse than saying so.
                let value = resolve(module, values, *index)?;
                let Value::Int(which) = value else {
                    return Err(Error::WrongShape);
                };
                path.push(which as u32);
            }
            values.insert(result, Value::Pointer(id, path));
        }
        op::PHI => {
            let result = step.operand(1)?;
            // Pairs of (value, the block it would have come from).
            let mut chosen = None;
            let mut pair = 2;
            while pair + 1 < step.operands.len() {
                if step.operands[pair + 1] == came_from {
                    chosen = Some(step.operands[pair]);
                    break;
                }
                pair += 2;
            }
            let from = chosen.ok_or(Error::Undefined(came_from))?;
            let value = resolve(module, values, from)?;
            values.insert(result, value);
        }
        op::COMPOSITE_CONSTRUCT => {
            let result = step.operand(1)?;
            let mut parts = Vec::new();
            for id in &step.operands[2..] {
                // A constituent may itself be a vector, and then its parts are
                // spliced in rather than nested: `vec4(a_vec2, 0.0, 1.0)` is
                // four floats, not two things and two floats.
                let value = resolve(module, values, *id)?;
                match value {
                    Value::Vector(inner) => parts.extend(inner),
                    scalar => parts.push(scalar),
                }
            }
            values.insert(result, Value::Vector(parts));
        }
        op::COMPOSITE_EXTRACT => {
            let result = step.operand(1)?;
            let composite = resolve(module, values, step.operand(2)?)?;
            let path: Vec<u32> = step.operands[3..].to_vec();
            values.insert(result, follow(&composite, &path)?);
        }
        op::VECTOR_SHUFFLE => {
            let result = step.operand(1)?;
            let first = resolve(module, values, step.operand(2)?)?;
            let second = resolve(module, values, step.operand(3)?)?;
            let first = first.parts()?.to_vec();
            let second = second.parts()?.to_vec();
            let mut parts = Vec::new();
            for which in &step.operands[4..] {
                let which = *which as usize;
                // 0xFFFFFFFF means "undefined" in a shuffle. Zero is as good an
                // answer as any and better than refusing a valid module.
                let value = if which == 0xFFFF_FFFF {
                    Value::Float(0.0)
                } else if which < first.len() {
                    first[which].clone()
                } else {
                    second
                        .get(which - first.len())
                        .cloned()
                        .ok_or(Error::WrongShape)?
                };
                parts.push(value);
            }
            values.insert(result, Value::Vector(parts));
        }
        op::VECTOR_TIMES_SCALAR => {
            let result = step.operand(1)?;
            let vector = resolve(module, values, step.operand(2)?)?;
            let scalar = resolve(module, values, step.operand(3)?)?.as_float()?;
            let parts = vector
                .floats()?
                .into_iter()
                .map(|each| Value::Float(each * scalar))
                .collect();
            values.insert(result, Value::Vector(parts));
        }
        op::DOT => {
            let result = step.operand(1)?;
            let first = resolve(module, values, step.operand(2)?)?.floats()?;
            let second = resolve(module, values, step.operand(3)?)?.floats()?;
            if first.len() != second.len() {
                return Err(Error::WrongShape);
            }
            let total = first
                .iter()
                .zip(second.iter())
                .fold(0.0f32, |sum, (a, b)| sum + a * b);
            values.insert(result, Value::Float(total));
        }
        op::F_NEGATE => {
            let result = step.operand(1)?;
            let value = resolve(module, values, step.operand(2)?)?;
            values.insert(result, map_one(&value, |each| -each)?);
        }
        op::F_ADD | op::F_SUB | op::F_MUL | op::F_DIV => {
            let result = step.operand(1)?;
            let first = resolve(module, values, step.operand(2)?)?;
            let second = resolve(module, values, step.operand(3)?)?;
            let opcode = step.opcode;
            let value = map_two(&first, &second, move |a, b| match opcode {
                op::F_ADD => a + b,
                op::F_SUB => a - b,
                op::F_MUL => a * b,
                _ => a / b,
            })?;
            values.insert(result, value);
        }
        op::F_ORD_EQUAL
        | op::F_ORD_LESS_THAN
        | op::F_ORD_GREATER_THAN
        | op::F_ORD_LESS_THAN_EQUAL
        | op::F_ORD_GREATER_THAN_EQUAL => {
            let result = step.operand(1)?;
            let first = resolve(module, values, step.operand(2)?)?.as_float()?;
            let second = resolve(module, values, step.operand(3)?)?.as_float()?;
            let answer = match step.opcode {
                op::F_ORD_EQUAL => first == second,
                op::F_ORD_LESS_THAN => first < second,
                op::F_ORD_GREATER_THAN => first > second,
                op::F_ORD_LESS_THAN_EQUAL => first <= second,
                _ => first >= second,
            };
            values.insert(result, Value::Bool(answer));
        }
        op::LOGICAL_AND | op::LOGICAL_OR => {
            let result = step.operand(1)?;
            let first = resolve(module, values, step.operand(2)?)?.as_bool()?;
            let second = resolve(module, values, step.operand(3)?)?.as_bool()?;
            let answer = if step.opcode == op::LOGICAL_AND {
                first && second
            } else {
                first || second
            };
            values.insert(result, Value::Bool(answer));
        }
        op::LOGICAL_NOT => {
            let result = step.operand(1)?;
            let value = resolve(module, values, step.operand(2)?)?.as_bool()?;
            values.insert(result, Value::Bool(!value));
        }
        op::SELECT => {
            let result = step.operand(1)?;
            let condition = resolve(module, values, step.operand(2)?)?.as_bool()?;
            let chosen = step.operand(if condition { 3 } else { 4 })?;
            let value = resolve(module, values, chosen)?;
            values.insert(result, value);
        }
        op::CONVERT_S_TO_F => {
            let result = step.operand(1)?;
            let value = resolve(module, values, step.operand(2)?)?;
            let parts = value
                .parts()?
                .iter()
                .map(|each| match each {
                    Value::Int(whole) => Ok(Value::Float(*whole as f32)),
                    Value::Float(already) => Ok(Value::Float(*already)),
                    _ => Err(Error::WrongShape),
                })
                .collect::<Result<Vec<_>, _>>()?;
            values.insert(
                result,
                if parts.len() == 1 {
                    parts.into_iter().next().unwrap_or(Value::Void)
                } else {
                    Value::Vector(parts)
                },
            );
        }
        op::EXT_INST => extended(module, step, values)?,
        // A local `OpVariable` is a declaration, not an action: its slot exists
        // already because the module recorded it.
        op::VARIABLE | op::FUNCTION | op::FUNCTION_END => {}
        other => return Err(Error::Unimplemented(other)),
    }
    Ok(())
}

/// `GLSL.std.450`, which is where every shader's `clamp`, `mix` and `sqrt` come
/// from.
fn extended(module: &Module, step: &Step, values: &mut BTreeMap<Id, Value>) -> Result<(), Error> {
    let result = step.operand(1)?;
    let set = step.operand(2)?;
    if Some(set) != module.glsl_std {
        // An extended set this module imported under another name. Refused by
        // number rather than assumed to be GLSL's: the numbering is per set,
        // so guessing would run the wrong function.
        return Err(Error::UnimplementedExtended(step.operand(3)?));
    }
    let which = step.operand(3)?;
    let argument = |index: usize| -> Result<Value, Error> {
        resolve(module, values, step.operand(4 + index)?)
    };

    let value = match which {
        glsl::F_ABS => map_one(&argument(0)?, f32::abs)?,
        glsl::FLOOR => map_one(&argument(0)?, floor)?,
        glsl::FRACT => map_one(&argument(0)?, |each| each - floor(each))?,
        glsl::SQRT => map_one(&argument(0)?, square_root)?,
        glsl::F_MIN => map_two(
            &argument(0)?,
            &argument(1)?,
            |a, b| if a < b { a } else { b },
        )?,
        glsl::F_MAX => map_two(
            &argument(0)?,
            &argument(1)?,
            |a, b| if a > b { a } else { b },
        )?,
        glsl::F_CLAMP => {
            let low = argument(1)?;
            let high = argument(2)?;
            let above = map_two(&argument(0)?, &low, |a, b| if a > b { a } else { b })?;
            map_two(&above, &high, |a, b| if a < b { a } else { b })?
        }
        glsl::F_MIX => {
            // `x * (1 - a) + y * a`, component by component. Written out rather
            // than as two operations so that the interpolant may be a vector.
            let x = argument(0)?.floats()?;
            let y = argument(1)?.floats()?;
            let a = argument(2)?.floats()?;
            if x.len() != y.len() {
                return Err(Error::WrongShape);
            }
            let mut parts = Vec::with_capacity(x.len());
            for index in 0..x.len() {
                let amount = if a.len() == 1 {
                    a[0]
                } else {
                    *a.get(index).ok_or(Error::WrongShape)?
                };
                parts.push(Value::Float(x[index] * (1.0 - amount) + y[index] * amount));
            }
            widen(parts)
        }
        glsl::STEP => map_two(&argument(0)?, &argument(1)?, |edge, x| {
            if x < edge {
                0.0
            } else {
                1.0
            }
        })?,
        glsl::LENGTH => {
            let parts = argument(0)?.floats()?;
            Value::Float(square_root(
                parts.iter().fold(0.0, |sum, each| sum + each * each),
            ))
        }
        glsl::NORMALIZE => {
            let parts = argument(0)?.floats()?;
            let length = square_root(parts.iter().fold(0.0, |sum, each| sum + each * each));
            // A zero-length vector has no direction. Zero is returned rather
            // than a not-a-number, which is what the division would give and
            // what would then spread through everything downstream.
            let scale = if length == 0.0 { 0.0 } else { 1.0 / length };
            widen(
                parts
                    .into_iter()
                    .map(|each| Value::Float(each * scale))
                    .collect(),
            )
        }
        other => return Err(Error::UnimplementedExtended(other)),
    };
    values.insert(result, value);
    Ok(())
}

/// A list of parts as a value: a vector, or the scalar it is if there is one.
fn widen(mut parts: Vec<Value>) -> Value {
    if parts.len() == 1 {
        parts.pop().unwrap_or(Value::Void)
    } else {
        Value::Vector(parts)
    }
}

/// Apply a function to every float in a value.
fn map_one(value: &Value, what: impl Fn(f32) -> f32) -> Result<Value, Error> {
    let parts = value
        .floats()?
        .into_iter()
        .map(|each| Value::Float(what(each)))
        .collect();
    Ok(widen(parts))
}

/// Apply a function to two values, component by component.
///
/// A scalar on either side is spread across the other, because that is what
/// `OpFMul` of a vector and a scalar would mean if a compiler emitted one --
/// though in practice it emits `OpVectorTimesScalar` instead.
fn map_two(first: &Value, second: &Value, what: impl Fn(f32, f32) -> f32) -> Result<Value, Error> {
    let a = first.floats()?;
    let b = second.floats()?;
    let count = a.len().max(b.len());
    if a.len() != count && a.len() != 1 {
        return Err(Error::WrongShape);
    }
    if b.len() != count && b.len() != 1 {
        return Err(Error::WrongShape);
    }
    let mut parts = Vec::with_capacity(count);
    for index in 0..count {
        let left = if a.len() == 1 { a[0] } else { a[index] };
        let right = if b.len() == 1 { b[0] } else { b[index] };
        parts.push(Value::Float(what(left, right)));
    }
    Ok(widen(parts))
}

/// What an identifier stands for: a value computed earlier, a constant, or a
/// variable — which stands for a pointer to itself.
fn resolve(module: &Module, values: &BTreeMap<Id, Value>, id: Id) -> Result<Value, Error> {
    if let Some(value) = values.get(&id) {
        return Ok(value.clone());
    }
    if let Some(value) = module.constants.get(&id) {
        return Ok(value.clone());
    }
    if module.variables.contains_key(&id) {
        return Ok(Value::Pointer(id, Vec::new()));
    }
    Err(Error::Undefined(id))
}

/// Walk a path of indices into a value.
fn follow(value: &Value, path: &[u32]) -> Result<Value, Error> {
    let mut here = value;
    for index in path {
        let parts = match here {
            Value::Vector(parts) => parts,
            _ => return Err(Error::WrongShape),
        };
        here = parts.get(*index as usize).ok_or(Error::WrongShape)?;
    }
    Ok(here.clone())
}

/// Write a value at a path inside a variable.
fn store_into(
    memory: &mut BTreeMap<Id, Value>,
    base: Id,
    path: &[u32],
    value: Value,
) -> Result<(), Error> {
    if path.is_empty() {
        memory.insert(base, value);
        return Ok(());
    }
    // A store into part of a variable that has never been written needs
    // somewhere to put it. An empty vector grown to fit is the only sensible
    // answer, and it is what a shader that writes `colour.r` before `colour.g`
    // depends on.
    let slot = memory
        .entry(base)
        .or_insert_with(|| Value::Vector(Vec::new()));
    let mut here = slot;
    for index in path {
        let Value::Vector(parts) = here else {
            return Err(Error::WrongShape);
        };
        let index = *index as usize;
        while parts.len() <= index {
            parts.push(Value::Float(0.0));
        }
        here = &mut parts[index];
    }
    *here = value;
    Ok(())
}

/// A square root, because `core` has none.
///
/// `f32::sqrt` is in `std`, not `core`: it is one instruction on this processor
/// but a library call on others, so the standard library owns it and a
/// `#![no_std]` crate does not get it. This is the usual answer -- an initial
/// guess from halving the exponent in the bit pattern, then Newton's method,
/// which doubles the number of correct digits each time and is well inside a
/// float's precision after four rounds.
#[must_use]
pub fn square_root(value: f32) -> f32 {
    if value < 0.0 {
        return f32::NAN;
    }
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    // Halving the exponent: the bias makes this a shift and an add rather than
    // a division, and it lands within a factor of two of the answer.
    let mut guess = f32::from_bits((value.to_bits() >> 1) + (127 << 22));
    for _ in 0..4 {
        guess = 0.5 * (guess + value / guess);
    }
    guess
}

/// The largest whole number not greater than `value`, because `core` has no
/// `floor` either, and for the same reason.
#[must_use]
fn floor(value: f32) -> f32 {
    if !value.is_finite() {
        return value;
    }
    // Beyond this every float is already whole, and the cast below would
    // overflow.
    if value.abs() >= 2_147_483_648.0 {
        return value;
    }
    let truncated = value as i64 as f32;
    if value < 0.0 && truncated != value {
        truncated - 1.0
    } else {
        truncated
    }
}

/// Whether a type is one this interpreter has values for.
///
/// Used by callers that want to report "this shader wants a matrix" before
/// running it rather than after.
#[must_use]
pub fn is_supported(kind: &Type) -> bool {
    matches!(
        kind,
        Type::Void | Type::Bool | Type::Int { .. } | Type::Float { .. } | Type::Vector { .. }
    )
}
