use croaring::Bitmap;
use tessera_filter::{ColumnPostings, SortedDict};
use tessera_types::AttrLocalId;

/// Does `document`'s token sequence contain `phrase`'s contiguously and in order?
///
/// Both sides come from the same analyser, so this compares the index's own units rather than raw
/// text: a phrase found here is one whose words the index holds adjacent. A repeated word is not
/// collapsed on either side: `"the the"` matches a document that says it twice in a row and not
/// one that says it once, which distinguishes a phrase from the word-bag `match` already answers.
pub(in crate::filter) fn contains_phrase(document: &[String], phrase: &[String]) -> bool {
    // An empty phrase is refused upstream. `windows` panics on a zero length and yields nothing
    // past the document's end, so both are stated here rather than left to it.
    if phrase.is_empty() || phrase.len() > document.len() {
        return false;
    }
    document.windows(phrase.len()).any(|w| w == phrase)
}

/// `match` over one text column: intersect the tokens' postings inside the candidate, or count
/// them where fewer than all are required.
///
/// Every posting is masked as it is read, before anything is unioned or counted, so no bitmap that
/// reaches the answer ever holds an entity outside the candidate. An unresolved token contributes
/// an empty posting rather than short-circuiting: under m-of-n it must still consume its place in
/// the count, or `match` of three tokens with `minimum = 2` would silently become a two-token
/// question when one is absent from the corpus.
pub(in crate::filter) fn text_match(
    dict: &SortedDict,
    postings: &ColumnPostings,
    tokens: &[String],
    minimum: u32,
    candidate: &Bitmap,
) -> std::io::Result<Bitmap> {
    if tokens.is_empty() || minimum == 0 {
        // An empty `match` matches nothing rather than everything, the same reading `any_of([])`
        // takes.
        return Ok(Bitmap::new());
    }
    // `minimum` counts distinct tokens, the caller's query already deduplicated, so a request for
    // four of two words is one no item can meet.
    if minimum as usize > tokens.len() {
        return Ok(Bitmap::new());
    }
    // Each token's ordinal, resolved before any posting is read, so the conjunction below can
    // narrow token by token without a second dictionary pass.
    let mut ordinals: Vec<Option<u32>> = Vec::with_capacity(tokens.len());
    for token in tokens {
        ordinals
            .push(dict.resolve(token).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?);
    }

    // Plain `match`: one running set, narrowed token by token, rather than one bitmap per token
    // intersected at the end. The set only shrinks and starts at the candidate, so the answer
    // stays inside it from the first step; each step after the first also works against an
    // already-narrowed set, cheaper by the same container-count reasoning as any bitmap operation.
    if minimum as usize == tokens.len() {
        // The candidate is borrowed for the first step, never cloned: a clone of a million-entity
        // mask costs ~800 ns, most of a one-word query's whole budget.
        let mut live: Option<Bitmap> = None;
        for ordinal in &ordinals {
            match live.as_mut() {
                None => {
                    live = Some(match ordinal {
                        Some(ordinal) => postings.narrow(AttrLocalId::new(*ordinal), candidate)?,
                        None => Bitmap::new(),
                    });
                }
                // Every token after it narrows the running set in place, which allocates nothing.
                Some(live) => match ordinal {
                    Some(ordinal) => postings.narrow_inplace(AttrLocalId::new(*ordinal), live)?,
                    None => live.clear(),
                },
            }
        }
        let mut out = live.unwrap_or_default();
        out.run_optimize();
        return Ok(out);
    }

    // m-of-n needs every token's answer at once, a count cannot be accumulated into one set, so it
    // holds n bitmaps, each bounded by the candidate rather than the posting.
    let mut per_token: Vec<Bitmap> = Vec::with_capacity(tokens.len());
    for ordinal in &ordinals {
        per_token.push(match ordinal {
            Some(ordinal) => postings.narrow(AttrLocalId::new(*ordinal), candidate)?,
            None => Bitmap::new(),
        });
    }

    // How many of the tokens each candidate entity carries, counted over the union rather than the
    // candidate, so the work is the postings' size and not the mask's.
    let mut union = Bitmap::new();
    for token in &per_token {
        union |= token;
    }
    let mut out = Bitmap::new();
    for entity in union.iter() {
        let hits = per_token.iter().filter(|t| t.contains(entity)).count();
        if hits as u32 >= minimum {
            out.add(entity);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod phrase_tests {
    use super::contains_phrase;

    fn t(words: &str) -> Vec<String> {
        if words.is_empty() {
            return Vec::new();
        }
        words.split(' ').map(str::to_string).collect()
    }

    /// Adjacency and order, at the two boundaries and past them. Runs over the token sequence
    /// rather than through the analyser, so a segmentation change cannot make this fail.
    #[test]
    fn a_phrase_is_a_contiguous_run_in_order() {
        let doc = t("the quick brown fox jumps");
        assert!(contains_phrase(&doc, &t("quick brown")));
        assert!(contains_phrase(&doc, &t("the quick")), "at the start");
        assert!(contains_phrase(&doc, &t("jumps")), "the last word alone");
        assert!(contains_phrase(&doc, &t("fox jumps")), "at the end");
        assert!(contains_phrase(&doc, &doc), "the whole document");

        assert!(!contains_phrase(&doc, &t("brown quick")), "order is a term");
        assert!(
            !contains_phrase(&doc, &t("quick fox")),
            "both words, not adjacent — the conjunction's answer and not this one"
        );
        assert!(!contains_phrase(&doc, &t("the fox")));
    }

    /// A repeated word is evidence twice over, on both sides: this separates a phrase from the
    /// deduplicated word-bag `match` resolves.
    #[test]
    fn a_repeated_word_is_not_collapsed() {
        assert!(contains_phrase(&t("had had had had"), &t("had had")));
        assert!(!contains_phrase(&t("the cat the hat"), &t("the the")));
        assert!(contains_phrase(&t("the the cat"), &t("the the")));
    }

    /// A phrase longer than the document, and an empty one on either side. `windows(0)` panics and
    /// `windows(n > len)` yields nothing, so both are decided before the walk rather than by it.
    #[test]
    fn the_degenerate_shapes_are_decided_before_the_walk() {
        assert!(!contains_phrase(&t("one two"), &t("one two three")));
        assert!(!contains_phrase(&[], &t("anything")));
        assert!(
            !contains_phrase(&t("a document"), &[]),
            "an empty phrase is not everywhere"
        );
        assert!(!contains_phrase(&[], &[]));
    }
}
