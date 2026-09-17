//! A module, read into something a walker can act on.
//!
//! The word stream is in the order a compiler emits it, which is very nearly
//! the order this needs: capabilities, then the entry point, then decorations,
//! then types and constants, then the functions. Nothing here reorders it. What
//! it does is put the declarations into maps by identifier, and keep each
//! function's body as a run of instructions to be walked later.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::{built_in, decoration, glsl, instructions, op, storage, Error, Id, MAGIC};

/// What kind of shader an entry point is.
///
/// Only the three that matter here. A module declares this per entry point, and
/// it decides what the built-ins mean: `FragCoord` in a vertex shader would be
/// a module that does not make sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionModel {
    Vertex,
    Fragment,
    Compute,
    /// Anything else in the specification. Kept rather than refused, so that a
    /// module can be *read* and reported on even when it cannot be run.
    Other(u32),
}

impl ExecutionModel {
    fn of(value: u32) -> Self {
        match value {
            0 => Self::Vertex,
            4 => Self::Fragment,
            5 => Self::Compute,
            other => Self::Other(other),
        }
    }
}

/// A type, as far as this crate needs one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Void,
    Bool,
    Int { width: u32, signed: bool },
    Float { width: u32 },
    Vector { component: Id, count: u32 },
    Pointer { storage: u32, pointee: Id },
    Function { result: Id },
}

/// How a variable is wired to the world outside the shader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decoration {
    pub location: Option<u32>,
    pub built_in: Option<u32>,
}

/// One `OpVariable`.
#[derive(Debug, Clone)]
pub struct Variable {
    pub id: Id,
    /// The *pointer* type, which is what `OpVariable` names. What the variable
    /// holds is that pointer's pointee — a distinction it is easy to lose, and
    /// losing it makes every `OpLoad` come back with the wrong shape.
    pub pointer_type: Id,
    pub storage: u32,
    pub decoration: Decoration,
}

/// The one entry point this crate runs.
#[derive(Debug, Clone)]
pub struct EntryPoint {
    pub model: ExecutionModel,
    pub function: Id,
    pub name: String,
    /// Every `Input` and `Output` the entry point uses. A module lists these
    /// explicitly, which is how a driver knows what to wire up without walking
    /// the code.
    pub interface: Vec<Id>,
}

/// One instruction, kept.
#[derive(Debug, Clone)]
pub struct Step {
    pub opcode: u16,
    pub operands: Vec<u32>,
}

impl Step {
    /// The `index`th operand, or an error naming what was short.
    pub fn operand(&self, index: usize) -> Result<u32, Error> {
        self.operands
            .get(index)
            .copied()
            .ok_or(Error::TooFewOperands {
                opcode: self.opcode,
                wanted: index + 1,
                had: self.operands.len(),
            })
    }
}

/// A module, read.
#[derive(Debug, Clone)]
pub struct Module {
    /// One more than the largest identifier used. Every identifier is checked
    /// against it, which is the cheapest guard there is against a module that
    /// was assembled wrong.
    pub bound: u32,
    pub types: BTreeMap<Id, Type>,
    /// `OpConstant` and `OpConstantComposite`, already turned into values.
    pub constants: BTreeMap<Id, crate::run::Value>,
    pub variables: BTreeMap<Id, Variable>,
    /// The type each result identifier has, for the instructions that declare
    /// one. What tells the interpreter whether an `OpLoad` produces a scalar or
    /// a four-wide vector.
    pub result_types: BTreeMap<Id, Id>,
    pub entry: EntryPoint,
    /// The identifier `OpExtInstImport` gave `GLSL.std.450`, if it was
    /// imported. `OpExtInst` names the set by this, so a module importing two
    /// sets is unambiguous.
    pub glsl_std: Option<Id>,
    /// The entry function's instructions, in order, from its first `OpLabel` to
    /// its `OpFunctionEnd`.
    pub body: Vec<Step>,
}

impl Module {
    /// Read a module out of its words.
    pub fn parse(words: &[u32]) -> Result<Self, Error> {
        crate::check_header(words)?;
        debug_assert_eq!(words[0], MAGIC);
        let bound = words[3];

        let mut types = BTreeMap::new();
        let mut constants = BTreeMap::new();
        let mut variables: BTreeMap<Id, Variable> = BTreeMap::new();
        let mut result_types = BTreeMap::new();
        let mut decorations: BTreeMap<Id, Decoration> = BTreeMap::new();
        let mut entry: Option<EntryPoint> = None;
        let mut glsl_std = None;

        // Function bodies, keyed by the identifier `OpFunction` produced. Every
        // one is kept rather than only the entry's: which function is the entry
        // point is stated in `OpEntryPoint`, and a compiler is free to emit
        // that after the functions.
        let mut bodies: BTreeMap<Id, Vec<Step>> = BTreeMap::new();
        let mut inside: Option<Id> = None;

        let check = |id: Id| -> Result<Id, Error> {
            if id == 0 || id >= bound {
                Err(Error::BadId(id))
            } else {
                Ok(id)
            }
        };

        for maybe in instructions(words) {
            let instruction = maybe?;
            let code = instruction.opcode;

            // Inside a function, everything but the end of it is body.
            if let Some(function) = inside {
                if code == op::FUNCTION_END {
                    inside = None;
                } else {
                    // `OpVariable` inside a function is a local, and is
                    // declared rather than executed -- but its type still has
                    // to be known, so it is recorded here as well as kept.
                    if code == op::VARIABLE {
                        let pointer_type = check(instruction.operand(0)?)?;
                        let id = check(instruction.operand(1)?)?;
                        result_types.insert(id, pointer_type);
                        variables.insert(
                            id,
                            Variable {
                                id,
                                pointer_type,
                                storage: instruction.operand(2)?,
                                decoration: Decoration::default(),
                            },
                        );
                    } else if produces_a_value(code) {
                        let kind = check(instruction.operand(0)?)?;
                        let id = check(instruction.operand(1)?)?;
                        result_types.insert(id, kind);
                    }
                    bodies.entry(function).or_default().push(Step {
                        opcode: code,
                        operands: instruction.operands.to_vec(),
                    });
                }
                continue;
            }

            match code {
                // Said and not acted on. A capability this crate cannot honour
                // shows up as an unimplemented instruction later, where it can
                // be named; refusing here would refuse modules that declare
                // `Shader` and use nothing exotic.
                op::NOP
                | op::CAPABILITY
                | op::EXTENSION
                | op::MEMORY_MODEL
                | op::EXECUTION_MODE
                | op::SOURCE
                | op::SOURCE_EXTENSION
                | op::NAME
                | op::MEMBER_NAME
                | op::STRING
                | op::LINE
                | op::MEMBER_DECORATE => {}

                op::EXT_INST_IMPORT => {
                    let id = check(instruction.operand(0)?)?;
                    let (name, _) = instruction.string(1)?;
                    if name == glsl::NAME {
                        glsl_std = Some(id);
                    }
                }

                op::ENTRY_POINT => {
                    if entry.is_some() {
                        // More than one. A real module may have several, and
                        // choosing between them needs a name from the caller --
                        // which is an interface decision, not a parsing one, and
                        // is not being guessed at here.
                        return Err(Error::NoEntryPoint);
                    }
                    let model = ExecutionModel::of(instruction.operand(0)?);
                    let function = check(instruction.operand(1)?)?;
                    let (name, words_used) = instruction.string(2)?;
                    let interface = instruction.operands[2 + words_used..]
                        .iter()
                        .map(|id| check(*id))
                        .collect::<Result<Vec<_>, _>>()?;
                    entry = Some(EntryPoint {
                        model,
                        function,
                        name,
                        interface,
                    });
                }

                op::DECORATE => {
                    let target = check(instruction.operand(0)?)?;
                    let what = instruction.operand(1)?;
                    let record = decorations.entry(target).or_default();
                    match what {
                        decoration::LOCATION => record.location = Some(instruction.operand(2)?),
                        decoration::BUILT_IN => record.built_in = Some(instruction.operand(2)?),
                        // Everything else: relaxed precision, flat, noperspective,
                        // and the rest. None of them changes what this computes.
                        _ => {}
                    }
                }

                op::TYPE_VOID => {
                    types.insert(check(instruction.operand(0)?)?, Type::Void);
                }
                op::TYPE_BOOL => {
                    types.insert(check(instruction.operand(0)?)?, Type::Bool);
                }
                op::TYPE_INT => {
                    let id = check(instruction.operand(0)?)?;
                    types.insert(
                        id,
                        Type::Int {
                            width: instruction.operand(1)?,
                            signed: instruction.operand(2)? != 0,
                        },
                    );
                }
                op::TYPE_FLOAT => {
                    let id = check(instruction.operand(0)?)?;
                    types.insert(
                        id,
                        Type::Float {
                            width: instruction.operand(1)?,
                        },
                    );
                }
                op::TYPE_VECTOR => {
                    let id = check(instruction.operand(0)?)?;
                    types.insert(
                        id,
                        Type::Vector {
                            component: check(instruction.operand(1)?)?,
                            count: instruction.operand(2)?,
                        },
                    );
                }
                op::TYPE_POINTER => {
                    let id = check(instruction.operand(0)?)?;
                    types.insert(
                        id,
                        Type::Pointer {
                            storage: instruction.operand(1)?,
                            pointee: check(instruction.operand(2)?)?,
                        },
                    );
                }
                op::TYPE_FUNCTION => {
                    let id = check(instruction.operand(0)?)?;
                    types.insert(
                        id,
                        Type::Function {
                            result: check(instruction.operand(1)?)?,
                        },
                    );
                }

                op::CONSTANT_TRUE | op::CONSTANT_FALSE => {
                    let kind = check(instruction.operand(0)?)?;
                    let id = check(instruction.operand(1)?)?;
                    result_types.insert(id, kind);
                    constants.insert(id, crate::run::Value::Bool(code == op::CONSTANT_TRUE));
                }
                op::CONSTANT => {
                    let kind = check(instruction.operand(0)?)?;
                    let id = check(instruction.operand(1)?)?;
                    let literal = instruction.operand(2)?;
                    result_types.insert(id, kind);
                    // The literal's *bits*, interpreted by the type. A
                    // thirty-two bit float constant is the IEEE encoding in one
                    // word, which is exactly what `from_bits` wants -- and
                    // reading it as an integer instead is a bug that produces
                    // enormous numbers rather than an error.
                    let value = match types.get(&kind) {
                        Some(Type::Float { .. }) => {
                            crate::run::Value::Float(f32::from_bits(literal))
                        }
                        Some(Type::Int { signed: true, .. }) => {
                            crate::run::Value::Int(literal as i32)
                        }
                        Some(Type::Int { .. }) => crate::run::Value::Int(literal as i32),
                        Some(_) => return Err(Error::NotAType(kind)),
                        None => return Err(Error::Undefined(kind)),
                    };
                    constants.insert(id, value);
                }
                op::CONSTANT_COMPOSITE => {
                    let kind = check(instruction.operand(0)?)?;
                    let id = check(instruction.operand(1)?)?;
                    result_types.insert(id, kind);
                    let mut parts = Vec::new();
                    for part in &instruction.operands[2..] {
                        let part = check(*part)?;
                        parts.push(
                            constants
                                .get(&part)
                                .cloned()
                                .ok_or(Error::Undefined(part))?,
                        );
                    }
                    constants.insert(id, crate::run::Value::Vector(parts));
                }

                op::VARIABLE => {
                    let pointer_type = check(instruction.operand(0)?)?;
                    let id = check(instruction.operand(1)?)?;
                    result_types.insert(id, pointer_type);
                    variables.insert(
                        id,
                        Variable {
                            id,
                            pointer_type,
                            storage: instruction.operand(2)?,
                            decoration: Decoration::default(),
                        },
                    );
                }

                op::FUNCTION => {
                    let id = check(instruction.operand(1)?)?;
                    bodies.insert(id, Vec::new());
                    inside = Some(id);
                }

                other => return Err(Error::Unimplemented(other)),
            }
        }

        let entry = entry.ok_or(Error::NoEntryPoint)?;
        for (id, what) in decorations {
            if let Some(variable) = variables.get_mut(&id) {
                variable.decoration = what;
            }
        }
        let body = bodies.remove(&entry.function).unwrap_or_default();

        Ok(Self {
            bound,
            types,
            constants,
            variables,
            result_types,
            entry,
            glsl_std,
            body,
        })
    }

    /// The type an identifier has, following one level of pointer.
    ///
    /// A variable's own type is a *pointer*; what `OpLoad` of it produces is
    /// the pointee. Everything that asks "what shape is this" wants the second.
    pub fn pointee_of(&self, id: Id) -> Result<&Type, Error> {
        let kind = self
            .result_types
            .get(&id)
            .copied()
            .ok_or(Error::Undefined(id))?;
        match self.types.get(&kind) {
            Some(Type::Pointer { pointee, .. }) => {
                self.types.get(pointee).ok_or(Error::Undefined(*pointee))
            }
            Some(other) => Ok(other),
            None => Err(Error::Undefined(kind)),
        }
    }

    /// How wide a type is: one for a scalar, `count` for a vector.
    pub fn width_of(&self, kind: &Type) -> u32 {
        match kind {
            Type::Vector { count, .. } => *count,
            _ => 1,
        }
    }

    /// The inputs the entry point reads, with how each is addressed.
    ///
    /// What a caller needs in order to supply them, taken from the module's own
    /// interface list rather than by walking the code.
    pub fn inputs(&self) -> Vec<&Variable> {
        self.interface_with(storage::INPUT)
    }

    /// The outputs the entry point writes.
    pub fn outputs(&self) -> Vec<&Variable> {
        self.interface_with(storage::OUTPUT)
    }

    fn interface_with(&self, want: u32) -> Vec<&Variable> {
        self.entry
            .interface
            .iter()
            .filter_map(|id| self.variables.get(id))
            .filter(|variable| variable.storage == want)
            .collect()
    }

    /// Whether this module is one [`crate::run`] will attempt.
    ///
    /// Said separately from running it, so that a caller can report "this is a
    /// compute shader and I only draw" rather than starting and failing.
    pub fn is_runnable(&self) -> bool {
        matches!(
            self.entry.model,
            ExecutionModel::Fragment | ExecutionModel::Vertex
        ) && !self.body.is_empty()
    }
}

/// Whether an instruction's first two operands are a result type and a result
/// identifier.
///
/// Most produce a value and are laid out that way; the ones that do not are the
/// stores, the branches and the labels. Getting this wrong records a *branch
/// target* as if it were a type, which then turns up much later as a type that
/// is not a type.
fn produces_a_value(opcode: u16) -> bool {
    !matches!(
        opcode,
        op::STORE
            | op::LABEL
            | op::BRANCH
            | op::BRANCH_CONDITIONAL
            | op::RETURN
            | op::UNREACHABLE
            | op::SELECTION_MERGE
            | op::LOOP_MERGE
            | op::NAME
            | op::LINE
            | op::NOP
    )
}

/// The built-ins this crate will supply, by name, for a message.
pub fn built_in_name(value: u32) -> &'static str {
    match value {
        built_in::POSITION => "Position",
        built_in::FRAG_COORD => "FragCoord",
        built_in::VERTEX_INDEX => "VertexIndex",
        _ => "an unnamed built-in",
    }
}
