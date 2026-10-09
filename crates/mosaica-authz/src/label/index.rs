//! What an item is indexed under, and the DAG over the conjunctions indexed under a key of their
//! own.

use rustc_hash::FxHashMap;
use mosaica_access::{disjuncts, Expr, Label, LabelId, Labels, Scratch, DEFAULT_MAX_NODES};
use mosaica_types::TermId;

use crate::dict::PUBLIC_LABEL;

/// The first byte of a key that indexes one conjunction. A term holding a control character is
/// refused in a label and dropped from a credential, so no term is such a key.
pub const COMPOUND_KEY: u8 = 0;

/// The dictionary keys an item carrying `labels` is indexed under, sorted and distinct.
///
/// The labels are read as one disjunction and normalised ([`disjuncts`]). Each of its operands is
/// a key: a term under itself, and a conjunction under [`COMPOUND_KEY`] followed by its canonical
/// text. `public` is a key of its own beside them. A principal is admitted when it holds one of the
/// terms among the keys, or satisfies the conjunction behind one of the others, which is when it
/// satisfies one of the labels. A list of labels and one label writing the same disjunction are
/// indexed under the same keys.
///
/// Every reader of an item's labels calls this: the build, for each distinct label of its access
/// column and for a row whose labels are several and name a conjunction, and ingest, for each row's
/// `access`. A label that does not parse is refused, naming it.
pub fn index_keys<'a>(labels: impl IntoIterator<Item = &'a str>) -> Result<Vec<Vec<u8>>, String> {
    let mut keys = Vec::new();
    let mut exprs = Vec::new();
    for text in labels {
        let label = Label::parse(text, DEFAULT_MAX_NODES)
            .map_err(|e| format!("the access label {text:?} is refused: {e}"))?;
        match label.expr() {
            None => keys.push(PUBLIC_LABEL.to_vec()),
            Some(e) => exprs.push(e.clone()),
        }
    }
    keys.extend(disjuncts(exprs).iter().map(|d| match d {
        Expr::Term(t) => t.as_bytes().to_vec(),
        conjunction => {
            let text = conjunction.canonical();
            let mut key = Vec::with_capacity(1 + text.len());
            key.push(COMPOUND_KEY);
            key.extend_from_slice(text.as_bytes());
            key
        }
    }));
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

/// The canonical text of the conjunction `key` indexes, where `key` is a conjunction's own key.
pub fn label_of_key(key: &[u8]) -> Option<&str> {
    match key.split_first() {
        Some((&COMPOUND_KEY, text)) => std::str::from_utf8(text).ok(),
        _ => None,
    }
}

/// The conjunctions a dictionary indexes under a key of their own, compiled into one shared DAG,
/// each known by its key's ordinal. Inside a conjunction a subexpression stays one node, which
/// every conjunction holding it shares.
///
/// The DAG's leaves are terms by name, apart from the dictionary: a term that only appears inside
/// a conjunction has no posting and needs no ordinal.
#[derive(Clone, Default)]
pub struct LabelIndex {
    labels: Labels,
    /// Per label id, the ordinal of its key.
    key: Vec<TermId>,
    /// Per key ordinal, its label id.
    by_key: FxHashMap<TermId, LabelId>,
}

impl LabelIndex {
    /// Records the dictionary entry `descriptor` at `ordinal`. An entry that is not a conjunction's
    /// own key is a term, and is not recorded. A key whose text is not a conjunction is not
    /// recorded, so nobody satisfies it.
    pub(crate) fn add(&mut self, ordinal: TermId, descriptor: &[u8]) {
        let Some(text) = label_of_key(descriptor) else {
            return;
        };
        let Ok(label) = Label::parse(text, usize::MAX) else {
            return;
        };
        let Some(e @ Expr::And(_)) = label.expr() else {
            return;
        };
        let id = self.labels.intern(e);
        if id.raw() as usize == self.key.len() {
            self.key.push(ordinal);
        }
        self.by_key.insert(ordinal, id);
    }

    /// How many conjunctions are indexed under a key of their own.
    pub fn len(&self) -> usize {
        self.key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.key.is_empty()
    }

    /// Whether `ordinal` is a conjunction's own key rather than a term.
    pub fn is_conjunction_key(&self, ordinal: TermId) -> bool {
        self.by_key.contains_key(&ordinal)
    }

    /// Appends to `out` the key of every conjunction that a principal holding the terms `held`
    /// satisfies. The pass visits only the part of the DAG `held` reaches.
    pub fn authorise<'a>(&self, held: impl IntoIterator<Item = &'a str>, out: &mut Vec<TermId>) {
        if self.labels.is_empty() {
            return;
        }
        let mut satisfied = Vec::new();
        self.labels
            .authorise(held, &mut Scratch::default(), &mut satisfied);
        out.extend(satisfied.iter().map(|id| self.key[id.raw() as usize]));
    }

    /// Whether a principal holding the terms `held` satisfies the conjunction keyed at `ordinal`.
    /// `false` for an ordinal that is not a conjunction's own key.
    pub fn satisfied(&self, ordinal: TermId, held: &impl Fn(&str) -> bool) -> bool {
        self.by_key
            .get(&ordinal)
            .is_some_and(|&id| self.labels.satisfied(id, held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(labels: &[&str]) -> Vec<String> {
        index_keys(labels.iter().copied())
            .unwrap()
            .into_iter()
            .map(|k| String::from_utf8(k).unwrap().replace('\0', "#"))
            .collect()
    }

    #[test]
    fn each_disjunct_is_indexed_on_its_own() {
        assert_eq!(keys(&["public"]), ["public"]);
        assert_eq!(keys(&["public", "a"]), ["a", "public"]);
        assert_eq!(keys(&["b|a|\"c d\""]), ["a", "b", "c d"]);
        assert_eq!(keys(&["a|(a&b)"]), ["a"]);
        assert_eq!(keys(&["b&(a)"]), ["#a&b"]);
        assert_eq!(keys(&["x", "b&a", "x|y"]), ["#a&b", "x", "y"]);
        assert_eq!(keys(&["a|(b&c)"]), ["#b&c", "a"]);
        assert_eq!(keys(&["(t&c)|(s&(b|a))"]), ["#c&t", "#s&(a|b)"]);
        assert_eq!(keys(&["a&(b|(c&d))"]), ["#a&(b|(c&d))"]);
        assert!(index_keys(["a b"]).is_err());
        assert!(index_keys([""]).is_err());
        assert!(index_keys(["inherited"]).is_err());
        assert!(index_keys(Vec::<&str>::new()).unwrap().is_empty());
    }

    #[test]
    fn a_list_of_labels_is_indexed_as_one_label_writing_the_same_disjunction() {
        for (list, one) in [
            (&["a", "b&c"][..], "a|(b&c)"),
            (&["a", "a&b"], "a|(a&b)"),
            (&["x|(y&z)", "y&z&w"], "x|(y&z)"),
            (&["(p&q)|r", "s"], "s|r|(q&p)"),
        ] {
            assert_eq!(keys(list), keys(&[one]), "{list:?}");
        }
    }

    fn index(labels: &[&str]) -> (LabelIndex, Vec<TermId>) {
        let mut index = LabelIndex::default();
        let mut ordinals = Vec::new();
        for (i, label) in labels.iter().enumerate() {
            let key = index_keys([*label]).unwrap().remove(0);
            index.add(TermId::new(i as u32 + 10), &key);
            ordinals.push(TermId::new(i as u32 + 10));
        }
        (index, ordinals)
    }

    #[test]
    fn authorise_lists_the_keys_of_the_satisfied_conjunctions() {
        let (index, ordinals) = index(&["s&(a|b)", "a&e", "x"]);
        assert_eq!(index.len(), 2);
        assert!(index.is_conjunction_key(ordinals[0]));
        assert!(!index.is_conjunction_key(ordinals[2]));
        let mut out = Vec::new();
        index.authorise(["s", "b", "unknown"], &mut out);
        assert_eq!(out, vec![ordinals[0]]);
        out.clear();
        index.authorise(["a", "e", "s"], &mut out);
        out.sort_unstable();
        assert_eq!(out, ordinals[..2]);
    }

    #[test]
    fn a_conjunction_key_is_satisfied_by_the_terms_its_conjunction_needs() {
        let (index, ordinals) = index(&["s&(a|(t&c))", "x"]);
        assert!(index.satisfied(ordinals[0], &|t| ["s", "t", "c"].contains(&t)));
        assert!(!index.satisfied(ordinals[0], &|t| ["s", "t"].contains(&t)));
        assert!(
            !index.satisfied(ordinals[1], &|_| true),
            "a term is not a conjunction key"
        );
    }

    #[test]
    fn a_key_whose_text_is_not_a_conjunction_indexes_nothing() {
        let mut index = LabelIndex::default();
        index.add(TermId::new(1), b"\0a b");
        index.add(TermId::new(2), b"\0a");
        index.add(TermId::new(3), b"plain");
        index.add(TermId::new(4), b"\0a|(b&c)");
        assert!(index.is_empty());
    }
}
