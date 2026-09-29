use flowistry::{
  infoflow::mutation::{ModularMutationVisitor, Mutation},
  mir::placeinfo::PlaceInfo,
};
use indexical::bitset::rustc::IndexMatrix;
use rustc_middle::mir::{Body, Mutability, Place, visit::Visitor};
use rustc_utils::mir::location_or_arg::{LocationOrArg, index::LocationOrArgSet};

pub struct DirectInfluence<'a, 'tcx> {
  place_info: &'a PlaceInfo<'a, 'tcx>,
  influence: IndexMatrix<Place<'tcx>, LocationOrArg>,
}

impl<'a, 'tcx> DirectInfluence<'a, 'tcx> {
  pub fn build(body: &Body<'tcx>, place_info: &'a PlaceInfo<'a, 'tcx>) -> Self {
    let mut influence = IndexMatrix::new(place_info.location_domain());

    ModularMutationVisitor::new(place_info, |location, mutations| {
      let mut add = |place: Place<'tcx>, mutability: Mutability| {
        for alias in place_info.reachable_values(place, mutability) {
          influence.insert(*alias, location);
        }
      };

      for Mutation {
        mutated, inputs, ..
      } in mutations
      {
        for input in inputs {
          add(input, Mutability::Not);
        }

        add(mutated, Mutability::Mut);
      }
    })
    .visit_body(body);

    DirectInfluence {
      place_info,
      influence,
    }
  }

  /// The locations that directly influence any of `targets`, each once.
  pub fn lookup(
    &self,
    targets: impl IntoIterator<Item = Place<'tcx>>,
  ) -> LocationOrArgSet {
    let mut locations = LocationOrArgSet::new(self.place_info.location_domain());
    for target in targets {
      for alias in self.place_info.reachable_values(target, Mutability::Not) {
        locations.union(self.influence.row_set(alias));
      }
    }
    locations
  }
}
