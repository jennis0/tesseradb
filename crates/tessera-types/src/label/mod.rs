//! Access labels as Accumulo visibility expressions without negation.
//!
//! [`Label::parse`] is the one reader of a label, for the build and for a running service alike,
//! and for an item's label, a view's or a layer's `visibility`, an artifact's own label and a
//! default. It trims the text, refuses what the grammar refuses, and returns the label normalised:
//! nested operators of one kind flattened, operands sorted and deduplicated, and absorption
//! applied, so that `a|(a&b)` is `a`. Equal labels then have equal [`Label::canonical`] text, which
//! is what is stored. Two equivalent labels that normalise differently stay two labels.
//!
//! A list of labels, where a declaration or an item carries one, admits a principal who satisfies
//! any one of them. The expressions have no negation, so a label is monotone in the terms held:
//! holding more terms admits a superset.
//!
//! The index over labels, and their evaluation from a credential's terms, are in `tessera-authz`.

mod normal;
mod parse;

use std::fmt;

/// The label every principal holds, valid only as a whole label.
pub const PUBLIC: &str = "public";
/// The word a layer writes as its artifacts' default visibility to give them the layer's own
/// `visibility`. It is never a label, and a term equal to it ignoring ASCII case is refused.
pub const INHERITED: &str = "inherited";

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
        if trimmed == PUBLIC {
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
            None => PUBLIC.to_owned(),
            Some(e) => normal::canonical(e),
        }
    }

    /// Whether a principal holding the terms `held` satisfies this label.
    pub fn satisfied_by(&self, held: &impl Fn(&str) -> bool) -> bool {
        self.0.as_ref().is_none_or(|e| e.satisfied_by(held))
    }

    /// Held terms whose conjunction satisfies this label, sorted and distinct, or `None` where
    /// `held` does not satisfy it; `public` needs none. Of a disjunction, the first operand in
    /// the label's canonical order that `held` satisfies is taken, so the answer depends on the
    /// label and the terms held and on nothing else.
    pub fn witness(&self, held: &impl Fn(&str) -> bool) -> Option<Vec<&str>> {
        let mut out = Vec::new();
        if let Some(e) = &self.0 {
            if !e.witness(held, &mut out) {
                return None;
            }
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }
}

impl Expr {
    fn witness<'a>(&'a self, held: &impl Fn(&str) -> bool, out: &mut Vec<&'a str>) -> bool {
        match self {
            Expr::Term(t) => {
                out.push(t);
                held(t)
            }
            Expr::And(v) => v.iter().all(|e| e.witness(held, out)),
            Expr::Or(v) => {
                let mark = out.len();
                v.iter().any(|e| {
                    out.truncate(mark);
                    e.witness(held, out)
                })
            }
        }
    }

    fn satisfied_by(&self, held: &impl Fn(&str) -> bool) -> bool {
        match self {
            Expr::Term(t) => held(t),
            Expr::And(v) => v.iter().all(|e| e.satisfied_by(held)),
            Expr::Or(v) => v.iter().any(|e| e.satisfied_by(held)),
        }
    }

    /// Whether this is a term or a disjunction of terms.
    pub fn is_any_of(&self) -> bool {
        match self {
            Expr::Term(_) => true,
            Expr::Or(v) => v.iter().all(|e| matches!(e, Expr::Term(_))),
            Expr::And(_) => false,
        }
    }

    /// The operands of an operator, or the term itself.
    pub fn operands(&self) -> &[Expr] {
        match self {
            Expr::Term(_) => std::slice::from_ref(self),
            Expr::And(v) | Expr::Or(v) => v,
        }
    }
}

/// A label value read from data, trimmed. `None` where nothing is left, which is no label.
pub fn label_value(value: &str) -> Option<&str> {
    let label = value.trim();
    (!label.is_empty()).then_some(label)
}

/// A row's label values, each trimmed, with the empty ones dropped. An empty result is no label.
pub fn label_values<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    values.into_iter().filter_map(label_value).collect()
}

/// A label declared where one access label is expected, as it is stored: its canonical text.
/// `key` names the setting in the refusal.
pub fn declared_label(key: &str, word: &str) -> Result<String, String> {
    Label::parse(word, DEFAULT_MAX_NODES)
        .map(|label| label.canonical())
        .map_err(|e| format!("`{key}`: {e}"))
}

/// Whether a declared word is `public`, the label every principal holds.
pub fn is_public(word: &str) -> bool {
    word.trim() == PUBLIC
}

/// Whether a declared word is `inherited`, which a layer's artifact default may take.
pub fn is_inherited(word: &str) -> bool {
    word.trim() == INHERITED
}

/// The labels a view or a view group declares as its `visibility`, as they are stored: each
/// label's canonical text, or `None` for `public` alone or where none is declared. A principal
/// reaches the view by satisfying any one of them.
pub fn declared_visibility(declared: Option<&[String]>) -> Result<Option<Vec<String>>, String> {
    let Some(declared) = declared else {
        return Ok(None);
    };
    if declared.is_empty() {
        return Err(
            "`visibility = []` lists no labels; write `public`, or the labels a viewer needs \
             one of"
                .to_string(),
        );
    }
    let labels = declared
        .iter()
        .map(|label| declared_label("visibility", label))
        .collect::<Result<Vec<String>, String>>()?;
    if matches!(labels.as_slice(), [only] if only == PUBLIC) {
        return Ok(None);
    }
    if labels.iter().any(|label| label == PUBLIC) {
        return Err(
            "`visibility` lists `public` with other labels, and every principal holds \
             `public`; write it alone or remove it"
                .to_string(),
        );
    }
    Ok(Some(labels))
}

/// `point_visibility.default`, the label given to a point that carries none of its own, as it is
/// stored. `inherited` is refused, because a point has no layer to take a `visibility` from.
pub fn point_default(default: &str) -> Result<String, String> {
    declared_label("point_visibility.default", default)
}

/// An artifact's own access labels as they are stored, on the build and a running service alike:
/// each label's canonical text. A value that is empty once trimmed is dropped, and none left is no
/// label, which takes the layer's default.
pub fn artifact_access(labels: &[String]) -> Result<Vec<String>, String> {
    label_values(labels.iter().map(String::as_str))
        .into_iter()
        .map(|label| declared_label("access", label))
        .collect()
}

/// Whether a principal holding the terms `held` satisfies any of the stored `labels`. A label
/// that does not parse admits nobody.
pub fn admits<S: AsRef<str>>(labels: &[S], held: &impl Fn(&str) -> bool) -> bool {
    labels.iter().any(|label| {
        let label = label.as_ref();
        if label == PUBLIC {
            return true;
        }
        if !label.is_empty() && label.chars().all(parse::is_bare) {
            return held(label);
        }
        Label::parse(label, usize::MAX).is_ok_and(|label| label.satisfied_by(held))
    })
}

/// A term a credential presents, as it is held: trimmed, and `None` where nothing is left, where it
/// holds a control character, or where it is `public` in any case. Every session holds `public`
/// without presenting it.
pub fn held_term(term: &str) -> Option<&str> {
    let term = term.trim();
    let refused =
        term.is_empty() || term.chars().any(char::is_control) || term.eq_ignore_ascii_case(PUBLIC);
    (!refused).then_some(term)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_trimmed_and_an_empty_one_is_no_label() {
        assert_eq!(label_value(" red "), Some("red"));
        assert_eq!(label_value(" \t "), None);
        assert_eq!(label_values([" red ", "", " ", "a&b"]), vec!["red", "a&b"]);
    }

    #[test]
    fn a_declared_label_is_stored_as_its_canonical_text() {
        assert_eq!(declared_label("k", " red "), Ok("red".to_string()));
        assert_eq!(declared_label("k", " public "), Ok("public".to_string()));
        assert_eq!(declared_label("k", "b&(a)"), Ok("a&b".to_string()));
        for refused in ["", "   ", "inherited", " Inherited ", "a b", "Public"] {
            assert!(declared_label("k", refused).is_err(), "{refused:?}");
        }
        assert!(is_inherited(" inherited "));
        assert!(!is_inherited("Inherited"));
    }

    #[test]
    fn visibility_is_stored_as_its_labels_and_public_as_none() {
        let labels = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(declared_visibility(None), Ok(None));
        assert_eq!(declared_visibility(Some(&labels(&[" public "]))), Ok(None));
        assert_eq!(
            declared_visibility(Some(&labels(&[" finance ", "b&a"]))),
            Ok(Some(labels(&["finance", "a&b"])))
        );
        for refused in [
            labels(&[]),
            labels(&[""]),
            labels(&["finance", "public"]),
            labels(&["inherited"]),
            labels(&["a|b&c"]),
        ] {
            assert!(declared_visibility(Some(&refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn artifact_access_drops_empty_values_and_refuses_what_is_not_a_label() {
        let labels = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(artifact_access(&labels(&["", " "])), Ok(Vec::new()));
        assert_eq!(
            artifact_access(&labels(&[" b&a ", ""])),
            Ok(labels(&["a&b"]))
        );
        assert!(artifact_access(&labels(&["a,b"])).is_err());
    }

    #[test]
    fn stored_labels_admit_a_principal_who_satisfies_one() {
        let held = |t: &str| t == "a" || t == "c d";
        assert!(admits(&["x", "a"], &held));
        assert!(admits(&["a&\"c d\""], &held));
        assert!(!admits(&["a&b"], &held));
        assert!(admits(&["public"], &held));
        assert!(!admits::<&str>(&[], &held));
    }

    #[test]
    fn a_held_term_is_trimmed_and_never_public_or_a_control_character() {
        assert_eq!(held_term(" red "), Some("red"));
        assert_eq!(held_term("team a"), Some("team a"));
        for refused in ["", " ", "public", "PUBLIC", " Public ", "a\u{1}b"] {
            assert_eq!(held_term(refused), None, "{refused:?}");
        }
    }

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
    fn a_witness_is_the_first_satisfied_clause_in_canonical_order() {
        let l = label("(t&c)|(s&(b|a))");
        let all = |_: &str| true;
        assert_eq!(l.witness(&all), Some(vec!["c", "t"]));
        assert_eq!(
            l.witness(&|t| ["s", "b", "c"].contains(&t)),
            Some(vec!["b", "s"])
        );
        assert_eq!(l.witness(&|t| t == "s"), None);
        assert_eq!(label("y|x").witness(&all), Some(vec!["x"]));
        assert_eq!(label("public").witness(&|_| false), Some(vec![]));
    }

    #[test]
    fn a_label_is_satisfied_by_the_terms_it_needs() {
        let l = label("secret&(team_a|\"team b\")");
        assert!(l.satisfied_by(&|t| t == "secret" || t == "team b"));
        assert!(!l.satisfied_by(&|t| t == "secret"));
        assert!(label("public").satisfied_by(&|_| false));
    }
}
