//! Tokens to a tree.
//!
//! Precedence climbing: one function per level, each calling the one below it
//! and then looping while it sees an operator of its own level. It is the
//! shortest way to write a correct precedence table and the easiest to check,
//! because the table *is* the call order.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::lexer::{Spanned, Token};
use crate::Trouble;

/// What a program is.
pub type Program = Vec<Statement>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// `let name = value`
    Let {
        name: String,
        value: Expression,
        line: usize,
    },
    /// `name = value`
    Assign {
        name: String,
        value: Expression,
        line: usize,
    },
    /// `if condition { .. } else { .. }`
    If {
        condition: Expression,
        then: Program,
        otherwise: Program,
        line: usize,
    },
    While {
        condition: Expression,
        body: Program,
        line: usize,
    },
    /// `fn name(a, b) { .. }`
    Function {
        name: String,
        parameters: Vec<String>,
        body: Program,
        line: usize,
    },
    Return {
        value: Option<Expression>,
        line: usize,
    },
    /// An expression on its own, for its effect. A call, in practice.
    Do { value: Expression, line: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Int(i64),
    Str(String),
    Bool(bool),
    Nil,
    Name(String),
    Unary {
        operator: Unary,
        of: Box<Expression>,
        line: usize,
    },
    Binary {
        operator: Binary,
        left: Box<Expression>,
        right: Box<Expression>,
        line: usize,
    },
    Call {
        name: String,
        arguments: Vec<Expression>,
        line: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unary {
    Negate,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    And,
    Or,
}

/// Turn tokens into a program.
///
/// # Errors
///
/// The first thing that does not fit, with the line it was on.
pub fn parse(tokens: &[Spanned]) -> Result<Program, Trouble> {
    let mut parser = Parser { tokens, at: 0 };
    let program = parser.block_until(&Token::End)?;
    parser.want(&Token::End, "the end of the program")?;
    Ok(program)
}

struct Parser<'a> {
    tokens: &'a [Spanned],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        &self.tokens[self.at.min(self.tokens.len() - 1)].token
    }

    fn line(&self) -> usize {
        self.tokens[self.at.min(self.tokens.len() - 1)].line
    }

    fn take(&mut self) -> Token {
        let token = self.peek().clone();
        if self.at < self.tokens.len() - 1 {
            self.at += 1;
        }
        token
    }

    fn accept(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.take();
            return true;
        }
        false
    }

    fn want(&mut self, token: &Token, called: &str) -> Result<(), Trouble> {
        if self.accept(token) {
            return Ok(());
        }
        Err(Trouble::at(
            self.line(),
            alloc::format!("expected {called}, found {:?}", self.peek()),
        ))
    }

    /// Statements until `end`, which is not consumed.
    fn block_until(&mut self, end: &Token) -> Result<Program, Trouble> {
        let mut out = Vec::new();
        while self.peek() != end && self.peek() != &Token::End {
            out.push(self.statement()?);
        }
        Ok(out)
    }

    /// `{ .. }`
    fn braced(&mut self) -> Result<Program, Trouble> {
        self.want(&Token::OpenBrace, "`{`")?;
        let body = self.block_until(&Token::CloseBrace)?;
        self.want(&Token::CloseBrace, "`}`")?;
        Ok(body)
    }

    fn statement(&mut self) -> Result<Statement, Trouble> {
        let line = self.line();
        match self.peek().clone() {
            Token::Let => {
                self.take();
                let Token::Name(name) = self.take() else {
                    return Err(Trouble::at(line, "`let` wants a name"));
                };
                self.want(&Token::Assign, "`=`")?;
                let value = self.expression()?;
                Ok(Statement::Let { name, value, line })
            }
            Token::Fn => {
                self.take();
                let Token::Name(name) = self.take() else {
                    return Err(Trouble::at(line, "`fn` wants a name"));
                };
                self.want(&Token::OpenParen, "`(`")?;
                let mut parameters = Vec::new();
                while !self.accept(&Token::CloseParen) {
                    let Token::Name(parameter) = self.take() else {
                        return Err(Trouble::at(line, "a parameter has to be a name"));
                    };
                    parameters.push(parameter);
                    // A comma between, and one after the last is allowed
                    // because refusing it is a rule nobody remembers.
                    if !self.accept(&Token::Comma) && self.peek() != &Token::CloseParen {
                        return Err(Trouble::at(self.line(), "expected `,` or `)`"));
                    }
                }
                let body = self.braced()?;
                Ok(Statement::Function {
                    name,
                    parameters,
                    body,
                    line,
                })
            }
            Token::If => {
                self.take();
                let condition = self.expression()?;
                let then = self.braced()?;
                let otherwise = if self.accept(&Token::Else) {
                    // `else if` is an `else` whose body is one `if`, which is
                    // what every language with both does and costs nothing.
                    if self.peek() == &Token::If {
                        alloc::vec![self.statement()?]
                    } else {
                        self.braced()?
                    }
                } else {
                    Vec::new()
                };
                Ok(Statement::If {
                    condition,
                    then,
                    otherwise,
                    line,
                })
            }
            Token::While => {
                self.take();
                let condition = self.expression()?;
                let body = self.braced()?;
                Ok(Statement::While {
                    condition,
                    body,
                    line,
                })
            }
            Token::Return => {
                self.take();
                // `return` on its own returns nothing. Told apart from
                // `return <expression>` by what follows: a `}` or another
                // statement's keyword means there is no value.
                let value = match self.peek() {
                    Token::CloseBrace
                    | Token::End
                    | Token::Let
                    | Token::Fn
                    | Token::If
                    | Token::While
                    | Token::Return => None,
                    _ => Some(self.expression()?),
                };
                Ok(Statement::Return { value, line })
            }
            Token::Name(name) => {
                // A name at the start of a statement is either an assignment or
                // the beginning of an expression. One token of look-ahead tells
                // them apart, which is why `=` is not an operator here.
                if self.tokens.get(self.at + 1).map(|next| &next.token) == Some(&Token::Assign) {
                    self.take();
                    self.take();
                    let value = self.expression()?;
                    return Ok(Statement::Assign { name, value, line });
                }
                let value = self.expression()?;
                Ok(Statement::Do { value, line })
            }
            _ => {
                let value = self.expression()?;
                Ok(Statement::Do { value, line })
            }
        }
    }

    // The precedence table, lowest first. Each level calls the next and then
    // loops on its own operators, so the order of these functions *is* the
    // table -- there is no second place for it to disagree with.

    fn expression(&mut self) -> Result<Expression, Trouble> {
        self.or()
    }

    fn or(&mut self) -> Result<Expression, Trouble> {
        let mut left = self.and()?;
        while self.peek() == &Token::Or {
            let line = self.line();
            self.take();
            let right = self.and()?;
            left = Expression::Binary {
                operator: Binary::Or,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expression, Trouble> {
        let mut left = self.comparison()?;
        while self.peek() == &Token::And {
            let line = self.line();
            self.take();
            let right = self.comparison()?;
            left = Expression::Binary {
                operator: Binary::And,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expression, Trouble> {
        let mut left = self.sum()?;
        loop {
            let operator = match self.peek() {
                Token::Equal => Binary::Equal,
                Token::NotEqual => Binary::NotEqual,
                Token::Less => Binary::Less,
                Token::LessOrEqual => Binary::LessOrEqual,
                Token::Greater => Binary::Greater,
                Token::GreaterOrEqual => Binary::GreaterOrEqual,
                _ => return Ok(left),
            };
            let line = self.line();
            self.take();
            let right = self.sum()?;
            left = Expression::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
    }

    fn sum(&mut self) -> Result<Expression, Trouble> {
        let mut left = self.product()?;
        loop {
            let operator = match self.peek() {
                Token::Plus => Binary::Add,
                Token::Minus => Binary::Subtract,
                _ => return Ok(left),
            };
            let line = self.line();
            self.take();
            let right = self.product()?;
            left = Expression::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
    }

    fn product(&mut self) -> Result<Expression, Trouble> {
        let mut left = self.unary()?;
        loop {
            let operator = match self.peek() {
                Token::Star => Binary::Multiply,
                Token::Slash => Binary::Divide,
                Token::Percent => Binary::Remainder,
                _ => return Ok(left),
            };
            let line = self.line();
            self.take();
            let right = self.unary()?;
            left = Expression::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
                line,
            };
        }
    }

    fn unary(&mut self) -> Result<Expression, Trouble> {
        let line = self.line();
        let operator = match self.peek() {
            Token::Minus => Unary::Negate,
            Token::Not => Unary::Not,
            _ => return self.primary(),
        };
        self.take();
        let of = self.unary()?;
        Ok(Expression::Unary {
            operator,
            of: Box::new(of),
            line,
        })
    }

    fn primary(&mut self) -> Result<Expression, Trouble> {
        let line = self.line();
        match self.take() {
            Token::Int(value) => Ok(Expression::Int(value)),
            Token::Str(value) => Ok(Expression::Str(value)),
            Token::True => Ok(Expression::Bool(true)),
            Token::False => Ok(Expression::Bool(false)),
            Token::Nil => Ok(Expression::Nil),
            Token::OpenParen => {
                let inner = self.expression()?;
                self.want(&Token::CloseParen, "`)`")?;
                Ok(inner)
            }
            Token::Name(name) => {
                if !self.accept(&Token::OpenParen) {
                    return Ok(Expression::Name(name));
                }
                let mut arguments = Vec::new();
                while !self.accept(&Token::CloseParen) {
                    arguments.push(self.expression()?);
                    if !self.accept(&Token::Comma) && self.peek() != &Token::CloseParen {
                        return Err(Trouble::at(self.line(), "expected `,` or `)`"));
                    }
                }
                Ok(Expression::Call {
                    name,
                    arguments,
                    line,
                })
            }
            other => Err(Trouble::at(
                line,
                alloc::format!("expected a value, found {other:?}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn tree(source: &str) -> Program {
        parse(&lex(source).expect("should lex")).expect("should parse")
    }

    /// The whole point of the level ordering: `1 + 2 * 3` is `1 + (2 * 3)`.
    #[test]
    fn multiplication_binds_tighter_than_addition() {
        let program = tree("let x = 1 + 2 * 3");
        let Statement::Let { value, .. } = &program[0] else {
            panic!("expected a let");
        };
        let Expression::Binary {
            operator: Binary::Add,
            right,
            ..
        } = value
        else {
            panic!("the top should be the addition, was {value:?}");
        };
        assert!(matches!(
            **right,
            Expression::Binary {
                operator: Binary::Multiply,
                ..
            }
        ));
    }

    /// And comparison binds looser than both, so `1 + 1 == 2` is true rather
    /// than `1 + (1 == 2)`.
    #[test]
    fn comparison_binds_looser_than_arithmetic() {
        let program = tree("let x = 1 + 1 == 2");
        let Statement::Let { value, .. } = &program[0] else {
            panic!("expected a let");
        };
        assert!(matches!(
            value,
            Expression::Binary {
                operator: Binary::Equal,
                ..
            }
        ));
    }

    #[test]
    fn a_name_followed_by_assign_is_an_assignment() {
        assert!(matches!(tree("x = 1")[0], Statement::Assign { .. }));
        // And one that is not is an expression.
        assert!(matches!(tree("x")[0], Statement::Do { .. }));
    }

    #[test]
    fn else_if_is_an_else_containing_an_if() {
        let program = tree("if a { } else if b { }");
        let Statement::If { otherwise, .. } = &program[0] else {
            panic!("expected an if");
        };
        assert_eq!(otherwise.len(), 1);
        assert!(matches!(otherwise[0], Statement::If { .. }));
    }

    #[test]
    fn return_with_nothing_after_it() {
        let program = tree("fn f() { return }");
        let Statement::Function { body, .. } = &program[0] else {
            panic!("expected a function");
        };
        assert!(matches!(body[0], Statement::Return { value: None, .. }));
    }

    #[test]
    fn a_missing_brace_is_refused_with_its_line() {
        let tokens = lex("fn f() {\n  let x = 1\n").expect("should lex");
        let trouble = parse(&tokens).expect_err("the body is never closed");
        assert!(trouble.what.contains('}'), "{}", trouble.what);
    }
}
