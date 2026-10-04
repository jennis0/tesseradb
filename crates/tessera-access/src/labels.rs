//! Label ids, and the evaluation of a label against a set of held terms.

use crate::dag::{Dag, Scratch};
use crate::{Expr, LabelId};

/// Distinct labels, each with its label id, in one shared DAG. Label ids are issued in order from
/// zero.
#[derive(Clone)]
pub struct Labels {
    /// Per label id, the label's root in the DAG.
    root: Vec<u32>,
    dag: Dag,
}

impl Default for Labels {
    fn default() -> Self {
        Self::new()
    }
}

impl Labels {
    pub fn new() -> Self {
        Labels {
            root: Vec::new(),
            dag: Dag::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.root.len()
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    /// The label id of the normalised expression `e`, issuing a new one if no equal expression
    /// has one.
    pub fn intern(&mut self, e: &Expr) -> LabelId {
        let root = self.dag.intern(e);
        if let Some(id) = self.dag.root_label(root) {
            return id;
        }
        let id = LabelId::new(u32::try_from(self.root.len()).expect("fewer than 2^32 labels"));
        self.root.push(root);
        self.dag.set_root_label(root, id);
        id
    }

    /// Appends to `out` every label that a principal holding the terms `held` satisfies. The pass
    /// visits only the part of the DAG `held` reaches.
    pub fn authorise<'a>(
        &self,
        held: impl IntoIterator<Item = &'a str>,
        scratch: &mut Scratch,
        out: &mut Vec<LabelId>,
    ) {
        self.dag.authorise(held, scratch, out);
    }

    /// Whether a principal holding the terms `held` satisfies label `id`. The label alone is
    /// evaluated. Panics if `id` was not issued here.
    pub fn satisfied(&self, id: LabelId, held: &impl Fn(&str) -> bool) -> bool {
        self.dag.eval(self.root[id.raw() as usize], held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Label, DEFAULT_MAX_NODES};

    fn add(labels: &mut Labels, text: &str) -> LabelId {
        let label = Label::parse(text, DEFAULT_MAX_NODES).unwrap();
        labels.intern(label.expr().unwrap())
    }

    fn holds<'a>(terms: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |t| terms.contains(&t)
    }

    #[test]
    fn equal_labels_share_an_id() {
        let mut labels = Labels::new();
        let ids = ["a|b", "b|a|a", "a&b", "b&(a)", "a", " a "].map(|t| add(&mut labels, t));
        assert_eq!(ids.map(|id| id.raw()), [0, 0, 1, 1, 2, 2]);
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn labels_sharing_a_subexpression_keep_their_own_meaning() {
        let mut labels = Labels::new();
        let first = add(&mut labels, "s&(a|b)");
        assert_eq!(add(&mut labels, "(b|a)&s"), first);
        let other = add(&mut labels, "s|(a&b)");
        let third = add(&mut labels, "(a|b)&e");
        assert_eq!([first, other, third].map(|id| id.raw()), [0, 1, 2]);
        assert!(labels.satisfied(first, &holds(&["s", "b"])));
        assert!(!labels.satisfied(first, &holds(&["a", "b"])));
        assert!(labels.satisfied(other, &holds(&["a", "b"])));
        assert!(!labels.satisfied(third, &holds(&["s", "a"])));
        assert!(labels.satisfied(third, &holds(&["b", "e"])));
    }

    #[test]
    fn authorise_lists_the_satisfied_labels() {
        let mut labels = Labels::new();
        let compound = add(&mut labels, "secret&(team_a|team_b)");
        let term = add(&mut labels, "team_b");
        let other = add(&mut labels, "team_a&eu");
        let mut out = Vec::new();
        labels.authorise(["secret", "team_b", "unknown"], &mut Scratch::default(), &mut out);
        out.sort_unstable();
        assert_eq!(out, vec![compound, term]);
        assert!(!labels.satisfied(other, &holds(&["team_a"])));
        assert!(labels.satisfied(other, &holds(&["team_a", "eu"])));
    }
}
