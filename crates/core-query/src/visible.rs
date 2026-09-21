//! The set of node ids a read may see, in whichever of two shapes costs less.
//!
//! Every masked read probes this set once per candidate: once per node of a
//! label scan, once per vector candidate, once per neighbour of every node a
//! scoped expansion visits. A `HashSet<u32>` probe is a hash, a bucket load and
//! a comparison, and at 50,000 visible ids out of 200,000 the table no longer
//! fits in cache — 11.3 ns per probe against 5.8 ns at 2,000 ids, and 29% of a
//! whole `query_masked` call.
//!
//! What the second shape bought, on that store, release profile, five process
//! runs of 51 repetitions each, median of the medians
//! (`crates/core-api/tests/mask_bench.rs`):
//!
//! ```text
//!                             |visible| = 2,000      |visible| = 50,000
//!                             (stays Sparse)         (becomes Dense)
//! query_masked                1396.8 → 1374.9 us     7983.9 → 5685.5 us  −28.8%
//! find_similar_vector_masked  1415.7 → 1416.3 us    10272.0 → 7967.0 us  −22.4%
//! neighborhood_masked         2469.5 → 2474.8 us     2401.1 → 2392.4 us   −0.4%
//! intersect                     53.0 →   31.4 us     1305.1 →    0.6 us
//! ```
//!
//! The 2,000-id column is flat because the rule leaves that mask a `HashSet`:
//! it is the control, and it says the enum around the set costs nothing.
//! `neighborhood_masked` is flat at both sizes because the probe is about 1% of
//! a BFS that spends its time expanding edges and building rows — the win is
//! where the probe is the work, not everywhere a mask appears.
//!
//! # The two shapes, and the rule that picks one
//!
//! [`VisibleSet::Dense`] is a bitset over `[0, span)`, where `span` is one past
//! the largest visible id. A probe is a bounds check, a shift and an AND, and
//! the whole structure is `span / 8` bytes — 25 KB for a 200,000-id store,
//! which stays in L2.
//!
//! [`VisibleSet::Sparse`] is the `HashSet<u32>` this type replaced, kept for
//! the case that makes a bitset a bad trade: a role that sees ten nodes in a
//! ten-million-node store would pay 1.25 MB as a bitset against a few hundred
//! bytes as a set, and role masks are cached, so that cost would persist.
//!
//! The rule is one comparison, evaluated once at construction, and the variant
//! it picks is fixed for the set's life:
//!
//! ```text
//! dense  ⟺  len * 64 >= span        (span = max visible id + 1)
//! ```
//!
//! Read it as a memory rule rather than a density rule: `len * 64 >= span` is
//! exactly `span / 8 <= len * 8`, and `len * 8` bytes is about what a
//! `HashSet<u32>` costs per element once hashbrown's control bytes and load
//! factor are counted. So the bitset is chosen precisely when it is no larger
//! than the set it replaces, and the ten-in-ten-million case fails the test by
//! four orders of magnitude.
//!
//! The rule is checked against the **distinct** count, after duplicates in the
//! input have been collapsed — a caller may hand the same id twice, and a
//! multiplicity-inflated count could otherwise talk the rule into a bitset the
//! real population does not earn.

use std::collections::HashSet;

/// A set of dense node ids, represented by whichever shape the module-level
/// rule picks at construction.
#[derive(Clone, Debug)]
pub enum VisibleSet {
    /// Few ids relative to the id space they live in: a hash set.
    Sparse(HashSet<u32>),
    /// Enough ids that a bitset is no larger than the set would be.
    ///
    /// `words` covers `[0, words.len() * 64)`; an id at or past that is absent,
    /// which is what makes the probe a bounds check rather than a branch on a
    /// separately tracked span. `len` is the population count, cached because
    /// [`VisibleSet::len`] is called on paths that cannot afford to popcount a
    /// whole bitset.
    Dense { words: Box<[u64]>, len: usize },
}

impl VisibleSet {
    /// Collect `ids` and choose a representation by the module's rule.
    ///
    /// Duplicates are collapsed; the rule sees the distinct count.
    pub fn from_ids(ids: impl IntoIterator<Item = u32>) -> VisibleSet {
        let ids: Vec<u32> = ids.into_iter().collect();
        let Some(&max) = ids.iter().max() else {
            return VisibleSet::Sparse(HashSet::new());
        };
        // `+ 1` saturates: on a 32-bit `usize`, `u32::MAX as usize + 1`
        // overflows — panicking in debug, and in release wrapping to 0, which
        // would take the dense branch with a zero-length word vector and panic
        // on the first write. Saturating keeps the arithmetic honest on every
        // target; a 32-bit build then reads the span as `usize::MAX`, which
        // sends a mask that large to `Sparse`, the correct answer for it.
        let span = (max as usize).saturating_add(1);
        // `ids.len()` counts duplicates, so it is only an upper bound on the
        // population — enough to rule *out* a bitset, never enough to rule one
        // in. When it passes, the bitset itself does the deduplication and the
        // rule is re-checked on the count that comes out of it.
        //
        // The *allocation* below is sized from that inflated count, so a list
        // of many duplicates plus one high id can briefly allocate a bitset
        // that `from_words` then demotes. Bounded by the store's id space, and
        // the outcome is still correct — but the rule seeing the distinct
        // count is true of the result, not of the transient.
        if ids.len().saturating_mul(64) < span {
            return VisibleSet::Sparse(ids.into_iter().collect());
        }
        let mut words = vec![0u64; span.div_ceil(64)];
        let mut len = 0usize;
        for id in ids {
            let bit = 1u64 << (id % 64);
            let word = &mut words[id as usize / 64];
            if *word & bit == 0 {
                *word |= bit;
                len += 1;
            }
        }
        VisibleSet::from_words(words, len)
    }

    /// Wrap a finished bitset, demoting it to [`VisibleSet::Sparse`] when the
    /// population it turned out to hold does not earn the space.
    fn from_words(mut words: Vec<u64>, len: usize) -> VisibleSet {
        while words.last() == Some(&0) {
            words.pop();
        }
        let span = match words.last() {
            None => return VisibleSet::Sparse(HashSet::new()),
            Some(&top) => (words.len() - 1) * 64 + (64 - top.leading_zeros() as usize),
        };
        if len.saturating_mul(64) >= span {
            VisibleSet::Dense {
                words: words.into_boxed_slice(),
                len,
            }
        } else {
            VisibleSet::Sparse(bits(&words).collect())
        }
    }

    /// Is `id` in the set?
    ///
    /// The whole point of the type: on [`VisibleSet::Dense`] this is a bounds
    /// check, a shift and an AND.
    #[inline]
    pub fn contains(&self, id: u32) -> bool {
        match self {
            VisibleSet::Sparse(set) => set.contains(&id),
            VisibleSet::Dense { words, .. } => words
                .get(id as usize / 64)
                .is_some_and(|w| w >> (id % 64) & 1 == 1),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            VisibleSet::Sparse(set) => set.len(),
            VisibleSet::Dense { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The ids in the set, ascending on [`VisibleSet::Dense`] and in hash order
    /// on [`VisibleSet::Sparse`] — no caller depends on the order, and every
    /// masked path sorts or keys its own output.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        let sparse = match self {
            VisibleSet::Sparse(set) => Some(set.iter().copied()),
            VisibleSet::Dense { .. } => None,
        };
        let dense = match self {
            VisibleSet::Sparse(_) => None,
            VisibleSet::Dense { words, .. } => Some(bits(words)),
        };
        sparse
            .into_iter()
            .flatten()
            .chain(dense.into_iter().flatten())
    }

    /// The ids in both sets, with the representation re-chosen for the result.
    ///
    /// Two dense sets intersect word-wise with no per-element allocation, which
    /// is the operation a multi-leg scope runs on every read. Any other pairing
    /// walks the smaller side and probes the larger, so the cost is set by the
    /// narrower of the two masks — and narrowing is the only thing an
    /// intersection can do.
    pub fn intersect(&self, other: &VisibleSet) -> VisibleSet {
        if let (VisibleSet::Dense { words: a, .. }, VisibleSet::Dense { words: b, .. }) =
            (self, other)
        {
            let n = a.len().min(b.len());
            let words: Vec<u64> = (0..n).map(|i| a[i] & b[i]).collect();
            let len = words.iter().map(|w| w.count_ones() as usize).sum();
            return VisibleSet::from_words(words, len);
        }
        let (small, large) = if self.len() <= other.len() {
            (self, other)
        } else {
            (other, self)
        };
        VisibleSet::from_ids(small.iter().filter(|&id| large.contains(id)))
    }
}

/// The set bits of `words`, ascending, as ids.
fn bits(words: &[u64]) -> impl Iterator<Item = u32> + '_ {
    words.iter().enumerate().flat_map(|(w, &word)| {
        (0..64u32)
            .filter(move |b| word >> b & 1 == 1)
            .map(move |b| (w * 64) as u32 + b)
    })
}

impl FromIterator<u32> for VisibleSet {
    fn from_iter<I: IntoIterator<Item = u32>>(ids: I) -> VisibleSet {
        VisibleSet::from_ids(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(v: &VisibleSet) -> &'static str {
        match v {
            VisibleSet::Sparse(_) => "sparse",
            VisibleSet::Dense { .. } => "dense",
        }
    }

    /// The rule, at the boundary and on both sides of it, stated as the two
    /// facts a reader needs: which variant, and that the answer is the same
    /// either way.
    #[test]
    fn the_rule_picks_the_variant_and_never_the_answer() {
        // span 6400, 100 ids: 100 * 64 == 6400, the boundary itself — dense.
        let at: VisibleSet = (0..100).map(|i| i * 64 + 63).collect();
        assert_eq!(at.len(), 100);
        assert_eq!(variant(&at), "dense", "len * 64 == span is dense");

        // One id further out: span 6464, 100 * 64 < 6464 — sparse.
        let over: VisibleSet = (0..99)
            .map(|i| i * 64 + 63)
            .chain(std::iter::once(6463))
            .collect();
        assert_eq!(over.len(), 100);
        assert_eq!(variant(&over), "sparse", "len * 64 < span is sparse");

        // Whichever variant was picked, membership is the same predicate.
        for id in 0..7000u32 {
            assert_eq!(at.contains(id), id % 64 == 63 && id < 6400, "at id {id}");
        }
        assert!(over.contains(6463));
        assert!(!over.contains(6462));
    }

    #[test]
    fn a_ten_id_mask_in_a_ten_million_id_space_stays_sparse() {
        let far: VisibleSet = (0..10).map(|i| 9_999_990 + i).collect();
        assert_eq!(variant(&far), "sparse");
        assert_eq!(far.len(), 10);
        assert!(far.contains(9_999_999));
        assert!(!far.contains(9_999_989));
    }

    #[test]
    fn an_empty_set_is_sparse_and_contains_nothing() {
        let empty = VisibleSet::from_ids(std::iter::empty());
        assert_eq!(variant(&empty), "sparse");
        assert!(empty.is_empty());
        assert!(!empty.contains(0));
    }

    /// A caller repeating one id must not talk the rule into a bitset the real
    /// population does not earn.
    #[test]
    fn duplicates_do_not_inflate_the_count_the_rule_sees() {
        let dupes: VisibleSet = std::iter::repeat_n(1_000_000u32, 50_000).collect();
        assert_eq!(dupes.len(), 1);
        assert_eq!(variant(&dupes), "sparse");
        assert!(dupes.contains(1_000_000));
    }

    #[test]
    fn iter_returns_exactly_the_members_of_either_variant() {
        for ids in [vec![0u32, 1, 2, 63, 64, 65], vec![0u32, 9_999_999]] {
            let v: VisibleSet = ids.iter().copied().collect();
            let mut got: Vec<u32> = v.iter().collect();
            got.sort_unstable();
            assert_eq!(got, ids, "{} lost a member", variant(&v));
        }
    }

    #[test]
    fn intersect_narrows_on_every_pairing_of_variants() {
        let dense_a: VisibleSet = (0..200u32).collect();
        let dense_b: VisibleSet = (100..300u32).collect();
        let sparse_a: VisibleSet = [5u32, 150, 9_999_999].into_iter().collect();
        assert_eq!(variant(&dense_a), "dense");
        assert_eq!(variant(&sparse_a), "sparse");

        let dd = dense_a.intersect(&dense_b);
        let mut got: Vec<u32> = dd.iter().collect();
        got.sort_unstable();
        assert_eq!(got, (100..200).collect::<Vec<u32>>());

        for (l, r) in [(&dense_a, &sparse_a), (&sparse_a, &dense_a)] {
            let out = l.intersect(r);
            let mut got: Vec<u32> = out.iter().collect();
            got.sort_unstable();
            assert_eq!(got, vec![5, 150], "intersection is order-independent");
        }
    }

    /// The word-wise arm truncates to the shorter bitset; a member past that
    /// end is absent from the result, which is what an intersection means.
    #[test]
    fn intersect_of_two_dense_sets_respects_the_shorter_span() {
        let short: VisibleSet = (0..64u32).collect();
        let long: VisibleSet = (0..640u32).collect();
        let out = short.intersect(&long);
        assert_eq!(out.len(), 64);
        assert!(out.contains(63));
        assert!(!out.contains(64));
    }

    /// The word-wise arm must re-apply the rule to what it produced: two dense
    /// sets whose intersection is thin across the same wide span hand back a
    /// sparse result, not a bitset the survivors do not earn.
    #[test]
    fn a_dense_intersection_that_narrows_hard_demotes_itself() {
        let a: VisibleSet = (0..200_000u32).filter(|i| i % 2 == 0).collect();
        let b: VisibleSet = (0..3_200u32).chain(std::iter::once(199_998)).collect();
        assert_eq!(variant(&a), "dense");
        assert_eq!(
            variant(&b),
            "dense",
            "both sides must take the word-wise arm"
        );

        let out = a.intersect(&b);
        assert_eq!(out.len(), 1_601);
        assert_eq!(variant(&out), "sparse");
        assert!(out.contains(0) && out.contains(3_198) && out.contains(199_998));
        assert!(!out.contains(1) && !out.contains(3_200));
    }
}
