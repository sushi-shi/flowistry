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
//!
//! Explicit rows are shared between states (copy on write): a join shares the rows it
//! copies, and a row is only copied when it is written while shared. Consecutive
//! locations mostly have the same rows, so this saves most copies and most of the
//! memory of the states, and a join skips a shared row without comparing its bits.
//!
//! # Row groups
//!
//! A call can write thousands of rows with one value: in `Recurse` mode, a callee
//! returning a large enum (e.g. an error type with many variants) writes every leaf
//! of the call's destination with the same dependencies. Stored row by row, these
//! rows make up most of every later state. A [`RowGroups`] layout (shared by all
//! states of a body, like the seeds) lists such groups of rows, and
//! [`LazyMatrix::assign_group`] sets the value of every member at once: the state then
//! stores the group's value once. A member written on its own afterwards (or joined
//! with a state where the members have their own values) *expands* its group: every
//! member gets its own row again, sharing the group's value.

use std::{cell::Cell, fmt, hash::Hash, rc::Rc};

use indexical::{IndexedDomain, IndexedValue, bitset::rustc::IndexSet};
use rustc_data_structures::fx::FxHashMap;
use rustc_mir_dataflow::JoinSemiLattice;

use crate::mir::bitset::IndexSetExt;

/// A row value, shared between states until it is written.
type Row<C> = Rc<IndexSet<C>>;

/// Adds `from` to `row`, returning true if it changed. The row is copied first if it
/// is shared and changes; if `from` contains it, the row becomes a share of `from`.
fn union_row<C: IndexedValue + 'static>(row: &mut Row<C>, from: &Row<C>) -> bool {
  if Rc::ptr_eq(row, from) || row.contains_all(from) {
    false
  } else {
    if from.contains_all(row) {
      *row = Rc::clone(from);
    } else {
      Rc::make_mut(row).union(from);
    }
    true
  }
}

/// Adds `from` to `row`, returning true if it changed (copying the row if it is shared).
fn union_set_into_row<C: IndexedValue + 'static>(
  row: &mut Row<C>,
  from: &IndexSet<C>,
) -> bool {
  if row.contains_all(from) {
    false
  } else {
    Rc::make_mut(row).union(from);
    true
  }
}

/// Adds `col` to `row`, returning true if it changed (copying the row if it is shared).
fn insert_into_row<C: IndexedValue + 'static>(row: &mut Row<C>, col: C::Index) -> bool {
  if row.contains(col) {
    false
  } else {
    Rc::make_mut(row).insert(col);
    true
  }
}

/// The rows seeded at the start of a body: each row with the single column it starts
/// with. Shared by all the states of a body.
pub struct SeedRows<R, C: IndexedValue + 'static> {
  columns: FxHashMap<R, C::Index>,
  /// For each seed column `c`, the set `{c}`.
  singletons: FxHashMap<C::Index, Row<C>>,
  empty: Row<C>,
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
        Rc::new(set)
      });
    }
    SeedRows {
      columns,
      singletons,
      empty: Rc::new(IndexSet::new(domain)),
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

  fn singleton(&self, col: C::Index) -> &Row<C> {
    &self.singletons[&col]
  }
}

/// Identifies a group of a [`RowGroups`] layout.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct GroupId(u32);

/// A group of rows that some writes set to one value together, see
/// [`LazyMatrix::assign_group`].
pub struct RowGroup<R> {
  id: GroupId,
  members: Box<[R]>,
}

impl<R> RowGroup<R> {
  /// The identifier of the group in its layout.
  pub fn id(&self) -> GroupId {
    self.id
  }

  /// The rows of the group.
  pub fn members(&self) -> &[R] {
    &self.members
  }
}

/// Disjoint groups of rows of a body, none of them seeded (see the [module
/// documentation](self#row-groups)). Shared by all the states of a body.
pub struct RowGroups<R> {
  groups: Vec<RowGroup<R>>,
  group_of: FxHashMap<R, GroupId>,
  /// How often a state expanded a group, see [`RowGroups::expansions`].
  expansions: Cell<usize>,
}

impl<R: Clone + Eq + Hash> RowGroups<R> {
  /// No groups.
  pub fn none() -> Self {
    RowGroups {
      groups: Vec::new(),
      group_of: FxHashMap::default(),
      expansions: Cell::new(0),
    }
  }

  /// Adds a group of the distinct rows `members`. Returns the group with the same
  /// members if there is one, and `None` if one of `members` is in another group.
  pub fn add(&mut self, members: Vec<R>) -> Option<GroupId> {
    let first = self.group_of.get(members.first()?).copied();
    if let Some(id) = first {
      let group = &self.groups[id.0 as usize];
      let same = group.members.len() == members.len()
        && members
          .iter()
          .all(|row| self.group_of.get(row) == Some(&id));
      return same.then_some(id);
    }
    if members.iter().any(|row| self.group_of.contains_key(row)) {
      return None;
    }
    let id = GroupId(u32::try_from(self.groups.len()).unwrap());
    for row in &members {
      let previous = self.group_of.insert(row.clone(), id);
      assert!(
        previous.is_none(),
        "the members of a group must be distinct"
      );
    }
    self.groups.push(RowGroup {
      id,
      members: members.into_boxed_slice(),
    });
    Some(id)
  }

  /// Whether there are no groups.
  pub fn is_empty(&self) -> bool {
    self.groups.is_empty()
  }

  /// The number of groups.
  pub fn len(&self) -> usize {
    self.groups.len()
  }

  /// The number of rows in groups.
  pub fn members_len(&self) -> usize {
    self.group_of.len()
  }

  /// The group `id`.
  pub fn group(&self, id: GroupId) -> &RowGroup<R> {
    &self.groups[id.0 as usize]
  }

  /// The group of `row`, if any.
  pub fn group_of(&self, row: &R) -> Option<GroupId> {
    if self.groups.is_empty() {
      return None;
    }
    self.group_of.get(row).copied()
  }

  /// How often a state stored the value of a group member by member again, because
  /// a member was written on its own (or joined with such a state).
  pub fn expansions(&self) -> usize {
    self.expansions.get()
  }
}

/// An explicit value of a seeded row.
#[derive(Clone)]
struct SeededRow<C: IndexedValue + 'static> {
  col: C::Index,
  /// The value, possibly empty (a tombstone).
  set: Row<C>,
}

/// A sparse matrix from rows `R` to sets of columns `C`, whose seeded rows (see
/// [`SeedRows`]) are implicit until they are written. See the [module
/// documentation](self).
///
/// The *value* of a row is its explicit value if it has one; else the value of its
/// group if the row is in a group ([`RowGroups`]) whose value is stored; else the
/// singleton of its seed if the matrix is seeded and the row is seeded; else the empty
/// set.
pub struct LazyMatrix<R, C: IndexedValue + 'static> {
  /// Whether the seeds are part of the value of this matrix.
  seeded: bool,
  /// Explicit rows that are not seeded. Never empty. A member of a group whose value
  /// is stored in `grouped` has no explicit row.
  plain: FxHashMap<R, Row<C>>,
  /// Explicit rows that are seeded; empty rows are tombstones.
  seeded_rows: FxHashMap<R, SeededRow<C>>,
  /// The value of every member of a group, for the groups whose members have no
  /// explicit rows. Never empty.
  grouped: FxHashMap<GroupId, Row<C>>,
  seeds: Rc<SeedRows<R, C>>,
  groups: Rc<RowGroups<R>>,
  #[cfg(feature = "shadow-eager")]
  shadow: indexical::bitset::rustc::IndexMatrix<R, C>,
}

impl<R, C> LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  /// The empty matrix (the bottom of the lattice), not seeded, without row groups.
  pub fn new(seeds: &Rc<SeedRows<R, C>>) -> Self {
    Self::with_groups(seeds, &Rc::new(RowGroups::none()))
  }

  /// The empty matrix (the bottom of the lattice), not seeded, whose rows may be
  /// written group by group (see [`assign_group`](Self::assign_group)).
  ///
  /// # Panics
  ///
  /// In debug builds, if a member of a group is seeded.
  pub fn with_groups(seeds: &Rc<SeedRows<R, C>>, groups: &Rc<RowGroups<R>>) -> Self {
    debug_assert!(
      groups
        .group_of
        .keys()
        .all(|row| seeds.column(row).is_none()),
      "a seeded row cannot be in a group"
    );
    LazyMatrix {
      seeded: false,
      plain: FxHashMap::default(),
      seeded_rows: FxHashMap::default(),
      grouped: FxHashMap::default(),
      seeds: Rc::clone(seeds),
      groups: Rc::clone(groups),
      #[cfg(feature = "shadow-eager")]
      shadow: indexical::bitset::rustc::IndexMatrix::new(&seeds.domain),
    }
  }

  /// The row groups of this matrix.
  pub fn groups(&self) -> &Rc<RowGroups<R>> {
    &self.groups
  }

  /// Gives every member of the group `id` its own explicit row with the value of the
  /// group, if the value of the group is stored. The values do not change.
  fn expand(&mut self, id: GroupId) {
    let Some(value) = self.grouped.remove(&id) else {
      return;
    };
    let groups = &self.groups;
    groups.expansions.set(groups.expansions.get() + 1);
    for row in groups.group(id).members() {
      self.plain.insert(row.clone(), Rc::clone(&value));
    }
  }

  /// Expands the group of `row` (see [`expand`](Self::expand)) if its value is stored.
  fn expand_group_of(&mut self, row: &R) {
    if !self.grouped.is_empty()
      && let Some(id) = self.groups.group_of(row)
    {
      self.expand(id);
    }
  }

  /// Sets the value of every member of the group `id` to `value`, as clearing each
  /// member and adding `value` to it would.
  pub fn assign_group(&mut self, id: GroupId, value: &IndexSet<C>) {
    let groups = Rc::clone(&self.groups);
    let members = groups.group(id).members();
    #[cfg(feature = "shadow-eager")]
    for row in members {
      self.shadow.clear_row(row);
      self.shadow.union_into_row(row.clone(), value);
    }

    if self.grouped.remove(&id).is_none() {
      for row in members {
        self.plain.remove(row);
      }
    }
    if !value.inner().is_empty() {
      self.grouped.insert(id, Rc::new(value.clone()));
    }

    #[cfg(feature = "shadow-eager")]
    for row in members {
      self.check_row(row);
    }
  }

  /// Empties the value of every member of the group `id`, as clearing each member
  /// would.
  pub fn clear_group(&mut self, id: GroupId) {
    let groups = Rc::clone(&self.groups);
    let members = groups.group(id).members();
    #[cfg(feature = "shadow-eager")]
    for row in members {
      self.shadow.clear_row(row);
    }

    if self.grouped.remove(&id).is_none() {
      for row in members {
        self.plain.remove(row);
      }
    }

    #[cfg(feature = "shadow-eager")]
    for row in members {
      self.check_row(row);
    }
  }

  /// Adds the seed of every seeded row to its value.
  pub fn seed(&mut self) {
    for row in self.seeded_rows.values_mut() {
      insert_into_row(&mut row.set, row.col);
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
    if !self.grouped.is_empty()
      && let Some(id) = self.groups.group_of(row)
    {
      // Members of groups are not seeded.
      return self.grouped.get(&id).unwrap_or(&self.seeds.empty);
    }
    match self.seeds.column(row) {
      None => &self.seeds.empty,
      Some(col) => match self.seeded_rows.get(row) {
        Some(explicit) => &explicit.set,
        None => self.implicit(col),
      },
    }
  }

  fn implicit(&self, col: C::Index) -> &Row<C> {
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

    self.expand_group_of(&row);
    let changed = if let Some(set) = self.plain.get_mut(&row) {
      union_set_into_row(set, from)
    } else {
      match self.seeds.column(&row) {
        None => {
          let changed = !from.inner().is_empty();
          if changed {
            self.plain.insert(row, Rc::new(from.clone()));
          }
          changed
        }
        Some(col) => {
          if let Some(explicit) = self.seeded_rows.get_mut(&row) {
            union_set_into_row(&mut explicit.set, from)
          } else {
            let mut set = Rc::clone(self.implicit(col));
            let changed = union_set_into_row(&mut set, from);
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

    self.expand_group_of(row);
    if self.plain.remove(row).is_none()
      && let Some(col) = self.seeds.column(row)
    {
      if self.seeded {
        // Without an explicit value, the row would take its seed.
        let set = Rc::clone(&self.seeds.empty);
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
    let plain = (self.plain.iter())
      .filter(|(_, set)| !set.inner().is_empty())
      .map(|(key, set)| (key, &**set));
    let explicit = self
      .seeded_rows
      .iter()
      .filter(|(_, row)| !row.set.inner().is_empty())
      .map(|(key, row)| (key, &*row.set));
    let implicit = self
      .seeded
      .then(|| {
        self
          .seeds
          .iter()
          .filter(|(key, _)| !self.seeded_rows.contains_key(key))
          .map(|(key, col)| (key, &**self.seeds.singleton(col)))
      })
      .into_iter()
      .flatten();
    let grouped = self.grouped.iter().flat_map(|(id, value)| {
      (self.groups.group(*id).members())
        .iter()
        .map(move |row| (row, &**value))
    });
    plain.chain(explicit).chain(implicit).chain(grouped)
  }

  /// The number of explicitly stored rows (including tombstones), counting the stored
  /// value of a group as one row.
  pub fn explicit_len(&self) -> usize {
    self.plain.len() + self.seeded_rows.len() + self.grouped.len()
  }

  /// The number of rows whose value is the stored value of their group.
  pub fn grouped_len(&self) -> usize {
    (self.grouped.keys())
      .map(|id| self.groups.group(*id).members().len())
      .sum()
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
    debug_assert!(Rc::ptr_eq(&self.groups, &other.groups));
    let was_seeded = self.seeded;
    let mut changed = false;

    // The groups whose value `other` stores: their members have no explicit rows in
    // `other`.
    for (id, value) in &other.grouped {
      if let Some(own) = self.grouped.get_mut(id) {
        changed |= union_row(own, value);
        continue;
      }
      let groups = Rc::clone(&self.groups);
      let members = groups.group(*id).members();
      if members.iter().any(|row| self.plain.contains_key(row)) {
        // Some members have their own values in `self`.
        for row in members {
          match self.plain.get_mut(row) {
            Some(own) => changed |= union_row(own, value),
            None => {
              self.plain.insert(row.clone(), Rc::clone(value));
              changed = true;
            }
          }
        }
      } else {
        // Every member is empty in `self`; `value` is not.
        self.grouped.insert(*id, Rc::clone(value));
        changed = true;
      }
    }

    for (row, set) in &other.plain {
      if !self.plain.contains_key(row) {
        // A member of a group whose value `self` stores needs its own row.
        self.expand_group_of(row);
      }
      match self.plain.get_mut(row) {
        Some(own) => changed |= union_row(own, set),
        None => {
          // `set` is not empty.
          self.plain.insert(row.clone(), Rc::clone(set));
          changed = true;
        }
      }
    }

    for (row, explicit) in &other.seeded_rows {
      match self.seeded_rows.get_mut(row) {
        Some(own) => changed |= union_row(&mut own.set, &explicit.set),
        None => {
          // The row's value in `self` is implicit.
          let mut set = Rc::clone(self.implicit(explicit.col));
          let grew = union_row(&mut set, &explicit.set);
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
          changed |= insert_into_row(&mut own.set, own.col);
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
      grouped: self.grouped.clone(),
      seeds: Rc::clone(&self.seeds),
      groups: Rc::clone(&self.groups),
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
    let groups = &self.groups;
    let grouped = (self.grouped.keys())
      .chain(other.grouped.keys())
      .flat_map(|id| groups.group(*id).members());
    let explicit = (self.plain.keys())
      .chain(other.plain.keys())
      .chain(self.seeded_rows.keys())
      .chain(other.seeded_rows.keys())
      .chain(grouped);
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
    for id in self.grouped.keys() {
      for row in self.groups.group(*id).members() {
        assert!(
          !self.plain.contains_key(row),
          "a member of a stored group has a row"
        );
        self.check_row(row);
      }
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

  /// Sets the value of every member of `group` to `value`.
  fn assign_group(&mut self, group: &RowGroup<R>, value: &IndexSet<C>)
  where
    R: Clone,
  {
    for row in group.members() {
      self.clear_row(row);
      self.union_into_row(row.clone(), value);
    }
  }

  /// Empties the value of every member of `group`.
  fn clear_group(&mut self, group: &RowGroup<R>) {
    for row in group.members() {
      self.clear_row(row);
    }
  }
}

impl<R, C> RowMatrix<R, C> for LazyMatrix<R, C>
where
  R: Clone + Eq + Hash,
  C: IndexedValue + 'static,
{
  fn assign_group(&mut self, group: &RowGroup<R>, value: &IndexSet<C>) {
    debug_assert!(std::ptr::eq(self.groups.group(group.id()), group));
    LazyMatrix::assign_group(self, group.id(), value)
  }

  fn clear_group(&mut self, group: &RowGroup<R>) {
    debug_assert!(std::ptr::eq(self.groups.group(group.id()), group));
    LazyMatrix::clear_group(self, group.id())
  }

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

  const ROWS: u32 = 36;
  /// Rows `SEEDABLE ..` are never seeded; they form the groups `GROUPS`.
  const SEEDABLE: u32 = 24;
  const GROUPS: [std::ops::Range<u32>; 2] = [24 .. 30, 30 .. 36];

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
    assert!(
      lazy.explicit_len() + lazy.implicit_len() + lazy.grouped_len() >= lazy_rows.len()
    );
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
      for row in 0 .. SEEDABLE {
        if rng.below(2) == 0 {
          seeds.push((row, ColIdx::from_usize(30 + rng.below(4))));
        }
      }
      let seed_rows = Rc::new(SeedRows::new(&domain, seeds.iter().copied()));
      let mut groups = RowGroups::none();
      let group_ids = GROUPS
        .iter()
        .map(|rows| groups.add(rows.clone().collect()).unwrap())
        .collect::<Vec<_>>();
      let groups = Rc::new(groups);
      let new_state = || (Lazy::with_groups(&seed_rows, &groups), Eager::new(&domain));
      let mut states = (0 .. 4).map(|_| new_state()).collect::<Vec<_>>();
      for step in 0 .. 60 {
        let i = rng.below(states.len());
        let what = format!("round {round} step {step}");
        match rng.below(13) {
          10 | 11 => {
            let g = rng.below(GROUPS.len());
            let mut from = IndexSet::new(&domain);
            for _ in 0 .. rng.below(3) {
              from.insert(ColIdx::from_usize(rng.below(34)));
            }
            let (lazy, eager) = &mut states[i];
            lazy.assign_group(group_ids[g], &from);
            for row in GROUPS[g].clone() {
              eager.clear_row(&row);
              // The eager matrix would store an empty row, which `==` distinguishes.
              if !from.is_empty() {
                eager.union_into_row(row, &from);
              }
            }
            assert_same(lazy, eager, &format!("{what}: assign group"));
          }
          12 => {
            let g = rng.below(GROUPS.len());
            let (lazy, eager) = &mut states[i];
            lazy.clear_group(group_ids[g]);
            for row in GROUPS[g].clone() {
              eager.clear_row(&row);
            }
            assert_same(lazy, eager, &format!("{what}: clear group"));
          }
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
              states[i] = new_state();
            }
          }
        }
        // Rows are shared between states: writing one state must not change another.
        for (lazy, eager) in &states {
          assert_same(lazy, eager, &format!("{what}: other states"));
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
