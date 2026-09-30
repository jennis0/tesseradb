//! Accumulo visibility expressions without negation: the parser and normalisation.

use std::cmp::Ordering;

/// A parsed expression. Terms are ids from the caller's term dictionary, so the derived order is
/// canonical within one dictionary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Expr {
    Term(u32),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

const MAX_DEPTH: usize = 256;

fn is_bare(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/')
}

/// Parses `input`, calling `term` once per term occurrence to turn its bytes into an id.
pub fn parse(input: &[u8], term: &mut dyn FnMut(&[u8]) -> u32) -> Result<Expr, ParseError> {
    if input.is_empty() {
        return Err(ParseError("the expression is empty; write at least one term".into()));
    }
    let mut p = Parser { s: input, i: 0, term, scratch: Vec::new() };
    let e = p.expr(0)?;
    if p.i != p.s.len() {
        return Err(p.err("unexpected character; close a bracket only after opening one"));
    }
    Ok(e)
}

struct Parser<'a, 'b> {
    s: &'a [u8],
    i: usize,
    term: &'b mut dyn FnMut(&[u8]) -> u32,
    scratch: Vec<u8>,
}

impl Parser<'_, '_> {
    fn err(&self, what: &str) -> ParseError {
        ParseError(format!("at byte {}: {what}", self.i))
    }

    fn expr(&mut self, depth: usize) -> Result<Expr, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("brackets nest too deeply"));
        }
        let first = self.atom(depth)?;
        let mut op: Option<u8> = None;
        let mut operands = vec![first];
        while self.i < self.s.len() {
            let b = self.s[self.i];
            match b {
                b'&' | b'|' => {
                    if op.is_some_and(|o| o != b) {
                        return Err(self.err("& and | are mixed; bracket one side, as in (a&b)|c"));
                    }
                    op = Some(b);
                    self.i += 1;
                    operands.push(self.atom(depth)?);
                }
                b')' => break,
                _ => return Err(self.err("expected & or | between terms")),
            }
        }
        Ok(match op {
            None => operands.pop().unwrap(),
            Some(b'&') => Expr::And(operands),
            Some(_) => Expr::Or(operands),
        })
    }

    fn atom(&mut self, depth: usize) -> Result<Expr, ParseError> {
        match self.s.get(self.i) {
            None => Err(self.err("the expression ends where a term was expected")),
            Some(b'(') => {
                self.i += 1;
                let e = self.expr(depth + 1)?;
                if self.s.get(self.i) != Some(&b')') {
                    return Err(self.err("a bracket is not closed; add )"));
                }
                self.i += 1;
                Ok(e)
            }
            Some(b'"') => {
                self.i += 1;
                self.scratch.clear();
                loop {
                    match self.s.get(self.i) {
                        None => return Err(self.err("a quoted term is not closed; add \"")),
                        Some(b'"') => {
                            self.i += 1;
                            break;
                        }
                        Some(b'\\') => match self.s.get(self.i + 1) {
                            Some(&c @ (b'"' | b'\\')) => {
                                self.scratch.push(c);
                                self.i += 2;
                            }
                            _ => return Err(self.err("only \\\" and \\\\ may be escaped")),
                        },
                        Some(&c) => {
                            self.scratch.push(c);
                            self.i += 1;
                        }
                    }
                }
                if self.scratch.is_empty() {
                    return Err(self.err("a quoted term is empty; write at least one character"));
                }
                Ok(Expr::Term((self.term)(&self.scratch)))
            }
            Some(&b) if is_bare(b) => {
                let start = self.i;
                while self.i < self.s.len() && is_bare(self.s[self.i]) {
                    self.i += 1;
                }
                Ok(Expr::Term((self.term)(&self.s[start..self.i])))
            }
            Some(_) => Err(self.err("a term may be bare [A-Za-z0-9_-.:/] or double-quoted")),
        }
    }
}

/// Flattens nested same-operator nodes, sorts and removes duplicate operands, applies absorption
/// and replaces a one-operand node by its operand. Bottom-up, so each child is normal first.
pub fn normalise(e: Expr) -> Expr {
    let (is_and, children) = match e {
        Expr::Term(_) => return e,
        Expr::And(v) => (true, v),
        Expr::Or(v) => (false, v),
    };
    let mut ops = Vec::with_capacity(children.len());
    for c in children {
        match (is_and, normalise(c)) {
            (true, Expr::And(v)) | (false, Expr::Or(v)) => ops.extend(v),
            (_, c) => ops.push(c),
        }
    }
    ops.sort_unstable();
    ops.dedup();
    absorb(&mut ops, is_and);
    if ops.len() == 1 {
        return ops.pop().unwrap();
    }
    if is_and {
        Expr::And(ops)
    } else {
        Expr::Or(ops)
    }
}

/// Under an OR (`is_and == false`), removes each operand X for which another operand Y has
/// conjuncts(Y) ⊆ conjuncts(X): `a|(a&b)` is `a`. Under an AND, the dual with disjuncts.
fn absorb(ops: &mut Vec<Expr>, is_and: bool) {
    // Only an operand of the dual kind has more than one part, so only it can be absorbed.
    fn parts(is_and: bool, x: &Expr) -> Option<&[Expr]> {
        match (is_and, x) {
            (false, Expr::And(v)) | (true, Expr::Or(v)) => Some(v.as_slice()),
            _ => None,
        }
    }
    let parts = |x| parts(is_and, x);
    if !ops.iter().any(|x| parts(x).is_some()) {
        return;
    }
    let mut keep = vec![true; ops.len()];
    for (i, x) in ops.iter().enumerate() {
        let Some(xs) = parts(x) else { continue };
        for (j, y) in ops.iter().enumerate() {
            if i == j || !keep[j] {
                continue;
            }
            let ys: &[Expr] = parts(y).unwrap_or(std::slice::from_ref(y));
            if is_subset(ys, xs) {
                keep[i] = false;
                break;
            }
        }
    }
    let mut k = keep.into_iter();
    ops.retain(|_| k.next().unwrap());
}

fn is_subset(small: &[Expr], big: &[Expr]) -> bool {
    if small.len() > big.len() {
        return false;
    }
    let mut j = 0;
    for s in small {
        loop {
            match big.get(j).map(|b| b.cmp(s)) {
                None | Some(Ordering::Greater) => return false,
                Some(Ordering::Equal) => {
                    j += 1;
                    break;
                }
                Some(Ordering::Less) => j += 1,
            }
        }
    }
    true
}

/// Direct evaluation, used only to check normalisation.
#[cfg(test)]
pub fn eval(e: &Expr, held: &dyn Fn(u32) -> bool) -> bool {
    match e {
        Expr::Term(t) => held(*t),
        Expr::And(v) => v.iter().all(|c| eval(c, held)),
        Expr::Or(v) => v.iter().any(|c| eval(c, held)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::HashMap;

    fn p(s: &str) -> Result<Expr, ParseError> {
        let mut dict: HashMap<Vec<u8>, u32> = HashMap::new();
        for (i, t) in ["a", "b", "c", "d"].iter().enumerate() {
            dict.insert(t.as_bytes().to_vec(), i as u32);
        }
        parse(s.as_bytes(), &mut |t| {
            let n = dict.len() as u32;
            *dict.entry(t.to_vec()).or_insert(n)
        })
    }

    fn n(s: &str) -> Expr {
        normalise(p(s).unwrap())
    }

    #[test]
    fn grammar() {
        assert_eq!(p("a").unwrap(), Expr::Term(0));
        assert!(p("").is_err());
        assert!(p("a&b|c").is_err());
        assert!(p("(a&b)|c").is_ok());
        assert!(p("a&(b|c)").is_ok());
        assert!(p("((a))").is_ok());
        assert!(p("()").is_err());
        assert!(p("a&").is_err());
        assert!(p("a b").is_err());
        assert!(p("(a&b").is_err());
        assert!(p("a&b)").is_err());
        assert!(p("\"\"").is_err());
        assert!(p("\"x\\y\"").is_err());
        assert!(p("!a").is_err());
        let q = p("\"a\\\"b\\\\\"&a").unwrap();
        assert_eq!(q, Expr::And(vec![Expr::Term(4), Expr::Term(0)]));
        assert_eq!(p("\"a\"").unwrap(), Expr::Term(0));
        assert!(p("user:a/b-c.d_e").is_ok());
    }

    #[test]
    fn normal_forms() {
        assert_eq!(n("a|(a&b)"), n("a"));
        assert_eq!(n("a&(a|b)"), n("a"));
        assert_eq!(n("(a&b)|(a&b&c)"), n("a&b"));
        assert_eq!(n("a&(b&c)"), n("c&b&a"));
        assert_eq!(n("a|a|b"), n("b|a"));
        assert_eq!(n("(a|b)&(b|a)"), n("a|b"));
        assert_eq!(n("((a))"), n("a"));
        assert_ne!(n("a&(b|c)"), n("(a&b)|(a&c)"));
    }

    fn random(rng: &mut StdRng, depth: u32) -> String {
        if depth == 0 || rng.gen_bool(0.35) {
            return ["a", "b", "c", "d"][rng.gen_range(0..4)].into();
        }
        let op = if rng.gen_bool(0.5) { "&" } else { "|" };
        let k = rng.gen_range(1..=4);
        let parts: Vec<String> = (0..k).map(|_| format!("({})", random(rng, depth - 1))).collect();
        parts.join(op)
    }

    #[test]
    fn normalisation_preserves_every_decision() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..5_000 {
            let s = random(&mut rng, 4);
            let raw = p(&s).unwrap();
            let norm = normalise(raw.clone());
            assert_eq!(normalise(norm.clone()), norm, "{s}");
            for held in 0u32..16 {
                let h = |t: u32| held & (1 << t) != 0;
                assert_eq!(eval(&raw, &h), eval(&norm, &h), "{s} held={held:04b}");
            }
        }
    }
}
