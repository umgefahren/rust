use smallvec::smallvec;

use crate::traits::{self, Obligation, ObligationCauseCode, PredicateObligation};
use rustc_data_structures::fx::FxHashSet;
use rustc_middle::ty::{self, Ty, TyCtxt, Upcast};
use rustc_span::symbol::Ident;
use rustc_span::Span;
use rustc_type_ir::outlives::{push_outlives_components, Component};

pub fn anonymize_predicate<'tcx>(
    tcx: TyCtxt<'tcx>,
    pred: ty::Predicate<'tcx>,
) -> ty::Predicate<'tcx> {
    let new = tcx.anonymize_bound_vars(pred.kind());
    tcx.reuse_or_mk_predicate(pred, new)
}

pub struct PredicateSet<'tcx> {
    tcx: TyCtxt<'tcx>,
    set: FxHashSet<ty::Predicate<'tcx>>,
}

impl<'tcx> PredicateSet<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self { tcx, set: Default::default() }
    }

    /// Adds a predicate to the set.
    ///
    /// Returns whether the predicate was newly inserted. That is:
    /// - If the set did not previously contain this predicate, `true` is returned.
    /// - If the set already contained this predicate, `false` is returned,
    ///   and the set is not modified: original predicate is not replaced,
    ///   and the predicate passed as argument is dropped.
    pub fn insert(&mut self, pred: ty::Predicate<'tcx>) -> bool {
        // We have to be careful here because we want
        //
        //    for<'a> Foo<&'a i32>
        //
        // and
        //
        //    for<'b> Foo<&'b i32>
        //
        // to be considered equivalent. So normalize all late-bound
        // regions before we throw things into the underlying set.
        self.set.insert(anonymize_predicate(self.tcx, pred))
    }
}

impl<'tcx> Extend<ty::Predicate<'tcx>> for PredicateSet<'tcx> {
    fn extend<I: IntoIterator<Item = ty::Predicate<'tcx>>>(&mut self, iter: I) {
        for pred in iter {
            self.insert(pred);
        }
    }

    fn extend_one(&mut self, pred: ty::Predicate<'tcx>) {
        self.insert(pred);
    }

    fn extend_reserve(&mut self, additional: usize) {
        Extend::<ty::Predicate<'tcx>>::extend_reserve(&mut self.set, additional);
    }
}

///////////////////////////////////////////////////////////////////////////
// `Elaboration` iterator
///////////////////////////////////////////////////////////////////////////

/// "Elaboration" is the process of identifying all the predicates that
/// are implied by a source predicate. Currently, this basically means
/// walking the "supertraits" and other similar assumptions. For example,
/// if we know that `T: Ord`, the elaborator would deduce that `T: PartialOrd`
/// holds as well. Similarly, if we have `trait Foo: 'static`, and we know that
/// `T: Foo`, then we know that `T: 'static`.
pub struct Elaborator<'tcx, O> {
    stack: Vec<O>,
    visited: PredicateSet<'tcx>,
    mode: Filter,
}

enum Filter {
    All,
    OnlySelf,
    OnlySelfThatDefines(Ident),
}

/// Describes how to elaborate an obligation into a sub-obligation.
///
/// For [`Obligation`], a sub-obligation is combined with the current obligation's
/// param-env and cause code. For [`ty::Predicate`], none of this is needed, since
/// there is no param-env or cause code to copy over.
pub trait Elaboratable<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx>;

    // Makes a new `Self` but with a different clause that comes from elaboration.
    fn child(&self, clause: ty::Clause<'tcx>) -> Self;

    // Makes a new `Self` but with a different clause and a different cause
    // code (if `Self` has one, such as [`PredicateObligation`]).
    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        span: Span,
        parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        index: usize,
    ) -> Self;
}

impl<'tcx> Elaboratable<'tcx> for PredicateObligation<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.predicate
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        Obligation {
            cause: self.cause.clone(),
            param_env: self.param_env,
            recursion_depth: 0,
            predicate: clause.as_predicate(),
        }
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        span: Span,
        parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        index: usize,
    ) -> Self {
        let cause = self.cause.clone().derived_cause(parent_trait_pred, |derived| {
            ObligationCauseCode::ImplDerived(Box::new(traits::ImplDerivedCause {
                derived,
                impl_or_alias_def_id: parent_trait_pred.def_id(),
                impl_def_predicate_index: Some(index),
                span,
            }))
        });
        Obligation {
            cause,
            param_env: self.param_env,
            recursion_depth: 0,
            predicate: clause.as_predicate(),
        }
    }
}

impl<'tcx> Elaboratable<'tcx> for ty::Predicate<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        *self
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        clause.as_predicate()
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        _index: usize,
    ) -> Self {
        clause.as_predicate()
    }
}

impl<'tcx> Elaboratable<'tcx> for (ty::Predicate<'tcx>, Span) {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.0
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        (clause.as_predicate(), self.1)
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        _index: usize,
    ) -> Self {
        (clause.as_predicate(), self.1)
    }
}

impl<'tcx> Elaboratable<'tcx> for (ty::Clause<'tcx>, Span) {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.0.as_predicate()
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        (clause, self.1)
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        _index: usize,
    ) -> Self {
        (clause, self.1)
    }
}

impl<'tcx> Elaboratable<'tcx> for ty::Clause<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.as_predicate()
    }

    fn child(&self, clause: ty::Clause<'tcx>) -> Self {
        clause
    }

    fn child_with_derived_cause(
        &self,
        clause: ty::Clause<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        _index: usize,
    ) -> Self {
        clause
    }
}

pub fn elaborate<'tcx, O: Elaboratable<'tcx>>(
    tcx: TyCtxt<'tcx>,
    obligations: impl IntoIterator<Item = O>,
) -> Elaborator<'tcx, O> {
    let mut elaborator =
        Elaborator { stack: Vec::new(), visited: PredicateSet::new(tcx), mode: Filter::All };
    elaborator.extend_deduped(obligations);
    elaborator
}

impl<'tcx, O: Elaboratable<'tcx>> Elaborator<'tcx, O> {
    fn extend_deduped(&mut self, obligations: impl IntoIterator<Item = O>) {
        // Only keep those bounds that we haven't already seen.
        // This is necessary to prevent infinite recursion in some
        // cases. One common case is when people define
        // `trait Sized: Sized { }` rather than `trait Sized { }`.
        // let visited = &mut self.visited;
        self.stack.extend(obligations.into_iter().filter(|o| self.visited.insert(o.predicate())));
    }

    /// Filter to only the supertraits of trait predicates, i.e. only the predicates
    /// that have `Self` as their self type, instead of all implied predicates.
    pub fn filter_only_self(mut self) -> Self {
        self.mode = Filter::OnlySelf;
        self
    }

    /// Filter to only the supertraits of trait predicates that define the assoc_ty.
    pub fn filter_only_self_that_defines(mut self, assoc_ty: Ident) -> Self {
        self.mode = Filter::OnlySelfThatDefines(assoc_ty);
        self
    }

    fn elaborate(&mut self, elaboratable: &O) {
        let tcx = self.visited.tcx;

        // We only elaborate clauses.
        let Some(clause) = elaboratable.predicate().as_clause() else {
            return;
        };

        let bound_clause = clause.kind();
        match bound_clause.skip_binder() {
            ty::ClauseKind::Trait(data) => {
                // Negative trait bounds do not imply any supertrait bounds
                if data.polarity != ty::PredicatePolarity::Positive {
                    return;
                }
                // Get predicates implied by the trait, or only super predicates if we only care about self predicates.
                let predicates = match self.mode {
                    Filter::All => tcx.explicit_implied_predicates_of(data.def_id()),
                    Filter::OnlySelf => tcx.explicit_super_predicates_of(data.def_id()),
                    Filter::OnlySelfThatDefines(ident) => {
                        tcx.explicit_supertraits_containing_assoc_item((data.def_id(), ident))
                    }
                };

                let obligations =
                    predicates.predicates.iter().enumerate().map(|(index, &(clause, span))| {
                        elaboratable.child_with_derived_cause(
                            clause.instantiate_supertrait(tcx, bound_clause.rebind(data.trait_ref)),
                            span,
                            bound_clause.rebind(data),
                            index,
                        )
                    });
                debug!(?data, ?obligations, "super_predicates");
                self.extend_deduped(obligations);
            }
            ty::ClauseKind::TypeOutlives(ty::OutlivesPredicate(ty_max, r_min)) => {
                // We know that `T: 'a` for some type `T`. We can
                // often elaborate this. For example, if we know that
                // `[U]: 'a`, that implies that `U: 'a`. Similarly, if
                // we know `&'a U: 'b`, then we know that `'a: 'b` and
                // `U: 'b`.
                //
                // We can basically ignore bound regions here. So for
                // example `for<'c> Foo<'a,'c>: 'b` can be elaborated to
                // `'a: 'b`.

                // Ignore `for<'a> T: 'a` -- we might in the future
                // consider this as evidence that `T: 'static`, but
                // I'm a bit wary of such constructions and so for now
                // I want to be conservative. --nmatsakis
                if r_min.is_bound() {
                    return;
                }

                let mut components = smallvec![];
                push_outlives_components(tcx, ty_max, &mut components);
                self.extend_deduped(
                    components
                        .into_iter()
                        .filter_map(|component| match component {
                            Component::Region(r) => {
                                if r.is_bound() {
                                    None
                                } else {
                                    Some(ty::ClauseKind::RegionOutlives(ty::OutlivesPredicate(
                                        r, r_min,
                                    )))
                                }
                            }

                            Component::Param(p) => {
                                let ty = Ty::new_param(tcx, p.index, p.name);
                                Some(ty::ClauseKind::TypeOutlives(ty::OutlivesPredicate(ty, r_min)))
                            }

                            Component::Placeholder(p) => {
                                let ty = Ty::new_placeholder(tcx, p);
                                Some(ty::ClauseKind::TypeOutlives(ty::OutlivesPredicate(ty, r_min)))
                            }

                            Component::UnresolvedInferenceVariable(_) => None,

                            Component::Alias(alias_ty) => {
                                // We might end up here if we have `Foo<<Bar as Baz>::Assoc>: 'a`.
                                // With this, we can deduce that `<Bar as Baz>::Assoc: 'a`.
                                Some(ty::ClauseKind::TypeOutlives(ty::OutlivesPredicate(
                                    alias_ty.to_ty(tcx),
                                    r_min,
                                )))
                            }

                            Component::EscapingAlias(_) => {
                                // We might be able to do more here, but we don't
                                // want to deal with escaping vars right now.
                                None
                            }
                        })
                        .map(|clause| elaboratable.child(bound_clause.rebind(clause).upcast(tcx))),
                );
            }
            ty::ClauseKind::RegionOutlives(..) => {
                // Nothing to elaborate from `'a: 'b`.
            }
            ty::ClauseKind::WellFormed(..) => {
                // Currently, we do not elaborate WF predicates,
                // although we easily could.
            }
            ty::ClauseKind::Projection(..) => {
                // Nothing to elaborate in a projection predicate.
            }
            ty::ClauseKind::ConstEvaluatable(..) => {
                // Currently, we do not elaborate const-evaluatable
                // predicates.
            }
            ty::ClauseKind::ConstArgHasType(..) => {
                // Nothing to elaborate
            }
        }
    }
}

impl<'tcx, O: Elaboratable<'tcx>> Iterator for Elaborator<'tcx, O> {
    type Item = O;

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.stack.len(), None)
    }

    fn next(&mut self) -> Option<Self::Item> {
        // Extract next item from top-most stack frame, if any.
        if let Some(obligation) = self.stack.pop() {
            self.elaborate(&obligation);
            Some(obligation)
        } else {
            None
        }
    }
}

///////////////////////////////////////////////////////////////////////////
// Supertrait iterator
///////////////////////////////////////////////////////////////////////////

pub fn supertraits<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_ref: ty::PolyTraitRef<'tcx>,
) -> FilterToTraits<Elaborator<'tcx, ty::Predicate<'tcx>>> {
    elaborate(tcx, [trait_ref.upcast(tcx)]).filter_only_self().filter_to_traits()
}

pub fn transitive_bounds<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_refs: impl Iterator<Item = ty::PolyTraitRef<'tcx>>,
) -> FilterToTraits<Elaborator<'tcx, ty::Predicate<'tcx>>> {
    elaborate(tcx, trait_refs.map(|trait_ref| trait_ref.upcast(tcx)))
        .filter_only_self()
        .filter_to_traits()
}

/// A specialized variant of `elaborate` that only elaborates trait references that may
/// define the given associated item with the name `assoc_name`. It uses the
/// `explicit_supertraits_containing_assoc_item` query to avoid enumerating super-predicates that
/// aren't related to `assoc_item`. This is used when resolving types like `Self::Item` or
/// `T::Item` and helps to avoid cycle errors (see e.g. #35237).
pub fn transitive_bounds_that_define_assoc_item<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_refs: impl Iterator<Item = ty::PolyTraitRef<'tcx>>,
    assoc_name: Ident,
) -> FilterToTraits<Elaborator<'tcx, ty::Predicate<'tcx>>> {
    elaborate(tcx, trait_refs.map(|trait_ref| trait_ref.upcast(tcx)))
        .filter_only_self_that_defines(assoc_name)
        .filter_to_traits()
}

///////////////////////////////////////////////////////////////////////////
// Other
///////////////////////////////////////////////////////////////////////////

impl<'tcx> Elaborator<'tcx, ty::Predicate<'tcx>> {
    fn filter_to_traits(self) -> FilterToTraits<Self> {
        FilterToTraits { base_iterator: self }
    }
}

/// A filter around an iterator of predicates that makes it yield up
/// just trait references.
pub struct FilterToTraits<I> {
    base_iterator: I,
}

impl<'tcx, I: Iterator<Item = ty::Predicate<'tcx>>> Iterator for FilterToTraits<I> {
    type Item = ty::PolyTraitRef<'tcx>;

    fn next(&mut self) -> Option<ty::PolyTraitRef<'tcx>> {
        while let Some(pred) = self.base_iterator.next() {
            if let Some(data) = pred.as_trait_clause() {
                return Some(data.map_bound(|t| t.trait_ref));
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (_, upper) = self.base_iterator.size_hint();
        (0, upper)
    }
}
