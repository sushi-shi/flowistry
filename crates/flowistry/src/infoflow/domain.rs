//! The state of the information flow analysis at a location, with the rows seeded
//! from the arguments stored once per body instead of once per location.
//!
//! At the start of a body, every place `p` in the interior of an argument `_i` (and of
//! the places its pointers point to) depends on `_i` itself: its row is `{_i}`. In a
//! body whose arguments have large interiors (e.g. `&self` of a type with many nested
//! fields), these rows can be the vast majority of the rows of every location, although
//! almost all of them keep their initial value everywhere.
//!
//! A [`LazyMatrix`] therefore distinguishes *explicit* rows, stored in the matrix, from
//! *implicit* ones: once a state is seeded (at the start of the body, and at every
//! location the start reaches), every seeded row that has no explicit value has its
//! seed as value. The seeds ([`SeedRows`]) are shared by all states of a body. A seeded
//! row that the analysis clears is stored as an explicit empty row (a *tombstone*).
//!
//! Every operation gives the same values (and the same "changed" answers) as the
//! eager [`IndexMatrix`](indexical::IndexMatrix) that stores every row, whose rows are
//! present if and only if they are non-empty. The `shadow-eager` feature checks this
//! at run time: every state then also holds the eager matrix, and every operation
//! compares both.

use std::{fmt, hash::Hash, rc::Rc};

use indexical::{IndexedDomain, IndexedValue, bitset::rustc::IndexSet};
use rustc_data_structures::fx::FxHashMap;
use rustc_mir_dataflow::JoinSemiLattice;

/// The rows seeded at the start of a body: each row with the single column it starts
/// with. Shared by all the states of a body.
pub struct SeedRows<R, C: IndexedValue + 'static> {
  columns: FxHashMap<R, C::Index>,
  /// For each seed column `c`, the set `{c}`.
  singletons: FxHashMap<C::Index, IndexSet<C>>,
  empty: IndexSet<C>,
  domain: Rc<IndexedDomain<C>>,
}

impl<R, C> SeedRows<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  /// The seeds `(row, column)`. A row may be given several times, but always with the
  /// same column.
  ///
  /// # Panics
  ///
  /// If a row is given with two different columns.
  pub fn new(
    domain: &Rc<IndexedDomain<C>>,
    seeds: impl IntoIterator<Item = (R, C::Index)>,
  ) -> Self {
    let mut columns = FxHashMap::default();
    let mut singletons = FxHashMap::default();
    for (row, col) in seeds {
      let seed = *columns.entry(row).or_insert(col);
      assert!(seed == col, "a seeded row must have a single column");
      singletons.entry(col).or_insert_with(|| {
        let mut set = IndexSet::new(domain);
        set.insert(col);
        set
      });
    }
    SeedRows {
      columns,
      singletons,
      empty: IndexSet::new(domain),
      domain: domain.clone(),
    }
  }

  /// No seeds.
  pub fn none(domain: &Rc<IndexedDomain<C>>) -> Self {
    Self::new(domain, [])
  }

  /// The number of seeded rows.
  pub fn len(&self) -> usize {
    self.columns.len()
  }

  /// The seed column of `row`, if it is seeded.
  pub fn column(&self, row: &R) -> Option<C::Index> {
    self.columns.get(row).copied()
  }

  /// The seeded rows and their columns.
  pub fn iter(&self) -> impl Iterator<Item = (&R, C::Index)> {
    self.columns.iter().map(|(row, col)| (row, *col))
  }

  fn singleton(&self, col: C::Index) -> &IndexSet<C> {
    &self.singletons[&col]
  }
}

/// An explicit value of a seeded row.
#[derive(Clone)]
struct SeededRow<C: IndexedValue + 'static> {
  col: C::Index,
  /// The value, possibly empty (a tombstone).
  set: IndexSet<C>,
}

/// A sparse matrix from rows `R` to sets of columns `C`, whose seeded rows (see
/// [`SeedRows`]) are implicit until they are written. See the [module
/// documentation](self).
///
/// The *value* of a row is its explicit value if it has one; else the singleton of its
/// seed if the matrix is seeded and the row is seeded; else the empty set.
pub struct LazyMatrix<R, C: IndexedValue + 'static> {
  /// Whether the seeds are part of the value of this matrix.
  seeded: bool,
  /// Explicit rows that are not seeded. Never empty.
  plain: FxHashMap<R, IndexSet<C>>,
  /// Explicit rows that are seeded; empty rows are tombstones.
  seeded_rows: FxHashMap<R, SeededRow<C>>,
  seeds: Rc<SeedRows<R, C>>,
  #[cfg(feature = "shadow-eager")]
  shadow: indexical::bitset::rustc::IndexMatrix<R, C>,
}

impl<R, C> LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  /// The empty matrix (the bottom of the lattice), not seeded.
  pub fn new(seeds: &Rc<SeedRows<R, C>>) -> Self {
    LazyMatrix {
      seeded: false,
      plain: FxHashMap::default(),
      seeded_rows: FxHashMap::default(),
      seeds: Rc::clone(seeds),
      #[cfg(feature = "shadow-eager")]
      shadow: indexical::bitset::rustc::IndexMatrix::new(&seeds.domain),
    }
  }

  /// Adds the seed of every seeded row to its value.
  pub fn seed(&mut self) {
    for row in self.seeded_rows.values_mut() {
      row.set.insert(row.col);
    }
    self.seeded = true;
  }

  /// Whether the seeds are part of the value of this matrix.
  pub fn is_seeded(&self) -> bool {
    self.seeded
  }

  /// The seeds of this matrix.
  pub fn seeds(&self) -> &Rc<SeedRows<R, C>> {
    &self.seeds
  }

  /// Returns the [`IndexedDomain`] for the column type.
  pub fn col_domain(&self) -> &Rc<IndexedDomain<C>> {
    &self.seeds.domain
  }

  /// The value of `row`.
  pub fn row_set(&self, row: &R) -> &IndexSet<C> {
    if let Some(set) = self.plain.get(row) {
      return set;
    }
    match self.seeds.column(row) {
      None => &self.seeds.empty,
      Some(col) => match self.seeded_rows.get(row) {
        Some(explicit) => &explicit.set,
        None => self.implicit(col),
      },
    }
  }

  fn implicit(&self, col: C::Index) -> &IndexSet<C> {
    if self.seeded {
      self.seeds.singleton(col)
    } else {
      &self.seeds.empty
    }
  }

  /// Adds all elements of `from` to the value of `row`, returning true if it changed.
  pub fn union_into_row(&mut self, row: R, from: &IndexSet<C>) -> bool {
    #[cfg(feature = "shadow-eager")]
    let shadow_changed = self.shadow.union_into_row(row.clone(), from);
    #[cfg(feature = "shadow-eager")]
    let checked_row = row.clone();

    let changed = if let Some(set) = self.plain.get_mut(&row) {
      set.union_changed(from)
    } else {
      match self.seeds.column(&row) {
        None => {
          let mut set = IndexSet::new(&self.seeds.domain);
          let changed = set.union_changed(from);
          if changed {
            self.plain.insert(row, set);
          }
          changed
        }
        Some(col) => {
          if let Some(explicit) = self.seeded_rows.get_mut(&row) {
            explicit.set.union_changed(from)
          } else {
            let mut set = self.implicit(col).clone();
            let changed = set.union_changed(from);
            if changed {
              self.seeded_rows.insert(row, SeededRow { col, set });
            }
            changed
          }
        }
      }
    };

    #[cfg(feature = "shadow-eager")]
    {
      assert_eq!(
        changed, shadow_changed,
        "shadow-eager: union_into_row changed"
      );
      self.check_row(&checked_row);
    }
    changed
  }

  /// Empties the value of `row`.
  pub fn clear_row(&mut self, row: &R) {
    #[cfg(feature = "shadow-eager")]
    self.shadow.clear_row(row);

    if self.plain.remove(row).is_none()
      && let Some(col) = self.seeds.column(row)
    {
      if self.seeded {
        // Without an explicit value, the row would take its seed.
        let set = IndexSet::new(&self.seeds.domain);
        self.seeded_rows.insert(row.clone(), SeededRow { col, set });
      } else {
        self.seeded_rows.remove(row);
      }
    }

    #[cfg(feature = "shadow-eager")]
    self.check_row(row);
  }

  /// The rows with a non-empty value, and their values.
  pub fn rows(&self) -> impl Iterator<Item = (&R, &IndexSet<C>)> {
    let plain = self.plain.iter().filter(|(_, set)| !set.inner().is_empty());
    let explicit = self
      .seeded_rows
      .iter()
      .filter(|(_, row)| !row.set.inner().is_empty())
      .map(|(key, row)| (key, &row.set));
    let implicit = self
      .seeded
      .then(|| {
        self
          .seeds
          .iter()
          .filter(|(key, _)| !self.seeded_rows.contains_key(key))
          .map(|(key, col)| (key, self.seeds.singleton(col)))
      })
      .into_iter()
      .flatten();
    plain.chain(explicit).chain(implicit)
  }

  /// The number of explicitly stored rows (including tombstones).
  pub fn explicit_len(&self) -> usize {
    self.plain.len() + self.seeded_rows.len()
  }

  /// The number of seeded rows whose value is implicit.
  pub fn implicit_len(&self) -> usize {
    if self.seeded {
      self.seeds.len() - self.seeded_rows.len()
    } else {
      0
    }
  }
}

impl<R, C> JoinSemiLattice for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  /// Adds the value of every row of `other` to the value of the row in `self`,
  /// returning true if some value of `self` changed.
  fn join(&mut self, other: &Self) -> bool {
    debug_assert!(Rc::ptr_eq(&self.seeds, &other.seeds));
    let was_seeded = self.seeded;
    let mut changed = false;

    for (row, set) in &other.plain {
      match self.plain.get_mut(row) {
        Some(own) => changed |= own.union_changed(set),
        None => {
          // `set` is not empty.
          self.plain.insert(row.clone(), set.clone());
          changed = true;
        }
      }
    }

    for (row, explicit) in &other.seeded_rows {
      match self.seeded_rows.get_mut(row) {
        Some(own) => changed |= own.set.union_changed(&explicit.set),
        None => {
          // The row's value in `self` is implicit.
          let mut set = self.implicit(explicit.col).clone();
          let grew = set.union_changed(&explicit.set);
          // Store the result unless it is still the implicit value. A tombstone of
          // `other` must be stored if `self` is not seeded yet, since `self` may become
          // seeded below.
          if grew
            || !(was_seeded || other.seeded)
            || !was_seeded && set.inner().is_empty()
          {
            self.seeded_rows.insert(row.clone(), SeededRow {
              col: explicit.col,
              set,
            });
          }
          changed |= grew;
        }
      }
    }

    if other.seeded {
      // The seeded rows of `other` without an explicit value have their seed.
      for (row, own) in &mut self.seeded_rows {
        if !other.seeded_rows.contains_key(row) {
          changed |= own.set.insert(own.col);
        }
      }
      if !was_seeded {
        // Every seeded row without an explicit value now has its seed instead of
        // nothing.
        self.seeded = true;
        changed |= self.seeded_rows.len() < self.seeds.len();
      }
    }

    #[cfg(feature = "shadow-eager")]
    {
      let shadow_changed = self.shadow.join(&other.shadow);
      assert_eq!(changed, shadow_changed, "shadow-eager: join changed");
      self.check_all();
    }
    changed
  }
}

impl<R, C> Clone for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  fn clone(&self) -> Self {
    LazyMatrix {
      seeded: self.seeded,
      plain: self.plain.clone(),
      seeded_rows: self.seeded_rows.clone(),
      seeds: Rc::clone(&self.seeds),
      #[cfg(feature = "shadow-eager")]
      shadow: self.shadow.clone(),
    }
  }
}

/// Equality of values: two matrices are equal if every row has the same value in both.
impl<R, C> PartialEq for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  fn eq(&self, other: &Self) -> bool {
    let same = |row: &R| self.row_set(row) == other.row_set(row);
    let explicit = (self.plain.keys())
      .chain(other.plain.keys())
      .chain(self.seeded_rows.keys())
      .chain(other.seeded_rows.keys());
    explicit.into_iter().all(same)
      && (self.seeded == other.seeded || self.seeds.iter().all(|(row, _)| same(row)))
  }
}

impl<R, C> Eq for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
}

impl<R, C> fmt::Debug for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash + fmt::Debug,
  C: IndexedValue + fmt::Debug + 'static,
{
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_map().entries(self.rows()).finish()
  }
}

#[cfg(feature = "shadow-eager")]
impl<R, C> LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  /// Inserts `col` into `row` of the eager shadow only: the analysis replays its eager
  /// seeding of the start state with this, independently of [`SeedRows`].
  pub fn shadow_insert(&mut self, row: R, col: C::Index) {
    self.shadow.insert(row, col);
  }

  fn check_row(&self, row: &R) {
    assert!(
      self.row_set(row) == self.shadow.row_set(row),
      "shadow-eager: a row differs from the eager matrix"
    );
  }

  /// Checks that every row has the same value as in the eager shadow, and that the
  /// same rows are non-empty.
  pub fn check_all(&self) {
    for (row, set) in self.shadow.rows() {
      assert!(
        self.row_set(row) == set,
        "shadow-eager: a row differs from the eager matrix"
      );
    }
    for row in self.plain.keys().chain(self.seeded_rows.keys()) {
      self.check_row(row);
    }
    if self.seeded {
      for (row, _) in self.seeds.iter() {
        self.check_row(row);
      }
    }
    let nonempty = self
      .shadow
      .rows()
      .filter(|(_, set)| !set.is_empty())
      .count();
    assert_eq!(
      self.rows().count(),
      nonempty,
      "shadow-eager: the non-empty rows differ from the eager matrix"
    );
  }
}

/// The row operations of the transfer function (see `FlowAnalysis::transfer`), on the
/// flow state ([`LazyMatrix`]) and on the eager matrices that callee summaries use.
pub(crate) trait RowMatrix<R, C: IndexedValue + 'static> {
  /// Returns the [`IndexedDomain`] for the column type.
  fn col_domain(&self) -> &Rc<IndexedDomain<C>>;

  /// The value of `row`.
  fn row_set(&self, row: &R) -> &IndexSet<C>;

  /// Adds all elements of `from` to the value of `row`, returning true if it changed.
  fn union_into_row(&mut self, row: R, from: &IndexSet<C>) -> bool;

  /// Empties the value of `row`.
  fn clear_row(&mut self, row: &R);
}

impl<R, C> RowMatrix<R, C> for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  fn col_domain(&self) -> &Rc<IndexedDomain<C>> {
    LazyMatrix::col_domain(self)
  }

  fn row_set(&self, row: &R) -> &IndexSet<C> {
    LazyMatrix::row_set(self, row)
  }

  fn union_into_row(&mut self, row: R, from: &IndexSet<C>) -> bool {
    LazyMatrix::union_into_row(self, row, from)
  }

  fn clear_row(&mut self, row: &R) {
    LazyMatrix::clear_row(self, row)
  }
}

impl<R, C> RowMatrix<R, C> for indexical::bitset::rustc::IndexMatrix<R, C>
where
  R: PartialEq + Eq + Hash + Clone,
  C: IndexedValue + 'static,
{
  fn col_domain(&self) -> &Rc<IndexedDomain<C>> {
    indexical::bitset::rustc::IndexMatrix::col_domain(self)
  }

  fn row_set(&self, row: &R) -> &IndexSet<C> {
    indexical::bitset::rustc::IndexMatrix::row_set(self, row)
  }

  fn union_into_row(&mut self, row: R, from: &IndexSet<C>) -> bool {
    indexical::bitset::rustc::IndexMatrix::union_into_row(self, row, from)
  }

  fn clear_row(&mut self, row: &R) {
    indexical::bitset::rustc::IndexMatrix::clear_row(self, row)
  }
}

#[cfg(test)]
mod test {
  use indexical::bitset::rustc::IndexMatrix;

  use super::*;

  #[derive(Clone, PartialEq, Eq, Hash, Debug)]
  pub struct Col(u32);

  indexical::define_index_type! {
    pub struct ColIdx for Col = u32;
  }

  /// A small deterministic generator (xorshift64*).
  struct Rng(u64);

  impl Rng {
    fn below(&mut self, n: usize) -> usize {
      self.0 ^= self.0 >> 12;
      self.0 ^= self.0 << 25;
      self.0 ^= self.0 >> 27;
      (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as usize % n
    }
  }

  const ROWS: u32 = 24;

  type Eager = IndexMatrix<u32, Col>;
  type Lazy = LazyMatrix<u32, Col>;

  fn eager_value(eager: &Eager, row: u32) -> Vec<ColIdx> {
    eager.row_set(&row).indices().collect()
  }

  fn lazy_value(lazy: &Lazy, row: u32) -> Vec<ColIdx> {
    lazy.row_set(&row).indices().collect()
  }

  fn assert_same(lazy: &Lazy, eager: &Eager, what: &str) {
    for row in 0 .. ROWS {
      assert_eq!(
        lazy_value(lazy, row),
        eager_value(eager, row),
        "row {row} after {what}"
      );
    }
    let mut lazy_rows = lazy.rows().map(|(row, _)| *row).collect::<Vec<_>>();
    let mut eager_rows = eager
      .rows()
      .filter(|(_, set)| !set.is_empty())
      .map(|(row, _)| *row)
      .collect::<Vec<_>>();
    lazy_rows.sort();
    eager_rows.sort();
    assert_eq!(lazy_rows, eager_rows, "non-empty rows after {what}");
    assert!(lazy.explicit_len() + lazy.implicit_len() >= lazy_rows.len());
  }

  /// Random sequences of operations on several states give the same values, and the
  /// same "changed" answers, as the eager matrix.
  #[test]
  fn lazy_matrix_matches_eager() {
    let domain = Rc::new(IndexedDomain::from_iter((0 .. 40).map(Col)));
    let mut rng = Rng(0x243f_6a88_85a3_08d3);
    let mut outcomes = [0usize; 2];
    for round in 0 .. 300 {
      // About half of the rows are seeded, with one of a few columns each.
      let mut seeds = Vec::new();
      for row in 0 .. ROWS {
        if rng.below(2) == 0 {
          seeds.push((row, ColIdx::from_usize(30 + rng.below(4))));
        }
      }
      let seed_rows = Rc::new(SeedRows::new(&domain, seeds.iter().copied()));
      let mut states = (0 .. 4)
        .map(|_| (Lazy::new(&seed_rows), Eager::new(&domain)))
        .collect::<Vec<_>>();
      for step in 0 .. 60 {
        let i = rng.below(states.len());
        let what = format!("round {round} step {step}");
        match rng.below(10) {
          0 => {
            let (lazy, eager) = &mut states[i];
            lazy.seed();
            for (row, col) in &seeds {
              eager.insert(*row, *col);
              // The shadow is seeded eagerly by its user, as the analysis does.
              #[cfg(feature = "shadow-eager")]
              lazy.shadow_insert(*row, *col);
            }
            assert_same(lazy, eager, &format!("{what}: seed"));
          }
          1 | 2 | 3 => {
            let row = rng.below(ROWS as usize) as u32;
            let mut from = IndexSet::new(&domain);
            for _ in 0 .. 1 + rng.below(3) {
              from.insert(ColIdx::from_usize(rng.below(34)));
            }
            let (lazy, eager) = &mut states[i];
            let changed = lazy.union_into_row(row, &from);
            assert_eq!(changed, eager.union_into_row(row, &from), "{what}: union");
            outcomes[changed as usize] += 1;
            assert_same(lazy, eager, &format!("{what}: union"));
          }
          4 | 5 => {
            let row = rng.below(ROWS as usize) as u32;
            let (lazy, eager) = &mut states[i];
            lazy.clear_row(&row);
            eager.clear_row(&row);
            assert_same(lazy, eager, &format!("{what}: clear"));
          }
          6 | 7 | 8 => {
            let j = rng.below(states.len());
            let (other_lazy, other_eager) = states[j].clone();
            let (lazy, eager) = &mut states[i];
            let changed = lazy.join(&other_lazy);
            assert_eq!(changed, eager.join(&other_eager), "{what}: join");
            outcomes[changed as usize] += 1;
            assert_same(lazy, eager, &format!("{what}: join"));
          }
          _ => {
            let j = rng.below(states.len());
            let (a, b) = (&states[i], &states[j]);
            assert_eq!(a.0 == b.0, a.1 == b.1, "{what}: eq");
            if rng.below(4) == 0 {
              states[i] = (Lazy::new(&seed_rows), Eager::new(&domain));
            }
          }
        }
      }
    }
    // Both answers are well represented.
    assert!(outcomes.iter().all(|n| *n > 1000), "{outcomes:?}");
  }

  #[test]
  #[should_panic]
  fn seed_rows_have_one_column() {
    let domain = Rc::new(IndexedDomain::from_iter((0 .. 4).map(Col)));
    SeedRows::new(&domain, [
      (0u32, ColIdx::from_usize(1)),
      (0, ColIdx::from_usize(2)),
    ]);
  }
}
