//! Fast size and inclusion queries on the bit-sets of the analysis.
//!
//! [`indexical`]'s generic [`BitSet`](indexical::bitset::BitSet) implementation for the
//! rustc bit-set counts the elements of a set by iterating over them, and decides
//! `a ⊇ b` by cloning `a`, adding `b` to the clone and comparing both counts. Both
//! dominate the dependency computation on bodies with many locations. The functions
//! here work on the underlying [`MixedBitSet`] instead: a count reads the chunk counts
//! (or pops the words) of the set, and an inclusion test allocates nothing.

use indexical::{
  IndexSet, IndexedValue,
  bitset::rustc::{MixedBitSet, RustcBitSet},
  pointer::PointerFamily,
};

/// The number of elements of `set`.
pub fn count(set: &RustcBitSet) -> usize {
  match set {
    MixedBitSet::Small(set) => set.count(),
    MixedBitSet::Large(set) => set.count(),
  }
}

/// Whether every element of `sub` is an element of `sup`.
///
/// # Panics
///
/// If the sets have different domain sizes.
pub fn is_superset(sup: &RustcBitSet, sub: &RustcBitSet) -> bool {
  match (sup, sub) {
    (MixedBitSet::Small(sup), MixedBitSet::Small(sub)) => sup.superset(sub),
    (MixedBitSet::Large(sup), MixedBitSet::Large(sub)) => {
      assert_eq!(sup.domain_size(), sub.domain_size());
      // `ChunkedBitSet` exposes no word-level inclusion test. Checking every element
      // of `sub` needs no allocation and stops at the first missing one.
      sub.count() <= sup.count() && sub.iter().all(|elem| sup.contains(elem))
    }
    _ => panic!("bit-sets with different domain sizes"),
  }
}

/// Fast versions of [`IndexSet::len`] and [`IndexSet::is_superset`] for sets backed by
/// the rustc bit-set, e.g.
/// [`LocationOrArgSet`](rustc_utils::mir::location_or_arg::index::LocationOrArgSet).
pub trait IndexSetExt {
  /// The number of elements, like [`IndexSet::len`].
  fn count(&self) -> usize;

  /// Whether every element of `other` is in `self`, like [`IndexSet::is_superset`].
  fn contains_all(&self, other: &Self) -> bool;
}

impl<'a, T, P> IndexSetExt for IndexSet<'a, T, RustcBitSet, P>
where
  T: IndexedValue + 'a,
  P: PointerFamily<'a>,
{
  fn count(&self) -> usize {
    count(self.inner())
  }

  fn contains_all(&self, other: &Self) -> bool {
    is_superset(self.inner(), other.inner())
  }
}

#[cfg(test)]
mod test {
  use indexical::bitset::BitSet;

  use super::*;

  /// A small deterministic generator (xorshift64*), so the property tests need no
  /// extra dependency and always check the same sets.
  struct Rng(u64);

  impl Rng {
    fn next(&mut self) -> u64 {
      self.0 ^= self.0 >> 12;
      self.0 ^= self.0 << 25;
      self.0 ^= self.0 >> 27;
      self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
      self.next() % n
    }
  }

  fn naive_count(set: &RustcBitSet) -> usize {
    set.iter().count()
  }

  /// indexical's generic definition of the inclusion test.
  fn naive_superset(sup: &RustcBitSet, sub: &RustcBitSet) -> bool {
    let mut union = sup.clone();
    BitSet::union(&mut union, sub);
    naive_count(&union) == naive_count(sup)
  }

  /// A random set over `0 .. size`, built from runs so that large sets get chunks that
  /// are empty, full, or mixed (the three representations of a `ChunkedBitSet` chunk).
  fn random_set(rng: &mut Rng, size: usize) -> RustcBitSet {
    let mut set = RustcBitSet::new_empty(size);
    match rng.below(6) {
      0 => {}
      1 => set.insert_all(),
      density => {
        let mut i = 0;
        while i < size {
          let run = 1 + rng.below(if density == 2 { 3000 } else { 40 }) as usize;
          let fill = rng.below(8) < density;
          for elem in i .. (i + run).min(size) {
            if fill || rng.below(16) == 0 {
              set.insert(elem);
            }
          }
          i += run;
        }
      }
    }
    set
  }

  /// A random subset of `set`.
  fn random_subset(rng: &mut Rng, set: &RustcBitSet, size: usize) -> RustcBitSet {
    let mut sub = RustcBitSet::new_empty(size);
    let keep = rng.below(5);
    for elem in set.iter() {
      if rng.below(4) < keep {
        sub.insert(elem);
      }
    }
    sub
  }

  const SIZES: &[usize] = &[
    1, 2, 63, 64, 65, 100, 2047, 2048, 2049, 4096, 5000, 10_000, 30_000,
  ];

  #[test]
  fn count_matches_naive() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for &size in SIZES {
      for _ in 0 .. 200 {
        let set = random_set(&mut rng, size);
        assert_eq!(count(&set), naive_count(&set), "size {size}");
      }
    }
  }

  #[test]
  fn superset_matches_naive() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let mut outcomes = [0usize; 2];
    for &size in SIZES {
      for _ in 0 .. 200 {
        let a = random_set(&mut rng, size);
        // Unrelated sets are rarely included in one another: also test subsets of `a`,
        // and subsets with one element changed.
        let mut b = match rng.below(3) {
          0 => random_set(&mut rng, size),
          _ => random_subset(&mut rng, &a, size),
        };
        if rng.below(4) == 0 {
          b.insert(rng.below(size as u64) as usize);
        }
        for (sup, sub) in [(&a, &b), (&b, &a), (&a, &a), (&b, &b)] {
          let expected = naive_superset(sup, sub);
          assert_eq!(is_superset(sup, sub), expected, "size {size}");
          outcomes[expected as usize] += 1;
        }
      }
    }
    // Both outcomes are well represented.
    assert!(outcomes.iter().all(|n| *n > 1000), "{outcomes:?}");
  }

  #[test]
  #[should_panic]
  fn superset_of_different_domains_panics() {
    is_superset(&RustcBitSet::new_empty(3000), &RustcBitSet::new_empty(3001));
  }
}
