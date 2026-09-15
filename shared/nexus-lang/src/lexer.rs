//! Characters to tokens.

use alloc::string::String;
use alloc::vec::Vec;

use crate::Trouble;

/// One thing the parser sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Int(i64),
    Str(String),
    Name(String),

    Let,
    Fn,
    If,
    Else,
    While,
    Return,
    True,
    False,
    Nil,
    And,
    Or,
    Not,

    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,

    OpenParen,
    CloseParen,
    OpenBrace,
    CloseBrace,
    Comma,

    /// The end, so the parser never has to check whether there is more.
    End,
}

/// A token and the line it was on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned {
    pub token: Token,
    pub line: usize,
}

/// Turn a program into tokens.
///
/// Newlines are *not* tokens. Statements end where they end -- `let x = 1`
/// followed by `let y = 2` needs no separator, because after the `1` there is
/// nothing a statement could continue with. That is only true because there are
/// no expression statements that begin with an operator, which is a property
/// this language has on purpose and would lose the day it gained a prefix `-`
/// at the start of a line.
///
/// # Errors
///
/// A character that begins nothing, or a string with no end.
pub fn lex(source: &str) -> Result<Vec<Spanned>, Trouble> {
    let bytes: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut line = 1usize;

    while at < bytes.len() {
        let character = bytes[at];

        if character == '\n' {
            line += 1;
            at += 1;
            continue;
        }
        if character.is_whitespace() {
            at += 1;
            continue;
        }
        // A comment runs to the end of the line. `#` and not `//`, because
        // `//` would need a look-ahead to tell it from division and this does
        // not.
        if character == '#' {
            while at < bytes.len() && bytes[at] != '\n' {
                at += 1;
            }
            continue;
        }

        if character.is_ascii_digit() {
            let from = at;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            let text: String = bytes[from..at].iter().collect();
            // Refused rather than wrapped. A number too large for the type is
            // somebody's mistake, and silently becoming a different number is
            // the worst way to answer it.
            let number = text
                .parse::<i64>()
                .map_err(|_| Trouble::at(line, "this number is too large"))?;
            out.push(Spanned {
                token: Token::Int(number),
                line,
            });
            continue;
        }

        if character.is_alphabetic() || character == '_' {
            let from = at;
            while at < bytes.len() && (bytes[at].is_alphanumeric() || bytes[at] == '_') {
                at += 1;
            }
            let text: String = bytes[from..at].iter().collect();
            let token = match text.as_str() {
                "let" => Token::Let,
                "fn" => Token::Fn,
                "if" => Token::If,
                "else" => Token::Else,
                "while" => Token::While,
                "return" => Token::Return,
                "true" => Token::True,
                "false" => Token::False,
                "nil" => Token::Nil,
                "and" => Token::And,
                "or" => Token::Or,
                "not" => Token::Not,
                _ => Token::Name(text),
            };
            out.push(Spanned { token, line });
            continue;
        }

        if character == '"' {
            at += 1;
            let mut text = String::new();
            loop {
                if at >= bytes.len() {
                    return Err(Trouble::at(line, "this string never ends"));
                }
                match bytes[at] {
                    '"' => {
                        at += 1;
                        break;
                    }
                    // A newline inside a string is almost always a missing
                    // quote, and reporting it here names the line it started on
                    // rather than the end of the file.
                    '\n' => return Err(Trouble::at(line, "this string never ends")),
                    '\\' if at + 1 < bytes.len() => {
                        at += 1;
                        text.push(match bytes[at] {
                            'n' => '\n',
                            't' => '\t',
                            '\\' => '\\',
                            '"' => '"',
                            other => other,
                        });
                        at += 1;
                    }
                    other => {
                        text.push(other);
                        at += 1;
                    }
                }
            }
            out.push(Spanned {
                token: Token::Str(text),
                line,
            });
            continue;
        }

        // Two-character operators first, or `==` reads as two assignments.
        let two = if at + 1 < bytes.len() {
            Some((character, bytes[at + 1]))
        } else {
            None
        };
        let token = match two {
            Some(('=', '=')) => Some(Token::Equal),
            Some(('!', '=')) => Some(Token::NotEqual),
            Some(('<', '=')) => Some(Token::LessOrEqual),
            Some(('>', '=')) => Some(Token::GreaterOrEqual),
            _ => None,
        };
        if let Some(token) = token {
            out.push(Spanned { token, line });
            at += 2;
            continue;
        }

        let token = match character {
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' => Token::Star,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '=' => Token::Assign,
            '<' => Token::Less,
            '>' => Token::Greater,
            '(' => Token::OpenParen,
            ')' => Token::CloseParen,
            '{' => Token::OpenBrace,
            '}' => Token::CloseBrace,
            ',' => Token::Comma,
            other => {
                return Err(Trouble::at(
                    line,
                    alloc::format!("there is no use for {other:?} here"),
                ));
            }
        };
        out.push(Spanned { token, line });
        at += 1;
    }

    out.push(Spanned {
        token: Token::End,
        line,
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString as _;

    fn kinds(source: &str) -> Vec<Token> {
        lex(source)
            .expect("should lex")
            .into_iter()
            .map(|spanned| spanned.token)
            .collect()
    }

    #[test]
    fn it_reads_the_pieces_of_a_statement() {
        assert_eq!(
            kinds("let x = 1 + 2"),
            alloc::vec![
                Token::Let,
                Token::Name("x".to_string()),
                Token::Assign,
                Token::Int(1),
                Token::Plus,
                Token::Int(2),
                Token::End
            ]
        );
    }

    /// `==` is one token. Reading it as two assignments is the mistake this
    /// orders the matching to avoid.
    #[test]
    fn two_character_operators_are_one_token() {
        assert_eq!(
            kinds("a == b"),
            alloc::vec![
                Token::Name("a".to_string()),
                Token::Equal,
                Token::Name("b".to_string()),
                Token::End
            ]
        );
    }

    #[test]
    fn it_counts_lines_for_the_error_message() {
        let trouble = lex("let x = 1\nlet y = ?").expect_err("`?` is not a token");
        assert_eq!(trouble.line, 2);
    }

    #[test]
    fn a_string_that_never_ends_is_refused() {
        assert!(lex("let x = \"oh").is_err());
        // And a newline ends it, because that is nearly always a missing quote.
        assert!(lex("let x = \"oh\nlet y = 1").is_err());
    }

    #[test]
    fn comments_run_to_the_end_of_the_line() {
        assert_eq!(
            kinds("1 # two\n3"),
            alloc::vec![Token::Int(1), Token::Int(3), Token::End]
        );
    }

    #[test]
    fn escapes_in_strings() {
        assert_eq!(
            kinds(r#""a\nb""#),
            alloc::vec![Token::Str("a\nb".to_string()), Token::End]
        );
    }

    /// Wrapping would make a program mean something nobody wrote.
    #[test]
    fn a_number_too_large_is_refused() {
        assert!(lex("99999999999999999999").is_err());
    }
}
