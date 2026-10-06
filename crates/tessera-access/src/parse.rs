//! The grammar of an access expression.
//!
//! A term is bare when it is made of ASCII letters, digits and `_ - . : /`, and is otherwise
//! double-quoted, with `\"` and `\\` as the only escapes. `&` and `|` join operands, and mixing
//! them in one bracket is refused. Whitespace outside quotes separates tokens and is otherwise
//! ignored, so two operands with only whitespace between them are refused for want of an operator.
//! A quoted term is the text between its quotes, inner spaces included, with its escapes applied.
//! A credential's terms are trimmed ([`super::held_term`]), so a quoted term with whitespace at
//! either end is refused: nobody could hold it. A term holding a control character, or equal to
//! `public` or `inherited` ignoring ASCII case, is refused. `public` is valid only as a whole
//! label, which [`super::Label::parse`] handles before this parser runs. `inherited` is never a
//! label.

use super::{Expr, LabelError};

/// Deeper nesting is refused, so that parsing, normalising and dropping a label stay within the
/// stack.
const MAX_DEPTH: usize = 256;

pub(super) const EMPTY: &str =
    "the label is empty; write a term, an expression such as `a&(b|c)`, or `public`";
const MIXED: &str = "`&` and `|` are mixed without brackets; bracket one side, as in `(a&b)|c`";
const NO_OPERATOR: &str = "two operands follow each other; join them with `&` or `|`";
const ENDS: &str = "the label ends where a term was expected; finish it with a term";
const UNCLOSED: &str = "a bracket is not closed; add `)`";
const STRAY_CLOSE: &str = "`)` closes no bracket; remove it";
const NOT_A_TERM: &str = "a term was expected; write letters, digits and `_-.:/` bare, or \
     quote any other term, as in `\"team a\"`";
const UNCLOSED_QUOTE: &str = "a quoted term is not closed; add `\"`";
const BAD_ESCAPE: &str =
    "only `\\\"` and `\\\\` are escapes in a quoted term; write other characters as they are";
const EMPTY_TERM: &str = "a quoted term is empty; write at least one character between the quotes";
const EDGE_SPACE: &str = "a quoted term starts or ends with whitespace, which no credential \
     can hold; remove the whitespace from the ends of the quoted term";
const CONTROL: &str = "a term holds a control character; remove it";
const PUBLIC_TERM: &str = "`public` is reserved and valid only as the whole label; write \
     `public` alone, or use another term";
const INHERITED_TERM: &str = "`inherited` is reserved for an annotation layer's artifact default \
     and is not a label; write another term, or `public`";
const TOO_DEEP: &str = "brackets nest more than 256 deep; write the expression with fewer brackets";

/// Parses `text` into an expression, without normalising it. `text` is not `public`, and is not
/// empty once trimmed.
pub(super) fn parse(text: &str) -> Result<Expr, LabelError> {
    let mut parser = Parser { s: text, i: 0 };
    let expr = parser.expr(0)?;
    if parser.i < text.len() {
        return Err(parser.err(STRAY_CLOSE));
    }
    Ok(expr)
}

pub(super) fn is_bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/')
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn err(&self, reason: &'static str) -> LabelError {
        LabelError::Invalid { at: self.i, reason }
    }

    /// The next character that is not whitespace, after moving past any whitespace before it.
    fn peek(&mut self) -> Option<char> {
        let rest = &self.s[self.i..];
        let token = rest.trim_start();
        self.i += rest.len() - token.len();
        token.chars().next()
    }

    /// The next character, whitespace included.
    fn peek_raw(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn expr(&mut self, depth: usize) -> Result<Expr, LabelError> {
        if depth > MAX_DEPTH {
            return Err(self.err(TOO_DEEP));
        }
        let mut operands = vec![self.operand(depth)?];
        let mut op = None;
        loop {
            let c = match self.peek() {
                None | Some(')') => break,
                Some(c @ ('&' | '|')) => c,
                Some(_) => return Err(self.err(NO_OPERATOR)),
            };
            if op.is_some_and(|o| o != c) {
                return Err(self.err(MIXED));
            }
            op = Some(c);
            self.i += 1;
            operands.push(self.operand(depth)?);
        }
        Ok(match op {
            None => operands.swap_remove(0),
            Some('&') => Expr::And(operands),
            Some(_) => Expr::Or(operands),
        })
    }

    fn operand(&mut self, depth: usize) -> Result<Expr, LabelError> {
        let next = self.peek();
        let start = self.i;
        let term = match next {
            None => return Err(self.err(ENDS)),
            Some('(') => return self.bracketed(depth),
            Some('"') => self.quoted()?,
            Some(c) if is_bare(c) => self.bare(),
            Some(_) => return Err(self.err(NOT_A_TERM)),
        };
        check_term(&term).map_err(|reason| LabelError::Invalid { at: start, reason })?;
        Ok(Expr::Term(term.into()))
    }

    fn bracketed(&mut self, depth: usize) -> Result<Expr, LabelError> {
        self.i += 1;
        let expr = self.expr(depth + 1)?;
        if self.peek() != Some(')') {
            return Err(self.err(UNCLOSED));
        }
        self.i += 1;
        Ok(expr)
    }

    fn bare(&mut self) -> String {
        let rest = &self.s[self.i..];
        let len = rest.find(|c| !is_bare(c)).unwrap_or(rest.len());
        self.i += len;
        rest[..len].to_owned()
    }

    fn quoted(&mut self) -> Result<String, LabelError> {
        let open = self.i;
        self.i += 1;
        let mut term = String::new();
        loop {
            let c = self.peek_raw().ok_or_else(|| self.err(UNCLOSED_QUOTE))?;
            self.i += c.len_utf8();
            match c {
                '"' => break,
                '\\' => term.push(self.escaped()?),
                c => term.push(c),
            }
        }
        if term.is_empty() {
            return Err(LabelError::Invalid {
                at: open,
                reason: EMPTY_TERM,
            });
        }
        Ok(term)
    }

    fn escaped(&mut self) -> Result<char, LabelError> {
        match self.peek_raw() {
            Some(c @ ('"' | '\\')) => {
                self.i += 1;
                Ok(c)
            }
            _ => Err(self.err(BAD_ESCAPE)),
        }
    }
}

/// The rules for a term, beyond the grammar.
fn check_term(term: &str) -> Result<(), &'static str> {
    if term.chars().any(char::is_control) {
        return Err(CONTROL);
    }
    if term.trim() != term {
        return Err(EDGE_SPACE);
    }
    if term.eq_ignore_ascii_case(super::PUBLIC) {
        return Err(PUBLIC_TERM);
    }
    if term.eq_ignore_ascii_case(super::INHERITED) {
        return Err(INHERITED_TERM);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(t: &str) -> Expr {
        Expr::Term(t.into())
    }

    #[test]
    fn terms_bare_and_quoted() {
        assert_eq!(parse("user:a/b-c.d_E9"), Ok(term("user:a/b-c.d_E9")));
        assert_eq!(parse(r#""a\"b\\c""#), Ok(term(r#"a"b\c"#)));
        assert_eq!(parse("\"team  a\""), Ok(term("team  a")));
        assert_eq!(parse("\"é&|()\""), Ok(term("é&|()")));
        assert_eq!(parse(" \t(a) "), Ok(term("a")));
    }

    #[test]
    fn whitespace_outside_quotes_means_nothing() {
        for (spaced, compact) in [
            (" pharma_a & ( gb | fr ) ", "pharma_a&(gb|fr)"),
            ("a\t&\nb", "a&b"),
            ("( a|b)", "(a|b)"),
            ("(a|b )", "(a|b)"),
            ("a&( b)", "a&(b)"),
            ("\"team a\" &b", "\"team a\"&b"),
            ("a\u{3000}|\u{a0}b", "a|b"),
        ] {
            assert_eq!(parse(spaced), parse(compact), "{spaced:?}");
            assert!(parse(compact).is_ok(), "{compact:?}");
        }
    }

    #[test]
    fn operators_and_brackets() {
        let and = Expr::And(vec![term("a"), Expr::Or(vec![term("b"), term("c")])]);
        assert_eq!(parse("a&(b|c)"), Ok(and));
        assert!(parse("(a&b)|c").is_ok());
        assert!(parse("a|b|c").is_ok());
    }

    #[test]
    fn what_the_grammar_refuses() {
        let refused = [
            "a&b|c",
            "a|b&c",
            "a&",
            "&a",
            "a b",
            "a\tb",
            "(a) (b)",
            "\"a\" \"b\"",
            "\" a\"",
            "\"a \"",
            "\" team a \"",
            "\"  \"",
            "\"a\u{a0}\"",
            "x&\"\u{3000}y\"",
            "(a",
            "a)",
            "()",
            "\"\"",
            "\"a",
            "\"a\\n\"",
            "!a",
            "a&!b",
            "é",
            "a,b",
            "\"a\tb\"",
            "\"\u{7}\"",
            "public&a",
            "PUBLIC",
            "inherited",
            "a|Inherited",
            "\"INHERITED\"&b",
        ];
        for text in refused {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = |n: usize| format!("{}a{}", "(".repeat(n), ")".repeat(n));
        assert!(parse(&deep(MAX_DEPTH)).is_ok());
        assert!(parse(&deep(MAX_DEPTH + 1)).is_err());
    }

    #[test]
    fn an_error_names_the_byte_it_is_at() {
        assert!(matches!(
            parse("a&b|c"),
            Err(LabelError::Invalid { at: 3, .. })
        ));
        assert!(matches!(
            parse("a&\"Public\""),
            Err(LabelError::Invalid { at: 2, .. })
        ));
        assert!(matches!(
            parse("a&\"\""),
            Err(LabelError::Invalid { at: 2, .. })
        ));
        assert!(matches!(
            parse("  a  b"),
            Err(LabelError::Invalid { at: 5, .. })
        ));
        assert!(matches!(
            parse("a | \" b\""),
            Err(LabelError::Invalid { at: 4, .. })
        ));
    }
}
