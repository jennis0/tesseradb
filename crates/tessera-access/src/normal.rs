//! Normalisation and the canonical text of a label.
//!
//! Operands are ordered by [`Expr`]'s derived order: terms by their bytes, before any conjunction,
//! before any disjunction. The order depends on nothing but the label, so a label normalises to
//! the same expression, and the same text, in every bundle.

use super::Expr;

/// Flattens nested operators of one kind, sorts and deduplicates operands, and replaces an
/// operator left with one operand by that operand.
pub(super) fn flatten(e: Expr) -> Expr {
    reshape(e, false)
}

/// [`flatten`], and absorption: `a|(a&b)` is `a`, and `a&(a|b)` is `a`.
pub(super) fn normalise(e: Expr) -> Expr {
    reshape(e, true)
}

fn reshape(e: Expr, absorbing: bool) -> Expr {
    let (and, children) = match e {
        Expr::Term(_) => return e,
        Expr::And(v) => (true, v),
        Expr::Or(v) => (false, v),
    };
    let mut ops = Vec::with_capacity(children.len());
    for child in children {
        match (and, reshape(child, absorbing)) {
            (true, Expr::And(v)) | (false, Expr::Or(v)) => ops.extend(v),
            (_, other) => ops.push(other),
        }
    }
    ops.sort_unstable();
    ops.dedup();
    if absorbing {
        absorb(&mut ops, and);
    }
    match (ops.len(), and) {
        (1, _) => ops.swap_remove(0),
        (_, true) => Expr::And(ops),
        (_, false) => Expr::Or(ops),
    }
}

/// Under a disjunction, removes each conjunction X for which another operand Y has every
/// conjunct of Y among X's conjuncts. Under a conjunction, the dual with disjuncts. `ops` is
/// sorted and distinct, and so is each operand's list of operands.
fn absorb(ops: &mut Vec<Expr>, and: bool) {
    fn parts(and: bool, x: &Expr) -> Option<&[Expr]> {
        match (and, x) {
            (false, Expr::And(v)) | (true, Expr::Or(v)) => Some(v),
            _ => None,
        }
    }
    let mut keep = vec![true; ops.len()];
    for (i, x) in ops.iter().enumerate() {
        let Some(xs) = parts(and, x) else { continue };
        let absorbed = ops.iter().enumerate().any(|(j, y)| {
            i != j && keep[j] && is_subset(parts(and, y).unwrap_or(y.operands()), xs)
        });
        keep[i] = !absorbed;
    }
    let mut kept = keep.into_iter();
    ops.retain(|_| kept.next().unwrap_or(true));
}

/// Whether every element of `small` is in `big`. Both are sorted.
fn is_subset(small: &[Expr], big: &[Expr]) -> bool {
    let mut rest = big.iter();
    small.len() <= big.len()
        && small
            .iter()
            .all(|s| rest.by_ref().find(|b| *b >= s).is_some_and(|b| b == s))
}

/// Operators and term occurrences in `e`.
pub(super) fn nodes(e: &Expr) -> usize {
    match e {
        Expr::Term(_) => 1,
        Expr::And(v) | Expr::Or(v) => 1 + v.iter().map(nodes).sum::<usize>(),
    }
}

/// The text of a normalised expression: operands in order, an operator's operand in brackets,
/// and a term bare where the grammar allows.
pub(super) fn canonical(e: &Expr) -> String {
    let mut out = String::new();
    write(e, &mut out, false);
    out
}

fn write(e: &Expr, out: &mut String, nested: bool) {
    let (op, v) = match e {
        Expr::Term(t) => return write_term(t, out),
        Expr::And(v) => ('&', v),
        Expr::Or(v) => ('|', v),
    };
    if nested {
        out.push('(');
    }
    for (i, operand) in v.iter().enumerate() {
        if i > 0 {
            out.push(op);
        }
        write(operand, out, true);
    }
    if nested {
        out.push(')');
    }
}

fn write_term(t: &str, out: &mut String) {
    if t.chars().all(super::parse::is_bare) {
        out.push_str(t);
        return;
    }
    out.push('"');
    for c in t.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::super::{Label, DEFAULT_MAX_NODES};

    fn n(text: &str) -> String {
        Label::parse(text, DEFAULT_MAX_NODES).unwrap().canonical()
    }

    #[test]
    fn equal_labels_normalise_to_equal_text() {
        assert_eq!(n("c&(b&a)"), "a&b&c");
        assert_eq!(n("b|a|a"), "a|b");
        assert_eq!(n("(b|a)&(a|b)"), "a|b");
        assert_eq!(n("((a))"), "a");
        assert_eq!(n("(y|x)&c"), "c&(x|y)");
        assert_eq!(n("(y&x)|(b&a)|z"), "z|(a&b)|(x&y)");
    }

    #[test]
    fn absorption_in_both_directions() {
        assert_eq!(n("a|(a&b)"), "a");
        assert_eq!(n("a&(a|b)"), "a");
        assert_eq!(n("(a&b)|(a&b&c)"), "a&b");
        assert_eq!(n("(a|b)&(a|b|c)&d"), "d&(a|b)");
        assert_eq!(n("a&(b|c)"), "a&(b|c)");
        assert_ne!(n("a&(b|c)"), n("(a&b)|(a&c)"));
    }

    #[test]
    fn terms_outside_the_bare_set_are_quoted() {
        assert_eq!(n(r#""b c"&"x\"y\\z"&a"#), r#"a&"b c"&"x\"y\\z""#);
        assert_eq!(n(r#""plain""#), "plain");
    }

    #[test]
    fn the_canonical_text_parses_to_the_same_label() {
        for text in [
            "a&(b|\"c d\")",
            "(x&y)|(p&(q|r))|z",
            "\"a\\\\\"|b",
            "public",
        ] {
            let label = Label::parse(text, DEFAULT_MAX_NODES).unwrap();
            let again = Label::parse(&label.canonical(), DEFAULT_MAX_NODES).unwrap();
            assert_eq!(again, label, "{text}");
        }
    }
}
