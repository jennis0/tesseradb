//! What an item is indexed under, and the DAG over the labels indexed under a key of their own.

use rustc_hash::FxHashMap;
use tessera_access::{Expr, Label, LabelId, Labels, Scratch, Shape, DEFAULT_MAX_NODES};
use tessera_types::TermId;

use crate::dict::PUBLIC_LABEL;

/// The first byte of a key that indexes one label holding a conjunction. A term holding a control
/// character is refused in a label and dropped from a credential, so no term is such a key.
pub const COMPOUND_KEY: u8 = 0;

/// The dictionary keys an item carrying `labels` is indexed under, sorted and distinct. A
/// principal is admitted when it holds one of the terms among them, or satisfies the label behind
/// one of the other keys. An item admits a principal who satisfies any one of its labels.
///
/// Every reader of an item's labels calls this: the build, for each distinct label of its access
/// column, and ingest, for each row's `access`. A label that does not parse is refused, naming it.
pub fn index_keys<'a>(labels: impl IntoIterator<Item = &'a str>) -> Result<Vec<Vec<u8>>, String> {
    let mut keys = Vec::new();
    for text in labels {
        let label = Label::parse(text, DEFAULT_MAX_NODES)
            .map_err(|e| format!("the access label {text:?} is refused: {e}"))?;
        match (label.shape(), label.expr()) {
            (Shape::Public, _) | (_, None) => keys.push(PUBLIC_LABEL.to_vec()),
            (Shape::AnyOf, Some(e)) => keys.extend(e.operands().iter().filter_map(|t| match t {
                Expr::Term(t) => Some(t.as_bytes().to_vec()),
                _ => None,
            })),
            (Shape::Compound, Some(_)) => {
                let mut key = vec![COMPOUND_KEY];
                key.extend_from_slice(label.canonical().as_bytes());
                keys.push(key);
            }
        }
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

/// The canonical text of the label `key` indexes, where `key` indexes one label of its own.
pub fn label_of_key(key: &[u8]) -> Option<&str> {
    match key.split_first() {
        Some((&COMPOUND_KEY, text)) => std::str::from_utf8(text).ok(),
        _ => None,
    }
}

/// The labels a dictionary indexes under a key of their own, compiled into one shared DAG, each
/// known by its key's ordinal.
///
/// The DAG's leaves are terms by name, apart from the dictionary: a term that only appears inside
/// such labels has no posting and needs no ordinal.
#[derive(Clone, Default)]
pub struct LabelIndex {
    labels: Labels,
    /// Per label id, the ordinal of its key.
    key: Vec<TermId>,
    /// Per key ordinal, its label id.
    by_key: FxHashMap<TermId, LabelId>,
}

impl LabelIndex {
    /// Records the dictionary entry `descriptor` at `ordinal`. An entry that is not a label's own
    /// key is a term, and is not recorded. A key whose text does not parse is not recorded, so
    /// nobody satisfies it.
    pub(crate) fn add(&mut self, ordinal: TermId, descriptor: &[u8]) {
        let Some(text) = label_of_key(descriptor) else {
            return;
        };
        let Ok(label) = Label::parse(text, usize::MAX) else {
            return;
        };
        let (Shape::Compound, Some(e)) = (label.shape(), label.expr()) else {
            return;
        };
        let id = self.labels.intern(e);
        if id.raw() as usize == self.key.len() {
            self.key.push(ordinal);
        }
        self.by_key.insert(ordinal, id);
    }

    /// How many labels are indexed under a key of their own.
    pub fn len(&self) -> usize {
        self.key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.key.is_empty()
    }

    /// Whether `ordinal` is a label's own key rather than a term.
    pub fn is_label_key(&self, ordinal: TermId) -> bool {
        self.by_key.contains_key(&ordinal)
    }

    /// Appends to `out` the key of every label that a principal holding the terms `held`
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

    /// Whether a principal holding the terms `held` satisfies the label keyed at `ordinal`.
    /// `false` for an ordinal that is not a label's own key.
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
    fn each_shape_is_indexed_its_own_way() {
        assert_eq!(keys(&["public"]), ["public"]);
        assert_eq!(keys(&["b|a|\"c d\""]), ["a", "b", "c d"]);
        assert_eq!(keys(&["a|(a&b)"]), ["a"]);
        assert_eq!(keys(&["b&(a)"]), ["#a&b"]);
        assert_eq!(keys(&["x", "b&a", "x|y"]), ["#a&b", "x", "y"]);
        assert!(index_keys(["a b"]).is_err());
        assert!(index_keys([""]).is_err());
        assert!(index_keys(["inherited"]).is_err());
        assert!(index_keys(Vec::<&str>::new()).unwrap().is_empty());
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
    fn authorise_lists_the_keys_of_the_satisfied_labels() {
        let (index, ordinals) = index(&["s&(a|b)", "a&e", "x"]);
        assert_eq!(index.len(), 2);
        assert!(index.is_label_key(ordinals[0]));
        assert!(!index.is_label_key(ordinals[2]));
        let mut out = Vec::new();
        index.authorise(["s", "b", "unknown"], &mut out);
        assert_eq!(out, vec![ordinals[0]]);
        out.clear();
        index.authorise(["a", "e", "s"], &mut out);
        out.sort_unstable();
        assert_eq!(out, ordinals[..2]);
    }

    #[test]
    fn a_label_key_is_satisfied_by_the_terms_its_label_needs() {
        let (index, ordinals) = index(&["(s&(a|b))|(t&c)", "x"]);
        assert!(index.satisfied(ordinals[0], &|t| ["s", "b", "c"].contains(&t)));
        assert!(!index.satisfied(ordinals[0], &|t| t == "s"));
        assert!(
            !index.satisfied(ordinals[1], &|_| true),
            "a term is not a label key"
        );
    }

    #[test]
    fn a_key_whose_text_is_not_a_compound_label_indexes_nothing() {
        let mut index = LabelIndex::default();
        index.add(TermId::new(1), b"\0a b");
        index.add(TermId::new(2), b"\0a");
        index.add(TermId::new(3), b"plain");
        assert!(index.is_empty());
    }
}
