//! The rows the rule refused, counted by file and reason, with a few of the values they carried.

use std::collections::{BTreeMap, BinaryHeap};

use tessera_lifecycle::resolve::Refusal;

/// The rows of one file refused for one reason.
///
/// **The values are the file's own**, as `field = value` for each unique field the row carried,
/// and a row that carried none is named by its position in the file. No internal id or number is
/// ever part of a report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RefusedRows {
    /// The file.
    pub source: String,
    /// The block reading it: `view 'world'`, `layer 'taxonomy' members`.
    pub object: String,
    /// `names_two_items`, `one_item_twice`, `one_value_twice`, `names_no_item` or
    /// `unknown_tessera_id`.
    pub reason: String,
    pub rows: u64,
    /// The values of the first ten refused rows in the file, each once.
    pub values: Vec<String>,
}

/// How many rows a report names by their values, for each file and reason.
pub(crate) const SAMPLES: usize = 10;

/// One file's refusals as the rule finds them, in any order.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    /// For each reason, how many rows and the first rows in the file.
    reasons: BTreeMap<&'static str, (u64, BinaryHeap<u64>)>,
}

impl Tally {
    pub(crate) fn refuse(&mut self, reason: &'static str, row: u64) {
        let (count, first) = self.reasons.entry(reason).or_default();
        *count += 1;
        if first.len() < SAMPLES {
            first.push(row);
        } else if first.peek().is_some_and(|&last| row < last) {
            first.pop();
            first.push(row);
        }
    }

    pub(crate) fn refuse_row(&mut self, refusal: &Refusal, row: u64) {
        self.refuse(refusal.reason(), row);
    }

    /// Every sampled row, ascending, for the caller to read the values of.
    pub(crate) fn sampled(&self) -> Vec<u64> {
        let mut rows: Vec<u64> = self
            .reasons
            .values()
            .flat_map(|(_, first)| first.iter().copied())
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// The report entries, a row's values found by `value_of`: each value once, in file order.
    pub(crate) fn finish(
        self,
        source: &str,
        object: &str,
        value_of: impl Fn(u64) -> String,
    ) -> Vec<RefusedRows> {
        self.reasons
            .into_iter()
            .map(|(reason, (rows, first))| {
                let mut values: Vec<String> = Vec::new();
                for text in first.into_sorted_vec().into_iter().map(&value_of) {
                    if !values.contains(&text) {
                        values.push(text);
                    }
                }
                RefusedRows {
                    source: source.to_string(),
                    object: object.to_string(),
                    reason: reason.to_string(),
                    rows,
                    values,
                }
            })
            .collect()
    }
}

/// The report as `tessera build` prints it: one line per file and reason.
pub fn describe(refused: &[RefusedRows]) -> Vec<String> {
    refused
        .iter()
        .map(|entry| {
            format!(
                "{} ({}): {} row(s) refused, {}{}",
                entry.object,
                entry.source,
                crate::thousands(entry.rows),
                reason_text(&entry.reason),
                match entry.values.is_empty() {
                    true => String::new(),
                    false => format!(": {}", entry.values.join("; ")),
                }
            )
        })
        .collect()
}

fn reason_text(reason: &str) -> &'static str {
    match reason {
        "names_two_items" => "each names two items",
        "one_item_twice" => "each names an item an earlier row of the file names",
        "one_value_twice" => "each gives a unique value an earlier row of the file gives",
        "names_no_item" => "each names no item",
        "unknown_tessera_id" => "each carries a tessera_id, which names no item at a build",
        _ => "refused",
    }
}
