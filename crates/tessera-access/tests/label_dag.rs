//! Random labels over eight terms, written with random brackets, evaluated through the DAG and
//! against the tree they were written from. Some terms differ only by a space inside their quotes.
//! The tree is evaluated by direct recursion here, without parsing or normalising, so agreement
//! checks the parser, normalisation, hash-consing, the bottom-up pass, top-down evaluation and the
//! witness together.
use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tessera_access::{Label, LabelId, Labels, Scratch, DEFAULT_MAX_NODES};

const TERMS: [&str; 8] = ["a", "b.c", "d e", "f", "g:h", "\"i\\", "a ", " f"];

enum Tree {
    Term(usize),
    And(Vec<Tree>),
    Or(Vec<Tree>),
}

fn random_tree(rng: &mut StdRng, depth: u32) -> Tree {
    if depth == 0 || rng.gen_bool(0.3) {
        return Tree::Term(rng.gen_range(0..TERMS.len()));
    }
    let operands = (0..rng.gen_range(1..=4))
        .map(|_| random_tree(rng, depth - 1))
        .collect();
    if rng.gen_bool(0.5) {
        Tree::And(operands)
    } else {
        Tree::Or(operands)
    }
}

fn space(rng: &mut StdRng) -> &'static str {
    ["", "", " ", "\t", "  "][rng.gen_range(0..5)]
}

fn term_text(t: usize) -> String {
    if TERMS[t].contains(['"', '\\', ' ']) {
        let escaped = TERMS[t].replace('\\', "\\\\").replace('"', "\\\"");
        return format!("\"{escaped}\"");
    }
    TERMS[t].to_owned()
}

/// Writes `tree`, bracketing an operand at random wherever the brackets
/// can be left out. Returns the text and the operator written outside any bracket.
fn text(tree: &Tree, rng: &mut StdRng) -> (String, Option<char>) {
    let (op, v) = match tree {
        Tree::Term(t) => return (term_text(*t), None),
        Tree::And(v) => ('&', v),
        Tree::Or(v) => ('|', v),
    };
    let mut out = String::new();
    let mut top = (v.len() > 1).then_some(op);
    for (i, operand) in v.iter().enumerate() {
        if i > 0 {
            out.push(op);
        }
        let (inner, inner_top) = text(operand, rng);
        let may_be_bare = v.len() == 1 || inner_top.is_none_or(|t| t == op);
        if may_be_bare && rng.gen_bool(0.5) {
            out.push_str(&inner);
            top = top.or(inner_top);
        } else {
            out.push_str(&format!("({inner})"));
        }
    }
    (out, top)
}

fn holds(tree: &Tree, held: u32) -> bool {
    match tree {
        Tree::Term(t) => held & (1 << t) != 0,
        Tree::And(v) => v.iter().all(|c| holds(c, held)),
        Tree::Or(v) => v.iter().any(|c| holds(c, held)),
    }
}

fn held_names(held: u32) -> Vec<&'static str> {
    (0..TERMS.len())
        .filter(|t| held & (1 << t) != 0)
        .map(|t| TERMS[t])
        .collect()
}

struct Case {
    tree: Tree,
    text: String,
    label: Label,
    id: LabelId,
}

fn cases(labels: &mut Labels) -> Vec<Case> {
    let mut rng = StdRng::seed_from_u64(20260930);
    (0..600)
        .map(|_| {
            let tree = random_tree(&mut rng, 4);
            let (text, _) = text(&tree, &mut rng);
            let (before, after) = (space(&mut rng), space(&mut rng));
            let label = Label::parse(&format!("{before}{text}{after}"), DEFAULT_MAX_NODES)
                .unwrap_or_else(|e| panic!("{text:?}: {e}"));
            let id = labels.intern(label.expr().expect("the generator writes no `public`"));
            Case {
                tree,
                text,
                label,
                id,
            }
        })
        .collect()
}

#[test]
fn authorise_agrees_with_direct_evaluation_for_every_credential() {
    let mut labels = Labels::new();
    let cases = cases(&mut labels);
    let mut scratch = Scratch::default();
    for held in 0u32..1 << TERMS.len() {
        let mut out = Vec::new();
        labels.authorise(held_names(held), &mut scratch, &mut out);
        for case in &cases {
            let expected = holds(&case.tree, held);
            assert_eq!(
                out.contains(&case.id),
                expected,
                "{} held={held:08b}",
                case.text
            );
        }
    }
}

#[test]
fn every_evaluation_agrees_with_direct_evaluation() {
    let mut labels = Labels::new();
    let cases = cases(&mut labels);
    for held in 0u32..1 << TERMS.len() {
        let names = held_names(held);
        let by_name = |t: &str| names.contains(&t);
        for case in &cases {
            let expected = holds(&case.tree, held);
            let context = format!("{} held={held:08b}", case.text);
            assert_eq!(labels.satisfied(case.id, &by_name), expected, "{context}");
            assert_eq!(case.label.satisfied_by(&by_name), expected, "{context}");
            let only_witness = case.label.witness(&by_name);
            assert_eq!(only_witness.is_some(), expected, "{context}");
            let only_witness = only_witness.unwrap_or_default();
            assert!(only_witness.iter().all(|t| by_name(t)), "{context}");
            assert!(
                !expected || labels.satisfied(case.id, &|t| only_witness.contains(&t)),
                "{context}"
            );
        }
    }
}

#[test]
fn equal_normal_forms_share_a_label_id_and_the_canonical_text_round_trips() {
    let mut labels = Labels::new();
    let cases = cases(&mut labels);
    let mut by_text: HashMap<String, LabelId> = HashMap::new();
    for case in &cases {
        let canonical = case.label.canonical();
        assert_eq!(
            Label::parse(&canonical, DEFAULT_MAX_NODES).as_ref(),
            Ok(&case.label)
        );
        assert_eq!(*by_text.entry(canonical).or_insert(case.id), case.id);
    }
    assert_eq!(labels.len(), by_text.len());
}

#[test]
fn a_space_inside_quotes_makes_another_term() {
    let mut labels = Labels::new();
    let quoted = Label::parse("\"a \"", DEFAULT_MAX_NODES).unwrap();
    let bare = Label::parse(" a ", DEFAULT_MAX_NODES).unwrap();
    assert_ne!(quoted, bare);
    let (quoted, bare) = (
        labels.intern(quoted.expr().unwrap()),
        labels.intern(bare.expr().unwrap()),
    );
    assert_ne!(quoted, bare);
    assert!(labels.satisfied(bare, &|t| t == "a"));
    assert!(!labels.satisfied(quoted, &|t| t == "a"));
}
