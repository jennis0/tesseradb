//! Access labels as Accumulo visibility expressions without negation.
//!
//! [`Label::parse`] is the one reader of a label, for the build and for ingest alike. It trims the
//! text, refuses what the grammar refuses, and returns the label normalised: nested operators of
//! one kind flattened, operands sorted and deduplicated, and absorption applied, so that `a|(a&b)`
//! is `a`. Equal labels then have equal [`Label::canonical`] text. Two equivalent labels that
//! normalise differently stay two labels.
//!
//! [`Labels`] gives each distinct label a [`LabelId`]. A label that is `public` or a disjunction
//! of terms is recorded with its terms, which index it. Every other label holds a conjunction and
//! is compiled into a shared expression DAG, which authorise evaluates bottom-up from a
//! credential's terms. The expressions have no negation, so a label is monotone in the terms
//! held: holding more terms admits a superset.

mod dag;
mod labels;
mod normal;
mod parse;

pub use dag::Scratch;
pub use labels::Labels;

use std::fmt;

/// The largest number of nodes a label may hold unless the service configures another.
pub const DEFAULT_MAX_NODES: usize = 1024;

/// A parsed access expression. Operands of a normalised expression are sorted, distinct, and
/// never of their parent's kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Expr {
    Term(Box<str>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// Which index serves a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `public`, which every session holds.
    Public,
    /// A single term or a disjunction of terms. It is indexed under each of its terms.
    AnyOf,
    /// Any label holding a conjunction. It is indexed under its label id and evaluated in the DAG.
    Compound,
}

/// A normalised access label.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Label(Option<Expr>);

/// Why a label was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelError {
    /// The text does not follow the grammar, or breaks a rule about terms. `at` is a byte offset
    /// into the text as given.
    Invalid { at: usize, reason: &'static str },
    /// The label holds more nodes than the limit allows.
    TooLarge { nodes: usize, limit: usize },
}

impl fmt::Display for LabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LabelError::Invalid { at, reason } => write!(f, "access label, at byte {at}: {reason}"),
            LabelError::TooLarge { nodes, limit } => write!(
                f,
                "the access label holds {nodes} nodes and the limit is {limit}; write a shorter \
                 label, or raise the limit"
            ),
        }
    }
}

impl std::error::Error for LabelError {}

impl Label {
    /// Reads `text` as an access label and normalises it. `max_nodes` bounds the operators and
    /// term occurrences of a label that holds a conjunction once nested operators are flattened
    /// and repeated operands removed, before absorption. A term or a disjunction of terms has no
    /// bound.
    pub fn parse(text: &str, max_nodes: usize) -> Result<Label, LabelError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(LabelError::Invalid {
                at: 0,
                reason: parse::EMPTY,
            });
        }
        if trimmed == tessera_types::label::PUBLIC {
            return Ok(Label(None));
        }
        let flat = normal::flatten(parse::parse(text)?);
        if flat.is_any_of() {
            return Ok(Label(Some(flat)));
        }
        let nodes = normal::nodes(&flat);
        if nodes > max_nodes {
            return Err(LabelError::TooLarge {
                nodes,
                limit: max_nodes,
            });
        }
        Ok(Label(Some(normal::normalise(flat))))
    }

    /// The expression, or `None` for `public`.
    pub fn expr(&self) -> Option<&Expr> {
        self.0.as_ref()
    }

    pub fn shape(&self) -> Shape {
        match &self.0 {
            None => Shape::Public,
            Some(e) if e.is_any_of() => Shape::AnyOf,
            Some(_) => Shape::Compound,
        }
    }

    /// The text stored for this label. Parsing it gives this label back.
    pub fn canonical(&self) -> String {
        match &self.0 {
            None => tessera_types::label::PUBLIC.to_owned(),
            Some(e) => normal::canonical(e),
        }
    }

    /// Whether a principal holding the terms `held` satisfies this label.
    pub fn satisfied_by(&self, held: &impl Fn(&str) -> bool) -> bool {
        self.0.as_ref().is_none_or(|e| e.satisfied_by(held))
    }
}

impl Expr {
    fn satisfied_by(&self, held: &impl Fn(&str) -> bool) -> bool {
        match self {
            Expr::Term(t) => held(t),
            Expr::And(v) => v.iter().all(|e| e.satisfied_by(held)),
            Expr::Or(v) => v.iter().any(|e| e.satisfied_by(held)),
        }
    }

    /// Whether this is a term or a disjunction of terms.
    fn is_any_of(&self) -> bool {
        match self {
            Expr::Term(_) => true,
            Expr::Or(v) => v.iter().all(|e| matches!(e, Expr::Term(_))),
            Expr::And(_) => false,
        }
    }

    /// The operands of an operator, or the term itself.
    fn operands(&self) -> &[Expr] {
        match self {
            Expr::Term(_) => std::slice::from_ref(self),
            Expr::And(v) | Expr::Or(v) => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(text: &str) -> Label {
        Label::parse(text, DEFAULT_MAX_NODES).unwrap()
    }

    #[test]
    fn public_is_accepted_only_as_the_whole_label() {
        assert_eq!(label(" public ").shape(), Shape::Public);
        assert_eq!(label("public").canonical(), "public");
        for text in [
            "Public",
            "PUBLIC",
            "\"public\"",
            "(public)",
            "public|a",
            "a&\"PuBlic\"",
        ] {
            assert!(Label::parse(text, DEFAULT_MAX_NODES).is_err(), "{text}");
        }
        assert!(label("publicly").shape() == Shape::AnyOf);
    }

    #[test]
    fn inherited_is_refused_in_any_case_and_any_position() {
        for text in [
            "inherited",
            " Inherited ",
            "INHERITED",
            "\"inherited\"",
            "(inherited)",
            "a&inherited",
            "a|(b&InHeRiTeD)",
        ] {
            assert!(Label::parse(text, DEFAULT_MAX_NODES).is_err(), "{text}");
        }
        assert_eq!(label("inheritance").shape(), Shape::AnyOf);
    }

    #[test]
    fn shapes() {
        assert_eq!(label("a").shape(), Shape::AnyOf);
        assert_eq!(label("a|b|\"c d\"").shape(), Shape::AnyOf);
        assert_eq!(label("a|(a&b)").shape(), Shape::AnyOf);
        assert_eq!(label("a&b").shape(), Shape::Compound);
        assert_eq!(label("a|(b&c)").shape(), Shape::Compound);
    }

    #[test]
    fn the_node_limit_counts_the_flattened_label() {
        assert!(Label::parse("a&(b&c)", 4).is_ok());
        assert!(Label::parse("a&a&a&(a&b)", 3).is_ok());
        let refused = Label::parse("a&(b|c)", 4);
        assert_eq!(refused, Err(LabelError::TooLarge { nodes: 5, limit: 4 }));
    }

    #[test]
    fn a_disjunction_of_terms_has_no_node_limit() {
        let wide = (0..5000).map(|i| format!("t{i}")).collect::<Vec<_>>();
        let label = Label::parse(&wide.join("|"), DEFAULT_MAX_NODES).unwrap();
        assert_eq!(label.shape(), Shape::AnyOf);
        assert_eq!(label.expr().map(|e| e.operands().len()), Some(5000));
        assert!(Label::parse("a|b|c", 1).is_ok());
        let compound = format!("x&({})", wide[..DEFAULT_MAX_NODES].join("|"));
        assert!(matches!(
            Label::parse(&compound, DEFAULT_MAX_NODES),
            Err(LabelError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_label_is_satisfied_by_the_terms_it_needs() {
        let l = label("secret&(team_a|\"team b\")");
        assert!(l.satisfied_by(&|t| t == "secret" || t == "team b"));
        assert!(!l.satisfied_by(&|t| t == "secret"));
        assert!(label("public").satisfied_by(&|_| false));
    }
}
