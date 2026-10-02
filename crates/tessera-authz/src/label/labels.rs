//! Label ids, and the evaluation of a label against a set of held terms.

use rustc_hash::FxHashMap;
use tessera_types::{LabelId, TermId};

use super::dag::{Dag, Scratch};
use super::{Expr, Label, Shape};

#[derive(Clone)]
enum Entry {
    Public,
    /// The label's term ids, sorted.
    AnyOf(Box<[TermId]>),
    /// The label's root in the DAG.
    Compound(u32),
}

/// Every distinct label, each with its label id. Label ids are issued in order from zero.
#[derive(Clone)]
pub struct Labels {
    entries: Vec<Entry>,
    any_of: FxHashMap<Box<[TermId]>, LabelId>,
    public: Option<LabelId>,
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
            entries: Vec::new(),
            any_of: FxHashMap::default(),
            public: None,
            dag: Dag::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The label id of `label`, issuing a new one if no equal label has one. `term` gives the id
    /// of each term the label names, and must give distinct terms distinct ids.
    pub fn intern(&mut self, label: &Label, mut term: impl FnMut(&str) -> TermId) -> LabelId {
        match label.expr() {
            None => self.intern_public(),
            Some(e) if label.shape() == Shape::AnyOf => self.intern_any_of(e, &mut term),
            Some(e) => self.intern_compound(e, &mut term),
        }
    }

    fn issue(&mut self, entry: Entry) -> LabelId {
        let id = u32::try_from(self.entries.len()).expect("fewer than 2^32 labels");
        self.entries.push(entry);
        LabelId::new(id)
    }

    fn intern_public(&mut self) -> LabelId {
        if let Some(id) = self.public {
            return id;
        }
        let id = self.issue(Entry::Public);
        self.public = Some(id);
        id
    }

    fn intern_any_of(&mut self, e: &Expr, term: &mut impl FnMut(&str) -> TermId) -> LabelId {
        let mut terms: Vec<TermId> = e
            .operands()
            .iter()
            .filter_map(|t| match t {
                Expr::Term(t) => Some(term(t)),
                _ => None,
            })
            .collect();
        terms.sort_unstable();
        terms.dedup();
        if let Some(&id) = self.any_of.get(terms.as_slice()) {
            return id;
        }
        let terms = terms.into_boxed_slice();
        let id = self.issue(Entry::AnyOf(terms.clone()));
        self.any_of.insert(terms, id);
        id
    }

    fn intern_compound(&mut self, e: &Expr, term: &mut impl FnMut(&str) -> TermId) -> LabelId {
        let root = self.dag.intern(e, term);
        if let Some(id) = self.dag.root_label(root) {
            return id;
        }
        let id = self.issue(Entry::Compound(root));
        self.dag.set_root_label(root, id);
        id
    }

    /// Which index serves `id`. Panics if `id` was not issued here.
    pub fn shape(&self, id: LabelId) -> Shape {
        match self.entries[id.raw() as usize] {
            Entry::Public => Shape::Public,
            Entry::AnyOf(_) => Shape::AnyOf,
            Entry::Compound(_) => Shape::Compound,
        }
    }

    /// The terms of a label shaped [`Shape::AnyOf`], sorted. Empty for any other label.
    pub fn any_of(&self, id: LabelId) -> &[TermId] {
        match &self.entries[id.raw() as usize] {
            Entry::AnyOf(terms) => terms,
            Entry::Public | Entry::Compound(_) => &[],
        }
    }

    /// Appends to `out` every compound label that a principal holding `held` satisfies. A label
    /// shaped [`Shape::AnyOf`] is satisfied exactly by holding one of its terms, which the
    /// postings of the held terms answer, so it is not listed.
    pub fn authorise(&self, held: &[TermId], scratch: &mut Scratch, out: &mut Vec<LabelId>) {
        self.dag.authorise(held, scratch, out);
    }

    /// Whether a principal holding the terms `held` satisfies label `id`. The label alone
    /// is evaluated, so no authorised set is built.
    pub fn satisfied(&self, id: LabelId, held: &impl Fn(TermId) -> bool) -> bool {
        match &self.entries[id.raw() as usize] {
            Entry::Public => true,
            Entry::AnyOf(terms) => terms.iter().any(|&t| held(t)),
            Entry::Compound(root) => self.dag.eval(*root, held),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::DEFAULT_MAX_NODES;
    use std::collections::HashMap;

    struct Fixture {
        labels: Labels,
        dict: HashMap<String, TermId>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                labels: Labels::new(),
                dict: HashMap::new(),
            }
        }

        fn add(&mut self, text: &str) -> LabelId {
            let label = Label::parse(text, DEFAULT_MAX_NODES).unwrap();
            let dict = &mut self.dict;
            self.labels.intern(&label, |t| {
                let next = TermId::new(dict.len() as u32);
                *dict.entry(t.to_owned()).or_insert(next)
            })
        }

        fn holds(&self, terms: &[&str]) -> impl Fn(TermId) -> bool {
            let held: Vec<TermId> = terms
                .iter()
                .filter_map(|t| self.dict.get(*t).copied())
                .collect();
            move |t| held.contains(&t)
        }
    }

    #[test]
    fn equal_labels_share_an_id_and_each_shape_is_kept() {
        let mut f = Fixture::new();
        let ids = ["public", "a|b", "b|a|a", "a&b", "b&(a)", "a", " public"].map(|t| f.add(t));
        assert_eq!(ids.map(|id| id.raw()), [0, 1, 1, 2, 2, 3, 0]);
        assert_eq!(f.labels.len(), 4);
        let shapes = [Shape::Public, Shape::AnyOf, Shape::Compound, Shape::AnyOf];
        assert_eq!(
            (0..4)
                .map(|i| f.labels.shape(LabelId::new(i)))
                .collect::<Vec<_>>(),
            shapes
        );
        assert_eq!(f.labels.any_of(ids[1]).len(), 2);
    }

    #[test]
    fn compound_labels_sharing_a_subexpression_keep_their_own_meaning() {
        let mut f = Fixture::new();
        let first = f.add("s&(a|b)");
        assert_eq!(f.add("(b|a)&s"), first);
        let other = f.add("s|(a&b)");
        let third = f.add("(a|b)&e");
        assert_eq!([first, other, third].map(|id| id.raw()), [0, 1, 2]);
        assert!(f.labels.satisfied(first, &f.holds(&["s", "b"])));
        assert!(!f.labels.satisfied(first, &f.holds(&["a", "b"])));
        assert!(f.labels.satisfied(other, &f.holds(&["a", "b"])));
        assert!(!f.labels.satisfied(third, &f.holds(&["s", "a"])));
        assert!(f.labels.satisfied(third, &f.holds(&["b", "e"])));
    }

    #[test]
    fn authorise_lists_the_satisfied_compound_labels() {
        let mut f = Fixture::new();
        let compound = f.add("secret&(team_a|team_b)");
        f.add("team_a");
        let other = f.add("team_a&eu");
        let mut out = Vec::new();
        let held: Vec<TermId> = ["secret", "team_b"].map(|t| f.dict[t]).to_vec();
        f.labels.authorise(&held, &mut Scratch::default(), &mut out);
        assert_eq!(out, vec![compound]);
        assert!(!f.labels.satisfied(other, &f.holds(&["team_a"])));
        assert!(f.labels.satisfied(other, &f.holds(&["team_a", "eu"])));
    }

    #[test]
    fn public_is_satisfied_by_every_principal_and_listed_by_no_authorise() {
        let mut f = Fixture::new();
        let public = f.add("public");
        f.add("a");
        assert!(f.labels.satisfied(public, &f.holds(&[])));
        assert!(f.labels.satisfied(public, &f.holds(&["a"])));
        let mut out = Vec::new();
        f.labels
            .authorise(&[f.dict["a"]], &mut Scratch::default(), &mut out);
        assert!(out.is_empty());
    }
}
