//! Walking the tree.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::parser::{Binary, Expression, Program, Statement, Unary};
use crate::value::Value;
use crate::Trouble;

/// How deep calls may go.
///
/// A bound rather than a stack overflow. This runs on a thread with a fixed
/// stack, and a program that recurses without an end would take the whole
/// machine down rather than report a mistake -- which is the difference between
/// a language and a hazard.
const DEEPEST: usize = 256;

/// How many steps a program may take.
///
/// The same argument. `while true { }` is a program somebody will write, and it
/// should end with a sentence rather than with a machine that has to be turned
/// off. Ten million is far more than any sensible program and a fraction of a
/// second of a loop that is going nowhere.
const MOST_STEPS: u64 = 10_000_000;

/// A function, as declared.
#[derive(Clone)]
struct Function {
    parameters: Vec<String>,
    body: Program,
}

/// Why evaluation of a block stopped.
enum Flow {
    /// It reached the end.
    Done,
    /// It hit a `return`.
    Return(Value),
}

/// A running program.
pub struct Interpreter {
    /// Variables, innermost scope last.
    ///
    /// A stack of maps and not one map: a function's parameters must not be
    /// visible to its caller, and a caller's variables must not be visible
    /// inside it -- which is why a call pushes a *fresh* scope rather than
    /// another layer on the same stack.
    scopes: Vec<BTreeMap<String, Value>>,
    functions: BTreeMap<String, Function>,
    output: Vec<String>,
    depth: usize,
    steps: u64,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// One with nothing defined.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scopes: alloc::vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            output: Vec::new(),
            depth: 0,
            steps: 0,
        }
    }

    /// Everything the program has printed, taking it.
    #[must_use]
    pub fn take_output(&mut self) -> Vec<String> {
        core::mem::take(&mut self.output)
    }

    /// Run a whole program.
    ///
    /// # Errors
    ///
    /// The first thing that went wrong, with its line.
    pub fn run(&mut self, program: &Program) -> Result<(), Trouble> {
        // Functions first, so a program may call one declared below the call.
        // Without this, a file has to be written bottom-up, which is a rule
        // about the reader rather than about the program.
        for statement in program {
            if let Statement::Function {
                name,
                parameters,
                body,
                ..
            } = statement
            {
                self.functions.insert(
                    name.clone(),
                    Function {
                        parameters: parameters.clone(),
                        body: body.clone(),
                    },
                );
            }
        }
        self.block(program)?;
        Ok(())
    }

    fn block(&mut self, program: &Program) -> Result<Flow, Trouble> {
        for statement in program {
            self.steps += 1;
            if self.steps > MOST_STEPS {
                return Err(Trouble::at(
                    line_of(statement),
                    "this program has run for too long; is a loop going nowhere?",
                ));
            }
            match statement {
                Statement::Let { name, value, .. } => {
                    let value = self.expression(value)?;
                    self.scopes
                        .last_mut()
                        .expect("there is always a scope")
                        .insert(name.clone(), value);
                }
                Statement::Assign { name, value, line } => {
                    let value = self.expression(value)?;
                    // Assignment finds an existing variable, innermost first,
                    // and refuses when there is none. `x = 1` on an undeclared
                    // name is a typo far more often than it is an intention,
                    // and a language that made a new variable there is one
                    // where a misspelling is silent.
                    let mut found = false;
                    for scope in self.scopes.iter_mut().rev() {
                        if let Some(slot) = scope.get_mut(name) {
                            *slot = value.clone();
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Err(Trouble::at(
                            *line,
                            alloc::format!("there is no `{name}` to assign to; did you mean `let`?"),
                        ));
                    }
                }
                Statement::If {
                    condition,
                    then,
                    otherwise,
                    line,
                } => {
                    let taken = self.condition(condition, *line)?;
                    // A fresh scope, so a `let` inside an `if` does not outlive
                    // it.
                    let flow = self.scoped(if taken { then } else { otherwise })?;
                    if let Flow::Return(value) = flow {
                        return Ok(Flow::Return(value));
                    }
                }
                Statement::While {
                    condition,
                    body,
                    line,
                } => loop {
                    self.steps += 1;
                    if self.steps > MOST_STEPS {
                        return Err(Trouble::at(
                            *line,
                            "this program has run for too long; is a loop going nowhere?",
                        ));
                    }
                    if !self.condition(condition, *line)? {
                        break;
                    }
                    if let Flow::Return(value) = self.scoped(body)? {
                        return Ok(Flow::Return(value));
                    }
                },
                // Already collected above.
                Statement::Function { .. } => {}
                Statement::Return { value, .. } => {
                    let value = match value {
                        Some(expression) => self.expression(expression)?,
                        None => Value::Nil,
                    };
                    return Ok(Flow::Return(value));
                }
                Statement::Do { value, .. } => {
                    self.expression(value)?;
                }
            }
        }
        Ok(Flow::Done)
    }

    /// Run a block in a scope of its own.
    fn scoped(&mut self, program: &Program) -> Result<Flow, Trouble> {
        self.scopes.push(BTreeMap::new());
        let flow = self.block(program);
        self.scopes.pop();
        flow
    }

    fn condition(&mut self, expression: &Expression, line: usize) -> Result<bool, Trouble> {
        let value = self.expression(expression)?;
        value.as_condition().ok_or_else(|| {
            Trouble::at(
                line,
                alloc::format!("a condition has to be true or false, this is {}", value.kind()),
            )
        })
    }

    fn look_up(&self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(value) = scope.get(name) {
                return Some(value.clone());
            }
        }
        None
    }

    fn expression(&mut self, expression: &Expression) -> Result<Value, Trouble> {
        match expression {
            Expression::Int(value) => Ok(Value::Int(*value)),
            Expression::Str(value) => Ok(Value::string(value.clone())),
            Expression::Bool(value) => Ok(Value::Bool(*value)),
            Expression::Nil => Ok(Value::Nil),
            Expression::Name(name) => self.look_up(name).ok_or_else(|| {
                Trouble::at(0, alloc::format!("there is nothing called `{name}`"))
            }),
            Expression::Unary { operator, of, line } => {
                let value = self.expression(of)?;
                match (operator, &value) {
                    (Unary::Negate, Value::Int(number)) => Ok(Value::Int(-number)),
                    (Unary::Not, Value::Bool(yes)) => Ok(Value::Bool(!yes)),
                    (Unary::Negate, other) => Err(Trouble::at(
                        *line,
                        alloc::format!("only a number can be negated, this is {}", other.kind()),
                    )),
                    (Unary::Not, other) => Err(Trouble::at(
                        *line,
                        alloc::format!("only a boolean can be `not`ed, this is {}", other.kind()),
                    )),
                }
            }
            Expression::Binary {
                operator,
                left,
                right,
                line,
            } => {
                // `and` and `or` stop early, which is why they are not in the
                // table below: `a and b` must not evaluate `b` when `a` is
                // false, or a guard like `x != nil and f(x)` would run `f`
                // exactly when it must not.
                if matches!(operator, Binary::And | Binary::Or) {
                    let first = self.condition(left, *line)?;
                    let stop = match operator {
                        Binary::And => !first,
                        _ => first,
                    };
                    if stop {
                        return Ok(Value::Bool(first));
                    }
                    return Ok(Value::Bool(self.condition(right, *line)?));
                }
                let left = self.expression(left)?;
                let right = self.expression(right)?;
                binary(*operator, &left, &right, *line)
            }
            Expression::Call {
                name,
                arguments,
                line,
            } => {
                let mut values = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    values.push(self.expression(argument)?);
                }
                self.call(name, values, *line)
            }
        }
    }

    fn call(&mut self, name: &str, arguments: Vec<Value>, line: usize) -> Result<Value, Trouble> {
        if let Some(value) = self.builtin(name, &arguments, line)? {
            return Ok(value);
        }

        let Some(function) = self.functions.get(name).cloned() else {
            return Err(Trouble::at(
                line,
                alloc::format!("there is no function called `{name}`"),
            ));
        };
        if arguments.len() != function.parameters.len() {
            return Err(Trouble::at(
                line,
                alloc::format!(
                    "`{name}` takes {} argument{}, given {}",
                    function.parameters.len(),
                    if function.parameters.len() == 1 { "" } else { "s" },
                    arguments.len()
                ),
            ));
        }

        self.depth += 1;
        if self.depth > DEEPEST {
            self.depth -= 1;
            return Err(Trouble::at(
                line,
                "calls are nested too deeply; is this recursion without an end?",
            ));
        }

        // A *fresh* stack of scopes, not another layer on the caller's: a
        // function must not see the caller's variables, and the caller must not
        // see its parameters afterwards.
        let caller = core::mem::replace(&mut self.scopes, alloc::vec![BTreeMap::new()]);
        for (parameter, value) in function.parameters.iter().zip(arguments) {
            self.scopes
                .last_mut()
                .expect("there is always a scope")
                .insert(parameter.clone(), value);
        }
        let flow = self.block(&function.body);
        self.scopes = caller;
        self.depth -= 1;

        match flow? {
            Flow::Return(value) => Ok(value),
            // A function that ran off the end gives nothing, which is what
            // `return` with no value gives too.
            Flow::Done => Ok(Value::Nil),
        }
    }

    /// The built-in functions, or `None` when the name is not one.
    fn builtin(
        &mut self,
        name: &str,
        arguments: &[Value],
        line: usize,
    ) -> Result<Option<Value>, Trouble> {
        let want = |how_many: usize| -> Result<(), Trouble> {
            if arguments.len() == how_many {
                Ok(())
            } else {
                Err(Trouble::at(
                    line,
                    alloc::format!(
                        "`{name}` takes {how_many} argument{}, given {}",
                        if how_many == 1 { "" } else { "s" },
                        arguments.len()
                    ),
                ))
            }
        };

        match name {
            "print" => {
                // Any number, joined by spaces, which is what makes
                // `print("x is", x)` read the way somebody writes it.
                let line: Vec<String> = arguments.iter().map(Value::show).collect();
                self.output.push(line.join(" "));
                Ok(Some(Value::Nil))
            }
            "len" => {
                want(1)?;
                match &arguments[0] {
                    // Characters and not bytes. A string with Japanese in it
                    // has a length somebody would recognise rather than the
                    // number of bytes it happens to take.
                    Value::Str(text) => Ok(Some(Value::Int(text.chars().count() as i64))),
                    other => Err(Trouble::at(
                        line,
                        alloc::format!("`len` wants a string, given {}", other.kind()),
                    )),
                }
            }
            "str" => {
                want(1)?;
                Ok(Some(Value::string(arguments[0].show())))
            }
            "int" => {
                want(1)?;
                match &arguments[0] {
                    Value::Int(value) => Ok(Some(Value::Int(*value))),
                    Value::Str(text) => match text.trim().parse::<i64>() {
                        Ok(value) => Ok(Some(Value::Int(value))),
                        // Refused rather than nought. A string that is not a
                        // number becoming nought is how a mistake turns into
                        // an answer.
                        Err(_) => Err(Trouble::at(
                            line,
                            alloc::format!("`{}` is not a number", text.as_ref()),
                        )),
                    },
                    other => Err(Trouble::at(
                        line,
                        alloc::format!("`int` cannot read {}", other.kind()),
                    )),
                }
            }
            _ => Ok(None),
        }
    }
}

/// The line a statement is on, for a message about it.
const fn line_of(statement: &Statement) -> usize {
    match statement {
        Statement::Let { line, .. }
        | Statement::Assign { line, .. }
        | Statement::If { line, .. }
        | Statement::While { line, .. }
        | Statement::Function { line, .. }
        | Statement::Return { line, .. }
        | Statement::Do { line, .. } => *line,
    }
}

/// One binary operator on two values.
fn binary(operator: Binary, left: &Value, right: &Value, line: usize) -> Result<Value, Trouble> {
    use Binary::{
        Add, Divide, Equal, Greater, GreaterOrEqual, Less, LessOrEqual, Multiply, NotEqual,
        Remainder, Subtract,
    };

    // Equality works on anything and never fails: two values of different kinds
    // are simply not equal. Every other operator wants particular kinds.
    match operator {
        Equal => return Ok(Value::Bool(left == right)),
        NotEqual => return Ok(Value::Bool(left != right)),
        _ => {}
    }

    // Adding strings is the one operation that is not about numbers, and it is
    // here because a language without it makes every message a call to a
    // function nobody wants to name.
    if let (Add, Value::Str(a), Value::Str(b)) = (operator, left, right) {
        let mut joined = String::with_capacity(a.len() + b.len());
        joined.push_str(a);
        joined.push_str(b);
        return Ok(Value::string(joined));
    }

    let (Value::Int(a), Value::Int(b)) = (left, right) else {
        return Err(Trouble::at(
            line,
            alloc::format!(
                "this needs two numbers, given {} and {}",
                left.kind(),
                right.kind()
            ),
        ));
    };
    let (a, b) = (*a, *b);

    Ok(match operator {
        Add => Value::Int(a.wrapping_add(b)),
        Subtract => Value::Int(a.wrapping_sub(b)),
        Multiply => Value::Int(a.wrapping_mul(b)),
        Divide | Remainder if b == 0 => {
            return Err(Trouble::at(line, "this divides by nothing"));
        }
        // `wrapping_div` for the one case that is not a division by zero and
        // still cannot be represented: the most negative number over minus one.
        Divide => Value::Int(a.wrapping_div(b)),
        Remainder => Value::Int(a.wrapping_rem(b)),
        Less => Value::Bool(a < b),
        LessOrEqual => Value::Bool(a <= b),
        Greater => Value::Bool(a > b),
        GreaterOrEqual => Value::Bool(a >= b),
        // Handled above.
        Equal | NotEqual | Binary::And | Binary::Or => unreachable!("handled before this"),
    })
}

#[cfg(test)]
mod tests {
    use crate::run;
    use alloc::string::ToString as _;
    use alloc::vec::Vec;

    fn printed(source: &str) -> Vec<alloc::string::String> {
        run(source).expect("should run")
    }

    #[test]
    fn it_prints() {
        assert_eq!(printed(r#"print("hello")"#), alloc::vec!["hello"]);
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(printed("print(1 + 2 * 3)"), alloc::vec!["7"]);
        assert_eq!(printed("print((1 + 2) * 3)"), alloc::vec!["9"]);
        assert_eq!(printed("print(7 % 3)"), alloc::vec!["1"]);
        assert_eq!(printed("print(-3 + 1)"), alloc::vec!["-2"]);
    }

    #[test]
    fn variables_and_assignment() {
        assert_eq!(
            printed("let x = 1\nx = x + 1\nprint(x)"),
            alloc::vec!["2"]
        );
    }

    /// A typo must not quietly make a new variable.
    #[test]
    fn assigning_to_something_undeclared_is_refused() {
        let trouble = run("x = 1").expect_err("there is no x");
        assert!(trouble.what.contains("let"), "{}", trouble.what);
    }

    #[test]
    fn conditions_and_branches() {
        assert_eq!(printed("if 1 < 2 { print(\"yes\") }"), alloc::vec!["yes"]);
        assert_eq!(
            printed("if 1 > 2 { print(\"yes\") } else { print(\"no\") }"),
            alloc::vec!["no"]
        );
        assert_eq!(
            printed("let x = 5\nif x < 3 { print(\"a\") } else if x < 9 { print(\"b\") } else { print(\"c\") }"),
            alloc::vec!["b"]
        );
    }

    /// The decision that surprises people, so it has a test that says so.
    #[test]
    fn a_number_is_not_a_condition() {
        assert!(run("if 1 { }").is_err());
    }

    #[test]
    fn while_loops() {
        assert_eq!(
            printed("let i = 0\nwhile i < 3 { print(i)\ni = i + 1 }"),
            alloc::vec!["0", "1", "2"]
        );
    }

    #[test]
    fn functions_and_recursion() {
        let source = "
fn fib(n) {
    if n < 2 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))
";
        assert_eq!(printed(source), alloc::vec!["55"]);
    }

    /// Declared below the call, because a file should not have to be written
    /// bottom-up.
    #[test]
    fn a_function_may_be_called_before_it_is_declared() {
        assert_eq!(printed("print(two())\nfn two() { return 2 }"), alloc::vec!["2"]);
    }

    /// A function must not see its caller's variables.
    #[test]
    fn scopes_do_not_leak_into_a_call() {
        let trouble = run("let secret = 1\nfn f() { return secret }\nprint(f())")
            .expect_err("`secret` is the caller's");
        assert!(trouble.what.contains("secret"), "{}", trouble.what);
    }

    /// And a `let` inside a block does not outlive it.
    #[test]
    fn a_block_has_its_own_scope() {
        assert!(run("if true { let x = 1 }\nprint(x)").is_err());
    }

    #[test]
    fn wrong_number_of_arguments_is_refused() {
        let trouble = run("fn f(a) { return a }\nprint(f(1, 2))").expect_err("f takes one");
        assert!(trouble.what.contains("takes 1 argument"), "{}", trouble.what);
    }

    #[test]
    fn strings_join_with_plus() {
        assert_eq!(
            printed(r#"print("a" + "b")"#),
            alloc::vec!["ab".to_string()]
        );
        // And a string plus a number is refused rather than guessed at.
        assert!(run(r#"print("a" + 1)"#).is_err());
    }

    #[test]
    fn the_builtins() {
        assert_eq!(printed(r#"print(len("hello"))"#), alloc::vec!["5"]);
        // Characters, not bytes.
        assert_eq!(printed(r#"print(len("日本語"))"#), alloc::vec!["3"]);
        assert_eq!(printed(r#"print(str(12) + "!")"#), alloc::vec!["12!"]);
        assert_eq!(printed(r#"print(int("42") + 1)"#), alloc::vec!["43"]);
        assert!(run(r#"print(int("no"))"#).is_err());
    }

    #[test]
    fn print_joins_its_arguments_with_spaces() {
        assert_eq!(printed(r#"print("x is", 1)"#), alloc::vec!["x is 1"]);
    }

    #[test]
    fn dividing_by_nothing_is_refused() {
        assert!(run("print(1 / 0)").is_err());
        assert!(run("print(1 % 0)").is_err());
    }

    /// `and` must not evaluate its right side when the left is false, or a
    /// guard is not a guard.
    #[test]
    fn and_stops_early() {
        // `boom` is not a function, so evaluating it would be an error.
        assert_eq!(printed("if false and boom() { }\nprint(\"fine\")"), alloc::vec!["fine"]);
        assert_eq!(printed("if true or boom() { print(\"fine\") }"), alloc::vec!["fine"]);
    }

    /// A loop going nowhere ends with a sentence, not with a machine that has
    /// to be turned off.
    #[test]
    fn a_program_that_never_stops_is_stopped() {
        let trouble = run("while true { }").expect_err("this never ends");
        assert!(trouble.what.contains("too long"), "{}", trouble.what);
    }

    /// And so does recursion without an end.
    #[test]
    fn endless_recursion_is_stopped() {
        let trouble = run("fn f() { return f() }\nprint(f())").expect_err("this never ends");
        assert!(trouble.what.contains("deeply"), "{}", trouble.what);
    }

    #[test]
    fn errors_carry_the_line() {
        let trouble = run("let a = 1\nlet b = 2\nprint(1 / 0)").expect_err("divides by nothing");
        assert_eq!(trouble.line, 3);
    }
}
