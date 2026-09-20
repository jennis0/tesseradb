use croaring::Bitmap;
use tessera_filter::{ColumnPostings, SortedDict};
use tessera_types::AttrLocalId;

/// Does `document`'s token sequence contain `phrase`'s contiguously and in order?
///
/// **Both sides come from the same analyser**, so this is a comparison of the index's own units and
/// not of raw text: a phrase found here is one whose words the index holds adjacent. A repeated
/// word is not collapsed on either side — `"the the"` matches a document that says it twice in a
/// row and not one that says it once — which is what distinguishes a phrase from the word-bag the
/// conjunction already answered.
pub(in crate::filter) fn contains_phrase(document: &[String], phrase: &[String]) -> bool {
    // An empty phrase is refused upstream, and a phrase longer than the document cannot occur —
    // `windows` would panic on a zero length and yields nothing past the end, so both are stated
    // rather than left to it.
    if phrase.is_empty() || phrase.len() > document.len() {
        return false;
    }
    document.windows(phrase.len()).any(|w| w == phrase)
}

/// `match` over one text column: intersect the tokens' postings inside the candidate, or count
/// them where fewer than all are required.
///
/// **Every posting is masked as it is read, before anything is unioned or counted**, so no bitmap
/// that reaches the answer holds an entity outside `M_sel` — which is I2 held by construction
/// rather than by a final intersection that could be forgotten.
///
/// It is masked *as read* and not *before*: `ColumnPostings::entities` returns an owned bitmap, so
/// each token's corpus-wide posting is materialised transiently and then narrowed. That is the
/// resident cost of a `match` and it is a function of the tokens named rather than of what the
/// principal may see — the same quantity Appendix C's C25 registers as observable in the timing,
/// here in bytes. Nothing derived from it survives the intersection.
///
/// **An unresolved token contributes an empty posting rather than short-circuiting.** Under plain
/// `match` that yields the empty set either way; under m-of-n it must still consume its place in
/// the count, or `match` of three tokens with `minimum = 2` would silently become a two-token
/// question when one of them is absent from the corpus. Decision 0067 accepts the timing this
/// leaves — a term's existence and coarse carrier count are observable in a postings route, for
/// text and keyword alike — and Appendix C carries the row.
pub(in crate::filter) fn text_match(
    dict: &SortedDict,
    postings: &ColumnPostings,
    tokens: &[String],
    minimum: u32,
    candidate: &Bitmap,
) -> std::io::Result<Bitmap> {
    if tokens.is_empty() || minimum == 0 {
        // No token can be satisfied by no evidence: an empty `match` matches nothing rather than
        // everything, which is the same reading `any_of([])` takes.
        return Ok(Bitmap::new());
    }
    // **More required than asked for is unsatisfiable, not the conjunction.** `minimum` counts
    // *distinct* tokens — the caller's query is deduplicated before it reaches here, so "the same
    // word twice" is one piece of evidence — and a request for four of two words is one no item
    // can meet. Folding it into the `>=` branch below would answer the two-word conjunction, which
    // is a different and strictly wider question than the one asked.
    if minimum as usize > tokens.len() {
        return Ok(Bitmap::new());
    }
    // Each token's ordinal, resolved before any posting is read. Cheap — a binary search over a
    // front-coded dictionary — and separating it from the reads is what lets the conjunction below
    // narrow token by token without a second dictionary pass.
    let mut ordinals: Vec<Option<u32>> = Vec::with_capacity(tokens.len());
    for token in tokens {
        ordinals
            .push(dict.resolve(token).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?);
    }

    // ---- plain `match`: one running set, narrowed token by token ------------------------------
    //
    // **The accumulator is the whole memory argument.** Materialising every token's answer and
    // intersecting at the end holds `n` bitmaps at once; carrying one running set holds one, and
    // that set only ever *shrinks*. It starts at the candidate — the entities this principal may
    // see — so the answer is inside `M_sel` from the first step rather than from a final
    // intersection, which is I2 by construction and one fewer place to forget it.
    //
    // It is also faster, for the reason the cost model gives: a bitmap operation costs
    // O(containers touched), so every step after the first works against an already-narrowed set.
    //
    // **What it does not do is short-circuit on empty**, and that is deliberate rather than
    // overlooked: `text_match`'s contract is that an unresolved token reads its place rather than
    // stopping, so the *shape* of the work does not distinguish "no document has this word" from
    // "none you may see has it" any more sharply than Appendix C's C25 already accepts. A running
    // set that emptied at token 1 and returned would make the remaining reads a function of that
    // distinction. The loop runs to the end; the reads it makes past an emptied set are cheap by
    // the same container arithmetic, `narrow` taking an empty candidate as an immediate answer.
    if minimum as usize == tokens.len() {
        // **The candidate is borrowed for the first step, never cloned**, and the difference is
        // measurable rather than tidy: a clone of a million-entity mask costs ~800 ns, which is
        // most of a one-word query's whole budget. The first `narrow` reads the candidate and
        // writes the answer; every step after it reads the previous answer.
        let mut live: Option<Bitmap> = None;
        for ordinal in &ordinals {
            match live.as_mut() {
                // The first token reads the candidate — **borrowed, never cloned**, which the
                // measurement forced: a clone of a million-entity mask is ~800 ns, most of a
                // one-word query's whole budget.
                None => {
                    live = Some(match ordinal {
                        Some(ordinal) => postings.narrow(AttrLocalId::new(*ordinal), candidate)?,
                        // A token no dictionary holds is carried by nothing, so the conjunction is
                        // empty from here on — reached by narrowing to nothing rather than by
                        // returning, per the paragraph above.
                        None => Bitmap::new(),
                    });
                }
                // Every token after it narrows the running set **in place**, which allocates
                // nothing. Chaining the out-of-place form instead measured 19–51% slower than the
                // route this replaced, on a full-coverage principal intersecting common words —
                // see `ColumnPostings::narrow_inplace`.
                Some(live) => match ordinal {
                    Some(ordinal) => postings.narrow_inplace(AttrLocalId::new(*ordinal), live)?,
                    None => live.clear(),
                },
            }
        }
        // `tokens` is non-empty here, so the loop ran at least once; the default is the fail-safe
        // reading of a state the guards above have already excluded.
        let mut out = live.unwrap_or_default();
        // Once, at the end. The narrowing steps deliberately skip it — run-optimising a set the
        // next intersection is about to shrink is work thrown away.
        out.run_optimize();
        return Ok(out);
    }

    // ---- m-of-n: the per-token answers, each already inside the candidate ----------------------
    //
    // This shape genuinely needs every token's answer at once — a count cannot be accumulated into
    // one set — so it holds `n` bitmaps. Each is bounded by the **candidate** rather than by the
    // posting, `narrow` never assembling the corpus-wide set, so the peak is `n × |M_sel|` and not
    // `n × |corpus|`.
    let mut per_token: Vec<Bitmap> = Vec::with_capacity(tokens.len());
    for ordinal in &ordinals {
        per_token.push(match ordinal {
            Some(ordinal) => postings.narrow(AttrLocalId::new(*ordinal), candidate)?,
            // An unresolved token still takes its place in the count, or `match` of three tokens
            // with `minimum = 2` would silently become a two-token question when one is absent.
            None => Bitmap::new(),
        });
    }

    // m-of-n: how many of the tokens each candidate entity carries. Counted over the union rather
    // than over the candidate, so the work is the postings' size and not the mask's.
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

    /// **Adjacency and order, at the two boundaries and past them.**
    ///
    /// Run over the token *sequence* rather than through the analyser, because what this predicate
    /// owns is the sequence comparison — the analyser has its own golden vectors and mixing the two
    /// would make a segmentation change fail here.
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

    /// **A repeated word is evidence twice over, on both sides.** This is what separates a phrase
    /// from the deduplicated word-bag `match` resolves: `"the the"` is a claim about a document
    /// saying it twice in a row, and the conjunction that narrows to it cannot tell the difference.
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
