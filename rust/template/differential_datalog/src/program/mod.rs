//! Datalog program.
//!
//! The client constructs a `struct Program` that describes Datalog relations and rules and
//! calls `Program::run()` to instantiate the program.  The method returns an error or an
//! instance of `RunningProgram` that can be used to interact with the program at runtime.
//! Interactions include starting, committing or rolling back a transaction and modifying input
//! relations. The engine invokes user-provided callbacks as records are added or removed from
//! relations. `RunningProgram::stop()` terminates the Datalog program destroying all its state.
//! If not invoked manually (which allows for manual error handling), `RunningProgram::stop`
//! will be called when the program object leaves scope.

// TODO: namespace cleanup
// TODO: single input relation

pub mod arrange;
pub mod config;
mod timestamp;
mod update;
mod worker;

use crate::{
    ddval::*,
    record::Mutator,
    render::{
        arrange_by::{ArrangeBy, ArrangementKind},
        RenderContext,
    },
};
use abomonation_derive::Abomonation;
pub use arrange::diff_distinct;
use arrange::{
    antijoin_arranged, Arrangement as DataflowArrangement, ArrangementFlavor, Arrangements,
};
use config::SelfProfilingRig;
pub use config::{Config, ProfilingConfig};
use crossbeam_channel::{Receiver, Sender};
use ddlog_profiler::{
    with_prof_context, ArrangementDebugInfo, DDlogSourceCode, OperatorDebugInfo, ProfMsg, Profile,
    RuleDebugInfo, SourcePosition,
};
use fnv::{FnvHashMap, FnvHashSet};
use num::{One, Zero};
use std::{
    any::Any,
    borrow::Cow,
    cmp,
    collections::{hash_map, BTreeSet},
    fmt::{self, Debug, Formatter},
    iter::{self, Cycle, Skip},
    ops::{Add, AddAssign, Mul, Neg, Range},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};
use timestamp::ToTupleTS;
pub use timestamp::{TSNested, TupleTS, TS};
use triomphe::Arc as ThinArc;
pub use update::Update;
use worker::DDlogWorker;

use differential_dataflow::difference::*;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::arrange::arrangement::Arranged;
use differential_dataflow::operators::arrange::*;
use differential_dataflow::operators::*;
use differential_dataflow::trace::implementations::ord::OrdKeySpine as DefaultKeyTrace;
use differential_dataflow::trace::implementations::ord::OrdValSpine as DefaultValTrace;
use differential_dataflow::trace::wrappers::enter::TraceEnter;
use differential_dataflow::trace::{BatchReader, Cursor, TraceReader};
use differential_dataflow::Collection;
use dogsdogsdogs::{
    altneu::AltNeu,
    calculus::{Differentiate, Integrate},
    operators::lookup_map,
};
use timely::communication::{initialize::WorkerGuards, Allocator};
use timely::dataflow::scopes::*;
use timely::order::TotalOrder;
use timely::progress::{timestamp::Refines, PathSummary, Timestamp};
use timely::worker::Worker;

type ValTrace<S> = DefaultValTrace<DDValue, DDValue, S, Weight, u32>;
type KeyTrace<S> = DefaultKeyTrace<DDValue, S, Weight, u32>;

type TValAgent<S> = TraceAgent<ValTrace<S>>;
type TKeyAgent<S> = TraceAgent<KeyTrace<S>>;

type TValEnter<P, T> = TraceEnter<TValAgent<P>, T>;
type TKeyEnter<P, T> = TraceEnter<TKeyAgent<P>, T>;

#[derive(Abomonation, Copy, Ord, PartialOrd, Eq, PartialEq, Hash, Debug, Clone)]
#[repr(transparent)]
pub struct CheckedWeight {
    pub value: i32,
}

impl Semigroup for CheckedWeight {
    fn is_zero(&self) -> bool {
        self.value == 0
    }
}

impl<'a> AddAssign<&'a Self> for CheckedWeight {
    fn add_assign(&mut self, other: &'a Self) {
        // intentional panic on overflow
        self.value = self
            .value
            .checked_add(other.value)
            .expect("Weight overflow");
    }
}

impl Add for CheckedWeight {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        // intentional panic on overflow
        Self {
            value: self
                .value
                .checked_add(other.value)
                .expect("Weight overflow"),
        }
    }
}

impl Mul for CheckedWeight {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self::Output {
        // intentional panic on overflow
        Self {
            value: self.value.checked_mul(rhs.value).expect("Weight overflow"),
        }
    }
}

impl Neg for CheckedWeight {
    type Output = Self;

    fn neg(self) -> Self::Output {
        // intentional panic on overflow
        Self {
            value: self.value.checked_neg().expect("Weight overflow"),
        }
    }
}

impl Monoid for CheckedWeight {
    fn zero() -> Self {
        Self { value: 0 }
    }
}

impl One for CheckedWeight {
    fn one() -> Self {
        Self { value: 1 }
    }
}

impl Zero for CheckedWeight {
    fn zero() -> Self {
        Self { value: 0 }
    }
    fn is_zero(&self) -> bool {
        self.value == 0
    }
}

impl From<i32> for CheckedWeight {
    fn from(item: i32) -> Self {
        Self { value: item }
    }
}

impl From<CheckedWeight> for i64 {
    fn from(item: CheckedWeight) -> Self {
        item.value as i64
    }
}

/// Weight is a diff associated with records in differential dataflow
#[cfg(feature = "checked_weights")]
pub type Weight = CheckedWeight;

/// Weight is a diff associated with records in differential dataflow
#[cfg(not(feature = "checked_weights"))]
pub type Weight = i32;

/// Message buffer for profiling messages
const PROF_MSG_BUF_SIZE: usize = 10_000;

/// Result type returned by this library
pub type Response<X> = Result<X, String>;

/// Unique identifier of a DDlog relation.
// TODO: Newtype this for type-safety
pub type RelId = usize;

/// Unique identifier of an index.
// TODO: Newtype this for type-safety
pub type IdxId = usize;

/// Unique identifier of an arranged relation.
/// The first element of the tuple identifies relation; the second is the index
/// of arrangement for the given relation.
// TODO: Newtype this for type-safety
pub type ArrId = (RelId, usize);

/// Function type used to map the content of a relation
/// (see `XFormCollection::Map`).
pub type MapFunc = fn(DDValue) -> DDValue;

/// Function type used to extract join key from a relation
/// (see `XFormCollection::StreamJoin`).
pub type KeyFunc = fn(&DDValue) -> Option<DDValue>;

/// (see `XFormCollection::FlatMap`).
pub type FlatMapFunc = fn(DDValue) -> Option<Box<dyn Iterator<Item = DDValue>>>;

/// Function type used to filter a relation
/// (see `XForm*::Filter`).
pub type FilterFunc = fn(&DDValue) -> bool;

/// Function type used to simultaneously filter and map a relation
/// (see `XFormCollection::FilterMap`).
pub type FilterMapFunc = fn(DDValue) -> Option<DDValue>;

/// Function type used to inspect a relation
/// (see `XFormCollection::InspectFunc`)
pub type InspectFunc = fn(&DDValue, TupleTS, Weight) -> ();

/// Function type used to arrange a relation into key-value pairs
/// (see `XFormArrangement::Join`, `XFormArrangement::Antijoin`).
pub type ArrangeFunc = fn(DDValue) -> Option<(DDValue, DDValue)>;

/// Function type used to assemble the result of a join into a value.
/// Takes join key and a pair of values from the two joined relations
/// (see `XFormArrangement::Join`).
pub type JoinFunc = fn(&DDValue, &DDValue, &DDValue) -> Option<DDValue>;

/// Similar to JoinFunc, but only takes values from the two joined
/// relations, and not the key (`XFormArrangement::StreamJoin`).
pub type ValJoinFunc = fn(&DDValue, &DDValue) -> Option<DDValue>;

/// Function type used to assemble the result of a semijoin into a value.
/// Takes join key and value (see `XFormArrangement::Semijoin`).
pub type SemijoinFunc = fn(&DDValue, &DDValue, &()) -> Option<DDValue>;

/// Similar to SemijoinFunc, but only takes one value.
/// (see `XFormCollection::StreamSemijoin`).
pub type StreamSemijoinFunc = fn(&DDValue) -> Option<DDValue>;

/// Aggregation function: aggregates multiple values into a single value.
pub type AggFunc = fn(&DDValue, &[(&DDValue, Weight)]) -> Option<DDValue>;

// TODO: add validating constructor for Program:
// - relation id's are unique
// - rules only refer to previously declared relations or relations in the local scc
// - input relations do not occur in LHS of rules
// - all references to arrangements are valid
/// A Datalog program is a vector of nodes representing
/// individual non-recursive relations and strongly connected components
/// comprised of one or more mutually recursive relations.
/// * `delayed_rels` - delayed relations used in the program.
/// * `init_data` - initial relation contents.
#[derive(Clone)]
pub struct Program {
    pub nodes: Vec<ProgNode>,
    pub delayed_rels: Vec<DelayedRelation>,
    pub init_data: Vec<(RelId, DDValue)>,
}

type TransformerMap<'a> =
    FnvHashMap<RelId, Collection<Child<'a, Worker<Allocator>, TS>, DDValue, Weight>>;

/// Represents a dataflow fragment implemented outside of DDlog directly in differential-dataflow.
///
/// Takes the set of already constructed collections and modifies this
/// set, adding new collections. Note that the transformer can only be applied in the top scope
/// (`Child<'a, Worker<Allocator>, TS>`), as we currently don't have a way to ensure that the
/// transformer is monotonic and thus it may not converge if used in a nested scope.
pub type TransformerFuncRes = Box<dyn for<'a> Fn(&mut TransformerMap<'a>)>;

/// A function returning a dataflow fragment implemented in differential-dataflow
pub type TransformerFunc = fn() -> TransformerFuncRes;

/// Program node is either an individual non-recursive relation, a transformer application or
/// a vector of one or more mutually recursive relations.
#[derive(Clone)]
pub enum ProgNode {
    Rel {
        rel: Relation,
    },
    Apply {
        transformer: Cow<'static, str>,
        source_pos: SourcePosition,
        tfun: TransformerFunc,
    },
    Scc {
        rels: Vec<RecursiveRelation>,
    },
}

/// Relation computed in a nested scope as a fixed point.
///
/// The `distinct` flag indicates that the `distinct` operator should be applied
/// to the relation before closing the loop to enforce convergence of the fixed
/// point computation.
#[derive(Clone)]
pub struct RecursiveRelation {
    pub rel: Relation,
    pub distinct: bool,
}

pub trait RelationCallback: Fn(RelId, &DDValue, Weight) + Send + Sync {
    fn clone_boxed(&self) -> Box<dyn RelationCallback>;
}

impl<T> RelationCallback for T
where
    T: Fn(RelId, &DDValue, Weight) + Clone + Send + Sync + ?Sized + 'static,
{
    fn clone_boxed(&self) -> Box<dyn RelationCallback> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn RelationCallback> {
    fn clone(&self) -> Self {
        self.clone_boxed()
    }
}

/// Caching mode for input relations only
///
/// `NoCache` - don't cache the contents of the relation.
/// `CacheSet` - cache relation as a set.  Duplicate inserts are
///     ignored (for relations without a key) or fail (for relations
///     with key).
/// `CacheMultiset` - cache relation as a generalized multiset with
///     integer weights.
#[derive(Clone)]
pub enum CachingMode {
    Stream,
    Set,
    Multiset,
}

/// Datalog relation.
///
/// defines a set of rules and a set of arrangements with which this relation is used in
/// rules.  The set of rules can be empty (if this is a ground relation); the set of arrangements
/// can also be empty if the relation is not used in the RHS of any rules.
#[derive(Clone)]
pub struct Relation {
    /// Relation name; does not have to be unique
    pub name: Cow<'static, str>,
    /// Location of relation declaration.
    pub source_pos: SourcePosition,
    /// `true` if this is an input relation. Input relations are populated by the client
    /// of the library via `RunningProgram::insert()`, `RunningProgram::delete()` and `RunningProgram::apply_updates()` methods.
    pub input: bool,
    /// Apply distinct_total() to this relation after concatenating all its rules
    pub distinct: bool,
    /// Caching mode (for input relations only).
    pub caching_mode: CachingMode,
    /// If `key_func` is present, this indicates that the relation is indexed with a unique
    /// key computed by key_func
    pub key_func: Option<fn(&DDValue) -> DDValue>,
    /// Unique relation id
    pub id: RelId,
    /// Rules that define the content of the relation.
    /// Input relations cannot have rules.
    /// Rules can only refer to relations introduced earlier in the program as well as relations in the same strongly connected
    /// component.
    pub rules: Vec<Rule>,
    /// Arrangements of the relation used to compute other relations.  Index in this vector
    /// along with relation id uniquely identifies the arrangement (see `ArrId`).
    pub arrangements: Vec<Arrangement>,
    /// Callback invoked when an element is added or removed from relation.
    pub change_cb: Option<Arc<dyn RelationCallback + 'static>>,
}

impl Relation {
    pub fn name(&self) -> &str {
        &*self.name
    }
}

/// `DelayedRelation` refers to the contents of a given base relation from
/// `delay` epochs ago.
///
/// The use of delayed relations in rules comes with an additional constraint.
/// A delayed relation produces outputs ahead of time, e.g., at time `ts` it
/// can yield values labeled `ts + delay`.  In DDlog we don't want to see these
/// values until we explicitly advance the epoch to `ts + delay`.  We apply the
/// `consolidate` operator before `probe`, which guarantees that any
/// output can only be produced once DD knows that it should not expect any more
/// changes for the given timstamp.  So as long as each output relation depends on
/// at least one regular (not delayed) relation, we shouldn't observe any values
/// generated ahead of time.  It is up to the compiler to enforce this
/// constraint.
#[derive(Clone)]
pub struct DelayedRelation {
    /// Source code locations where this delayed relation is used in rules.
    pub used_at: Vec<SourcePosition>,
    /// Unique id of this delayed relation.  Delayed and regular relation ids live in the
    /// same name space and therefore cannot clash.
    pub id: RelId,
    /// Id of the base relation that this DelayedRelation is a delayed version of.
    pub rel_id: RelId,
    /// The number of epochs to delay by.  Must be greater than 0.
    pub delay: TS,
    // /// We don't have a use case for this, and this is not exposed through the DDlog syntax (since
    // /// delayed relations are currently only used with streams), but we could in principle have
    // /// shared arrangements of delayed relations.
    // pub arrangements: Vec<Arrangement>,
}

/// A Datalog relation or rule can depend on other relations and their
/// arrangements.
#[derive(Copy, PartialEq, Eq, Hash, Debug, Clone)]
pub enum Dep {
    Rel(RelId),
    Arr(ArrId),
}

impl Dep {
    pub fn relid(&self) -> RelId {
        match self {
            Dep::Rel(relid) => *relid,
            Dep::Arr((relid, _)) => *relid,
        }
    }
}

/// Transformations, such as maps, flatmaps, filters, joins, etc. are the building blocks of
/// DDlog rules.
///
/// Different kinds of transformations can be applied only to flat collections,
/// only to arranged collections, or both. We therefore use separate types to represent
/// collection and arrangement transformations.
///
/// Note that differential sometimes allows the same kind of transformation to be applied to both
/// collections and arrangements; however the former is implemented on top of the latter and incurs
/// the additional cost of arranging the collection. We only support the arranged version of these
/// transformations, forcing the user to explicitly arrange the collection if necessary (or, as much
/// as possible, keep the data arranged throughout the chain of transformations).
///
/// `XFormArrangement` - arrangement transformation.
#[derive(Clone)]
pub enum XFormArrangement {
    /// FlatMap arrangement into a collection
    FlatMap {
        debug_info: OperatorDebugInfo,
        fmfun: FlatMapFunc,
        /// Transformation to apply to resulting collection.
        /// `None` terminates the chain of transformations.
        next: Box<Option<XFormCollection>>,
    },
    FilterMap {
        debug_info: OperatorDebugInfo,
        fmfun: FilterMapFunc,
        /// Transformation to apply to resulting collection.
        /// `None` terminates the chain of transformations.
        next: Box<Option<XFormCollection>>,
    },
    /// Aggregate
    Aggregate {
        debug_info: OperatorDebugInfo,
        /// Filter arrangement before grouping
        ffun: Option<FilterFunc>,
        /// Aggregation to apply to each group.
        aggfun: AggFunc,
        /// Apply transformation to the resulting collection.
        next: Box<Option<XFormCollection>>,
    },
    /// Join
    Join {
        debug_info: OperatorDebugInfo,
        /// Filter arrangement before joining
        ffun: Option<FilterFunc>,
        /// Arrangement to join with.
        arrangement: ArrId,
        /// Function used to put together ouput value.
        jfun: JoinFunc,
        /// Join returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
    /// Semijoin
    Semijoin {
        debug_info: OperatorDebugInfo,
        /// Filter arrangement before joining
        ffun: Option<FilterFunc>,
        /// Arrangement to semijoin with.
        arrangement: ArrId,
        /// Function used to put together ouput value.
        jfun: SemijoinFunc,
        /// Join returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
    /// Return a subset of values that correspond to keys not present in `arrangement`.
    Antijoin {
        debug_info: OperatorDebugInfo,
        /// Filter arrangement before joining
        ffun: Option<FilterFunc>,
        /// Arrangement to antijoin with
        arrangement: ArrId,
        /// Antijoin returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
    /// Streaming join: join arrangement with a collection.
    /// This outputs a collection obtained by matching each value
    /// in the input collection against the arrangement without
    /// arranging the collection first.
    StreamJoin {
        debug_info: OperatorDebugInfo,
        /// Filter arrangement before join.
        ffun: Option<FilterFunc>,
        /// Relation to join with.
        rel: RelId,
        /// Extract join key from the _collection_.
        kfun: KeyFunc,
        /// Function used to put together ouput value.  The first argument comes
        /// from the arrangement, the second from the collection.
        jfun: ValJoinFunc,
        /// Join returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
    /// Streaming semijoin.
    StreamSemijoin {
        debug_info: OperatorDebugInfo,
        /// Filter arrangement before join.
        ffun: Option<FilterFunc>,
        /// Relation to join with.
        rel: RelId,
        /// Extract join key from the relation.
        kfun: KeyFunc,
        /// Function used to put together ouput value.
        jfun: StreamSemijoinFunc,
        /// Join returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
}

impl XFormArrangement {
    pub(super) fn dependencies(&self) -> FnvHashSet<Dep> {
        match self {
            XFormArrangement::FlatMap { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormArrangement::FilterMap { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormArrangement::Aggregate { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormArrangement::Join {
                arrangement, next, ..
            } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Arr(*arrangement));
                deps
            }
            XFormArrangement::Semijoin {
                arrangement, next, ..
            } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Arr(*arrangement));
                deps
            }
            XFormArrangement::Antijoin {
                arrangement, next, ..
            } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Arr(*arrangement));
                deps
            }
            XFormArrangement::StreamJoin { rel, next, .. } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Rel(*rel));
                deps
            }
            XFormArrangement::StreamSemijoin { rel, next, .. } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Rel(*rel));
                deps
            }
        }
    }
}

/// `XFormCollection` - collection transformation.
#[derive(Clone)]
pub enum XFormCollection {
    /// Arrange the collection, apply `next` transformation to the resulting collection.
    Arrange {
        debug_info: OperatorDebugInfo,
        afun: ArrangeFunc,
        next: Box<XFormArrangement>,
    },
    /// The `Differentiate` operator subtracts the previous value
    /// of the collection from its current value, `C' = C - (C_-1)`.
    /// Can be used to transform a stream into a relation that stores
    /// the new values in the stream for each timestamp.
    Differentiate {
        debug_info: OperatorDebugInfo,
        next: Box<Option<XFormCollection>>,
    },
    /// Apply `mfun` to each element in the collection
    Map {
        debug_info: OperatorDebugInfo,
        mfun: MapFunc,
        next: Box<Option<XFormCollection>>,
    },
    /// FlatMap
    FlatMap {
        debug_info: OperatorDebugInfo,
        fmfun: FlatMapFunc,
        next: Box<Option<XFormCollection>>,
    },
    /// Filter collection
    Filter {
        debug_info: OperatorDebugInfo,
        ffun: FilterFunc,
        next: Box<Option<XFormCollection>>,
    },
    /// Map and filter
    FilterMap {
        debug_info: OperatorDebugInfo,
        fmfun: FilterMapFunc,
        next: Box<Option<XFormCollection>>,
    },
    /// Inspector
    Inspect {
        debug_info: OperatorDebugInfo,
        ifun: InspectFunc,
        next: Box<Option<XFormCollection>>,
    },
    /// Streaming join: join collection with an arrangement.
    /// This outputs a collection obtained by matching each value
    /// in the input collection against the arrangement without
    /// arranging the collection first.
    StreamJoin {
        debug_info: OperatorDebugInfo,
        /// Function to arrange collection into key/value pairs.
        afun: ArrangeFunc,
        /// Arrangement to join with.
        arrangement: ArrId,
        /// Function used to put together ouput values (the first argument
        /// comes from the collection, the second -- from the arrangement).
        jfun: ValJoinFunc,
        /// Join returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
    /// Streaming semijoin.
    StreamSemijoin {
        debug_info: OperatorDebugInfo,
        /// Function to arrange collection into key/value pairs.
        afun: ArrangeFunc,
        /// Arrangement to join with.
        arrangement: ArrId,
        /// Function used to put together ouput values (the first argument
        /// comes from the collection, the second -- from the arrangement).
        jfun: StreamSemijoinFunc,
        /// Join returns a collection: apply `next` transformation to it.
        next: Box<Option<XFormCollection>>,
    },
    /// Applies `xform` to the stream (i.e., to changes to the collection in
    /// the last timestamp) and produces the result while discarding any
    /// intermediate arrangements used to construct the result.
    /// Example: `xform` may arrange and aggregate the collection.  This
    /// will output the aggregate of values added at the current timestamp
    /// after each transaction.  Since the arrangement is instantly cleared,
    /// the old value of the aggregate will not get retracted during the next
    /// transaction.
    ///
    /// Stream xforms are currently only supported in the top-level contents.
    ///
    /// This transformation is implemented using the "calculus" feature of DD:
    /// it constructs an `AltNeu` scope, moves the collection into it using the
    /// `calculus::differentiate` operator, applies `xform` and extracts the
    /// result using `calculus:integrate`.
    /// NOTE: This is an experimental feature.  We currently don't
    /// have real use cases for it (stream joins are already more efficiently
    /// implemented using `lookup_map`, stream aggregation does not sound like
    /// a very useful feature to me, stream antijoins might be the killer app
    /// here), and the implementation is ugly.
    /// It might go away if we don't find what it's good for.
    StreamXForm {
        debug_info: OperatorDebugInfo,
        xform: Box<Option<XFormCollection>>,
        next: Box<Option<XFormCollection>>,
    },
}

impl XFormCollection {
    pub fn dependencies(&self) -> FnvHashSet<Dep> {
        match self {
            XFormCollection::Arrange { next, .. } => next.dependencies(),
            XFormCollection::Differentiate { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormCollection::Map { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormCollection::FlatMap { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormCollection::Filter { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormCollection::FilterMap { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormCollection::Inspect { next, .. } => match **next {
                None => FnvHashSet::default(),
                Some(ref n) => n.dependencies(),
            },
            XFormCollection::StreamJoin {
                arrangement, next, ..
            } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Arr(*arrangement));
                deps
            }
            XFormCollection::StreamSemijoin {
                arrangement, next, ..
            } => {
                let mut deps = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps.insert(Dep::Arr(*arrangement));
                deps
            }
            XFormCollection::StreamXForm { xform, next, .. } => {
                let deps1 = match **xform {
                    None => FnvHashSet::default(),
                    Some(ref x) => x.dependencies(),
                };
                let deps2 = match **next {
                    None => FnvHashSet::default(),
                    Some(ref n) => n.dependencies(),
                };
                deps1.union(&deps2).cloned().collect()
            }
        }
    }
}

/// Datalog rule (more precisely, the body of a rule) starts with a collection
/// or arrangement and applies a chain of transformations to it.
#[derive(Clone)]
pub enum Rule {
    CollectionRule {
        debug_info: RuleDebugInfo,
        rel: RelId,
        xform: Option<XFormCollection>,
    },
    ArrangementRule {
        debug_info: RuleDebugInfo,
        arr: ArrId,
        xform: XFormArrangement,
    },
}

impl Rule {
    fn dependencies(&self) -> FnvHashSet<Dep> {
        match self {
            Rule::CollectionRule { rel, xform, .. } => {
                let mut deps = match xform {
                    None => FnvHashSet::default(),
                    Some(ref x) => x.dependencies(),
                };
                deps.insert(Dep::Rel(*rel));
                deps
            }

            Rule::ArrangementRule { arr, xform, .. } => {
                let mut deps = xform.dependencies();
                deps.insert(Dep::Arr(*arr));
                deps
            }
        }
    }
}

/// Describes arrangement of a relation.
#[derive(Clone)]
pub enum Arrangement {
    /// Arrange into (key,value) pairs
    Map {
        debug_info: ArrangementDebugInfo,
        /// Function used to produce arrangement.
        afun: ArrangeFunc,
        /// The arrangement can be queried using `RunningProgram::query_arrangement`
        /// and `RunningProgram::dump_arrangement`.
        queryable: bool,
    },
    /// Arrange into a set of values
    Set {
        debug_info: ArrangementDebugInfo,
        /// Function used to produce arrangement.
        fmfun: FilterMapFunc,
        /// Apply distinct_total() before arranging filtered collection.
        /// This is necessary if the arrangement is to be used in an antijoin.
        distinct: bool,
    },
}

impl Arrangement {
    fn arrange_by(&self) -> &Cow<'static, str> {
        match self {
            Arrangement::Map { debug_info, .. } => &debug_info.arrange_by,
            Arrangement::Set { debug_info, .. } => &debug_info.arrange_by,
        }
    }

    fn used_at(&self) -> &[SourcePosition] {
        match self {
            Arrangement::Map { debug_info, .. } => debug_info.used_at.as_slice(),
            Arrangement::Set { debug_info, .. } => debug_info.used_at.as_slice(),
        }
    }

    fn used_in_indexes(&self) -> &[Cow<'static, str>] {
        match self {
            Arrangement::Map { debug_info, .. } => debug_info.used_in_indexes.as_slice(),
            Arrangement::Set { debug_info, .. } => debug_info.used_in_indexes.as_slice(),
        }
    }

    fn queryable(&self) -> bool {
        match *self {
            Arrangement::Map { queryable, .. } => queryable,
            Arrangement::Set { .. } => false,
        }
    }

    fn build_arrangement_root<S>(
        &self,
        render_context: &RenderContext,
        collection: &Collection<S, DDValue, Weight>,
    ) -> DataflowArrangement<S, Weight, TValAgent<S::Timestamp>, TKeyAgent<S::Timestamp>>
    where
        S: Scope,
        Collection<S, DDValue, Weight>: ThresholdTotal<S, DDValue, Weight>,
        S::Timestamp: Lattice + Ord + TotalOrder,
    {
        let kind = match *self {
            Arrangement::Map { afun, .. } => ArrangementKind::Map {
                value_function: afun,
            },
            Arrangement::Set {
                fmfun, distinct, ..
            } => {
                // TODO: We don't currently produce a `None` as the key extraction
                //       function, but doing so will simplify the dataflow graph
                //       in instances where a function isn't needed
                ArrangementKind::Set {
                    key_function: Some(fmfun),
                    distinct,
                }
            }
        };

        ArrangeBy {
            kind,
            target_relation: self.arrange_by().clone(),
        }
        .render_root(render_context, collection)
    }

    fn build_arrangement<S>(
        &self,
        render_context: &RenderContext,
        collection: &Collection<S, DDValue, Weight>,
    ) -> DataflowArrangement<S, Weight, TValAgent<S::Timestamp>, TKeyAgent<S::Timestamp>>
    where
        S: Scope,
        S::Timestamp: Lattice + Ord,
    {
        let kind = match *self {
            Arrangement::Map { afun, .. } => ArrangementKind::Map {
                value_function: afun,
            },
            Arrangement::Set {
                fmfun, distinct, ..
            } => {
                // TODO: We don't currently produce a `None` as the key extraction
                //       function, but doing so will simplify the dataflow graph
                //       in instances where a function isn't needed
                ArrangementKind::Set {
                    key_function: Some(fmfun),
                    distinct,
                }
            }
        };

        ArrangeBy {
            kind,
            target_relation: self.arrange_by().clone(),
        }
        .render(render_context, collection)
    }
}

/// Set relation content.
pub type ValSet = FnvHashSet<DDValue>;

/// Multiset relation content.
pub type ValMSet = DeltaSet;

/// Indexed relation content.
pub type IndexedValSet = FnvHashMap<DDValue, DDValue>;

/// Relation delta
pub type DeltaSet = FnvHashMap<DDValue, isize>;

/// Runtime representation of a datalog program.
///
/// The program will be automatically stopped when the object goes out
/// of scope. Error occurring as part of that operation are silently
/// ignored. If you want to handle such errors, call `stop` manually.
pub struct RunningProgram {
    /// Producer sides of channels used to send commands to workers.
    /// We use async channels to avoid deadlocks when workers are blocked
    /// in `step_or_park`.
    senders: Vec<Sender<Msg>>,
    /// Channels to receive replies from worker threads. We could use a single
    /// channel with multiple senders, but use many channels instead to avoid
    /// deadlocks when one of the workers has died, but `recv` blocks instead
    /// of failing, since the channel is still considered alive.
    reply_recv: Vec<Receiver<Reply>>,
    relations: FnvHashMap<RelId, RelationInstance>,
    worker_guards: Option<WorkerGuards<Result<(), String>>>,
    transaction_in_progress: bool,
    need_to_flush: bool,
    timestamp: TS,
    /// CPU profiling enabled (can be expensive).
    profile_cpu: Option<ThinArc<AtomicBool>>,
    /// Consume timely_events and output them to CSV file. Can be expensive.
    profile_timely: Option<ThinArc<AtomicBool>>,
    /// Change profiling enabled.
    profile_change: Option<ThinArc<AtomicBool>>,
    /// Profiling thread.
    prof_thread_handle: Option<JoinHandle<()>>,
    /// Profiling statistics.
    pub profile: Option<ThinArc<Mutex<Profile>>>,
    worker_round_robbin: Skip<Cycle<Range<usize>>>,
}

// Right now this Debug implementation is more or less a short cut.
// Ideally we would want to implement Debug for `RelationInstance`, but
// that quickly gets very cumbersome.
impl Debug for RunningProgram {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunningProgram")
            .field("senders", &self.senders)
            .field("reply_recv", &self.reply_recv)
            .field(
                "relations",
                &(&self.relations as *const FnvHashMap<RelId, RelationInstance>),
            )
            .field("transaction_in_progress", &self.transaction_in_progress)
            .field("need_to_flush", &self.need_to_flush)
            .field("profile_cpu", &self.profile_cpu)
            .field("profile_timely", &self.profile_timely)
            .field("profile_change", &self.profile_change)
            .field("prof_thread_handle", &self.prof_thread_handle)
            .field("profile", &self.profile)
            .finish()
    }
}

/// Runtime representation of relation
enum RelationInstance {
    Stream {
        /// Changes since start of transaction.
        delta: DeltaSet,
    },
    Multiset {
        /// Multiset of all elements in the relation.
        elements: ValMSet,
        /// Changes since start of transaction.
        delta: DeltaSet,
    },
    Flat {
        /// Set of all elements in the relation. Used to enforce set semantics for input relations
        /// (repeated inserts and deletes are ignored).
        elements: ValSet,
        /// Changes since start of transaction.
        delta: DeltaSet,
    },
    Indexed {
        key_func: fn(&DDValue) -> DDValue,
        /// Set of all elements in the relation indexed by key. Used to enforce set semantics,
        /// uniqueness of keys, and to query input relations by key.
        elements: IndexedValSet,
        /// Changes since start of transaction.  Only maintained for input relations and is used to
        /// enforce set semantics.
        delta: DeltaSet,
    },
}

impl RelationInstance {
    pub fn delta(&self) -> &DeltaSet {
        match self {
            RelationInstance::Stream { delta } => delta,
            RelationInstance::Multiset { delta, .. } => delta,
            RelationInstance::Flat { delta, .. } => delta,
            RelationInstance::Indexed { delta, .. } => delta,
        }
    }

    pub fn delta_mut(&mut self) -> &mut DeltaSet {
        match self {
            RelationInstance::Stream { delta } => delta,
            RelationInstance::Multiset { delta, .. } => delta,
            RelationInstance::Flat { delta, .. } => delta,
            RelationInstance::Indexed { delta, .. } => delta,
        }
    }
}

/// Messages sent to timely worker threads.
#[derive(Debug, Clone)]
enum Msg {
    /// Update input relation.
    Update {
        /// The batch of updates.
        updates: Vec<Update<DDValue>>,
        /// The timestamp these updates belong to.
        timestamp: TS,
    },
    /// Propagate changes through the pipeline.
    Flush {
        /// The timestamp to advance to.
        advance_to: TS,
    },
    /// Query arrangement.  If the second argument is `None`, returns
    /// all values in the collection; otherwise returns values associated
    /// with the specified key.
    Query(ArrId, Option<DDValue>),
    /// Stop worker.
    Stop,
}

/// Reply messages from timely worker threads.
#[derive(Debug)]
enum Reply {
    /// Acknowledge flush completion.
    FlushAck,
    /// Result of a query.
    QueryRes(Option<BTreeSet<DDValue>>),
}

impl Program {
    /// Initialize the program with the given configuration
    pub fn run(
        &self,
        config: Config,
        source_code: &'static DDlogSourceCode,
    ) -> Result<RunningProgram, String> {
        // Setup channels to communicate with the dataflow.
        // We use async channels to avoid deadlocks when workers are parked in
        // `step_or_park`.  This has the downside of introducing an unbounded buffer
        // that is only guaranteed to be fully flushed when the transaction commits.
        let (request_send, request_recv): (Vec<_>, Vec<_>) = (0..config.num_timely_workers)
            .map(|_| crossbeam_channel::unbounded::<Msg>())
            .unzip();
        let request_recv = Arc::from(request_recv);

        // Channels for responses from worker threads.
        let (reply_send, reply_recv): (Vec<_>, Vec<_>) = (0..config.num_timely_workers)
            .map(|_| crossbeam_channel::unbounded::<Reply>())
            .unzip();
        let reply_send = Arc::from(reply_send);

        let profiling_rig = SelfProfilingRig::new(&config, source_code);

        // Clone the program so that it can be moved into the timely computation
        let program = Arc::new(self.clone());
        let timely_config = config.timely_config()?;
        let worker_config = config.clone();
        let profiling_data = profiling_rig.profiling_data.clone();

        let (builders, others) = timely_config
            .communication
            .try_build()
            .map_err(|err| format!("failed to build timely communication config: {}", err))?;

        // Start up timely computation.
        // Note: We use `execute_from()` instead of `timely::execute()` because
        //       `execute()` automatically sets log hooks that connect to
        //       `TIMELY_WORKER_LOG_ADDR`, meaning that no matter what we do
        //       our dataflow will always attempt to connect to that address
        //       if it's present in the env, causing things like ddshow/#7.
        //       See https://github.com/Kixiron/ddshow/issues/7
        let worker_guards = timely::execute::execute_from(
            builders,
            others,
            timely_config.worker,
            move |worker: &mut Worker<Allocator>| -> Result<_, String> {
                let logger = worker.log_register().get("timely");

                let worker = DDlogWorker::new(
                    worker,
                    worker_config.clone(),
                    program.clone(),
                    profiling_data.clone(),
                    Arc::clone(&request_recv),
                    Arc::clone(&reply_send),
                    logger,
                );

                worker.run().map_err(|e| {
                    eprintln!("Worker thread failed: {}", e);
                    e
                })
            },
        )
        .map_err(|err| format!("Failed to start timely computation: {:?}", err))?;

        let mut rels = FnvHashMap::default();
        for relid in self.input_relations() {
            let rel = self.get_relation(relid);
            if rel.input {
                match rel.caching_mode {
                    CachingMode::Stream => {
                        rels.insert(
                            relid,
                            RelationInstance::Stream {
                                delta: FnvHashMap::default(),
                            },
                        );
                    }
                    CachingMode::Multiset => {
                        rels.insert(
                            relid,
                            RelationInstance::Multiset {
                                elements: FnvHashMap::default(),
                                delta: FnvHashMap::default(),
                            },
                        );
                    }
                    CachingMode::Set => match rel.key_func {
                        None => {
                            rels.insert(
                                relid,
                                RelationInstance::Flat {
                                    elements: FnvHashSet::default(),
                                    delta: FnvHashMap::default(),
                                },
                            );
                        }
                        Some(f) => {
                            rels.insert(
                                relid,
                                RelationInstance::Indexed {
                                    key_func: f,
                                    elements: FnvHashMap::default(),
                                    delta: FnvHashMap::default(),
                                },
                            );
                        }
                    },
                }
            }
        }

        let running_program = RunningProgram {
            senders: request_send,
            reply_recv,
            relations: rels,
            worker_guards: Some(worker_guards),
            transaction_in_progress: false,
            need_to_flush: false,
            timestamp: 1,
            profile_cpu: profiling_rig.profile_cpu,
            profile_timely: profiling_rig.profile_timely,
            profile_change: profiling_rig.profile_change,
            prof_thread_handle: profiling_rig.profile_thread,
            profile: profiling_rig.profile,
            worker_round_robbin: (0..config.num_timely_workers).cycle().skip(0),
        };
        // Wait for the initial transaction to complete.
        running_program.await_flush_ack()?;

        Ok(running_program)
    }

    fn prof_thread_func(channel: Receiver<ProfMsg>, profile: ThinArc<Mutex<Profile>>) {
        loop {
            match channel.recv() {
                Ok(message) => {
                    profile.lock().unwrap().update(&message);
                }
                _ => return,
            }
        }
    }

    /* Lookup relation by id */
    fn get_relation(&self, relid: RelId) -> &Relation {
        for node in &self.nodes {
            match node {
                ProgNode::Rel { rel: r } => {
                    if r.id == relid {
                        return r;
                    }
                }
                ProgNode::Apply { .. } => {}
                ProgNode::Scc { rels: rs } => {
                    for r in rs {
                        if r.rel.id == relid {
                            return &r.rel;
                        }
                    }
                }
            }
        }

        panic!("get_relation({}): relation not found", relid)
    }

    /* indices of program nodes that use arrangement */
    fn arrangement_used_by_nodes(&self, arrid: ArrId) -> impl Iterator<Item = usize> + '_ {
        self.nodes.iter().enumerate().filter_map(move |(i, n)| {
            if Self::node_uses_arrangement(n, arrid) {
                Some(i)
            } else {
                None
            }
        })
    }

    fn node_uses_arrangement(n: &ProgNode, arrid: ArrId) -> bool {
        match n {
            ProgNode::Rel { rel } => Self::rel_uses_arrangement(rel, arrid),
            ProgNode::Apply { .. } => false,
            ProgNode::Scc { rels } => rels
                .iter()
                .any(|rel| Self::rel_uses_arrangement(&rel.rel, arrid)),
        }
    }

    fn rel_uses_arrangement(r: &Relation, arrid: ArrId) -> bool {
        r.rules
            .iter()
            .any(|rule| Self::rule_uses_arrangement(rule, arrid))
    }

    fn rule_uses_arrangement(r: &Rule, arrid: ArrId) -> bool {
        r.dependencies().contains(&Dep::Arr(arrid))
    }

    /// Returns all input relations of the program
    fn input_relations(&self) -> impl Iterator<Item = RelId> + '_ {
        self.nodes.iter().filter_map(|node| match node {
            ProgNode::Rel { rel: r } => {
                if r.input {
                    Some(r.id)
                } else {
                    None
                }
            }
            ProgNode::Apply { .. } => None,
            ProgNode::Scc { rels: rs } => {
                for r in rs {
                    assert!(!r.rel.input, "input relation ({}) in Scc", r.rel.name);
                }

                None
            }
        })
    }

    /// Return all relations required to compute rels, excluding recursive dependencies on rels
    fn dependencies<'a, R>(rels: R) -> FnvHashSet<Dep>
    where
        R: Iterator<Item = &'a Relation> + Clone + 'a,
    {
        let mut result = FnvHashSet::default();
        for rel in rels.clone() {
            for rule in &rel.rules {
                result = result.union(&rule.dependencies()).cloned().collect();
            }
        }

        result
            .into_iter()
            .filter(|d| rels.clone().all(|r| r.id != d.relid()))
            .collect()
    }

    /// TODO: Allow this to return an error, so we can replace `expect`'s below
    /// with proper error handling.
    // TODO: Much of this logic would be vastly simplified if we used a
    //       combination of traits and `Vec<XFormCollection>`s (as opposed to
    //       what we do now with a linked list of them)
    fn xform_collection<'a, S, T, Lookup>(
        col: Collection<S, DDValue, Weight>,
        xform: &Option<XFormCollection>,
        arrangements: &Arrangements<'a, S, T>,
        lookup_collection: Lookup,
    ) -> Collection<S, DDValue, Weight>
    where
        S: Scope,
        S::Timestamp: Lattice + Refines<T> + ToTupleTS,
        T: Lattice + Timestamp,
        Lookup: Fn(RelId) -> Option<Collection<S, DDValue, Weight>>,
    {
        match xform {
            None => col,
            Some(ref x) => Self::xform_collection_ref(&col, x, arrangements, lookup_collection),
        }
    }

    fn xform_collection_ref<'a, S, T, Lookup>(
        col: &Collection<S, DDValue, Weight>,
        xform: &XFormCollection,
        arrangements: &Arrangements<'a, S, T>,
        lookup_collection: Lookup,
    ) -> Collection<S, DDValue, Weight>
    where
        S: Scope,
        S::Timestamp: Lattice + Refines<T> + ToTupleTS,
        T: Lattice + Timestamp,
        Lookup: Fn(RelId) -> Option<Collection<S, DDValue, Weight>>,
    {
        match *xform {
            XFormCollection::Arrange {
                ref debug_info,
                afun,
                ref next,
            } => {
                let arr =
                    with_prof_context(debug_info.clone(), || col.flat_map(afun).arrange_by_key());
                Self::xform_arrangement(&arr, &*next, arrangements, lookup_collection)
            }
            XFormCollection::Differentiate {
                ref debug_info,
                ref next,
            } => {
                #[allow(clippy::unnecessary_cast)]
                let one =
                    <dyn Any>::downcast_ref::<<S::Timestamp as Timestamp>::Summary>(&(1 as TS))
                        .expect("Differentiate operator used in recursive context");

                let diff = with_prof_context(debug_info.clone(), || {
                    col.concat(
                        &col.delay(move |t| one.results_in(t).expect("Integer overflow in Differentiate: maximal number of transactions exceeded")).negate())
                });

                Self::xform_collection(diff, &*next, arrangements, lookup_collection)
            }
            XFormCollection::Map {
                ref debug_info,
                mfun,
                ref next,
            } => {
                let mapped = with_prof_context(debug_info.clone(), || col.map(mfun));
                Self::xform_collection(mapped, &*next, arrangements, lookup_collection)
            }
            XFormCollection::FlatMap {
                ref debug_info,
                fmfun,
                ref next,
            } => {
                let flattened = with_prof_context(debug_info.clone(), || {
                    col.flat_map(move |x| fmfun(x).into_iter().flatten())
                });
                Self::xform_collection(flattened, &*next, arrangements, lookup_collection)
            }
            XFormCollection::Filter {
                ref debug_info,
                ffun,
                ref next,
            } => {
                let filtered = with_prof_context(debug_info.clone(), || col.filter(ffun));
                Self::xform_collection(filtered, &*next, arrangements, lookup_collection)
            }
            XFormCollection::FilterMap {
                ref debug_info,
                fmfun,
                ref next,
            } => {
                let flattened = with_prof_context(debug_info.clone(), || col.flat_map(fmfun));
                Self::xform_collection(flattened, &*next, arrangements, lookup_collection)
            }
            XFormCollection::Inspect {
                ref debug_info,
                ifun,
                ref next,
            } => {
                let inspect = with_prof_context(debug_info.clone(), || {
                    col.inspect(move |(v, ts, w)| ifun(v, ts.to_tuple_ts(), *w))
                });
                Self::xform_collection(inspect, &*next, arrangements, lookup_collection)
            }
            XFormCollection::StreamJoin {
                ref debug_info,
                afun,
                arrangement,
                jfun,
                ref next,
            } => {
                let join = with_prof_context(debug_info.clone(), || {
                    // arrange input collection
                    let collection_with_keys = col.flat_map(afun);
                    let arr = match arrangements.lookup_arr(arrangement) {
                        ArrangementFlavor::Local(DataflowArrangement::Map(arranged)) => arranged,
                        ArrangementFlavor::Local(DataflowArrangement::Set(_)) => {
                            panic!("StreamJoin: not a map arrangement {:?}", arrangement)
                        }
                        _ => panic!("StreamJoin in nested scope: {:?}", debug_info),
                    };
                    lookup_map(
                        &collection_with_keys,
                        arr,
                        |(k, _), key| *key = k.clone(),
                        move |v1, &w1, v2, &w2| (jfun(&v1.1, v2), w1 * w2),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                    )
                    // Filter out `None`'s.
                    // FIXME: We wouldn't need this if `lookup_map` allowed `output_func`
                    // to return `Option`.
                    .flat_map(|v| v)
                });
                Self::xform_collection(join, &*next, arrangements, lookup_collection)
            }
            XFormCollection::StreamSemijoin {
                ref debug_info,
                afun,
                arrangement,
                jfun,
                ref next,
            } => {
                let join = with_prof_context(debug_info.clone(), || {
                    // arrange input collection
                    let collection_with_keys = col.flat_map(afun);
                    let arr = match arrangements.lookup_arr(arrangement) {
                        ArrangementFlavor::Local(DataflowArrangement::Set(arranged)) => arranged,
                        ArrangementFlavor::Local(DataflowArrangement::Map(_)) => {
                            panic!("StreamSemijoin: not a set arrangement {:?}", arrangement)
                        }
                        _ => panic!("StreamSemijoin in nested scope: {:?}", debug_info),
                    };
                    lookup_map(
                        &collection_with_keys,
                        arr,
                        |(k, _), key| *key = k.clone(),
                        move |v1, &w1, _, &w2| (jfun(&v1.1), w1 * w2),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                    )
                    // Filter out `None`'s.
                    // FIXME: We wouldn't need this if `lookup_map` allowed `output_func`
                    // to return `Option`.
                    .flat_map(|v| v)
                });
                Self::xform_collection(join, &*next, arrangements, lookup_collection)
            }

            XFormCollection::StreamXForm {
                ref debug_info,
                ref xform,
                ref next,
            } => {
                let xformed = with_prof_context(debug_info.clone(), || {
                    col.scope().scoped::<AltNeu<S::Timestamp>, _, _>(
                        "Streaming transformation",
                        |inner| {
                            let d_col = col.differentiate(inner);

                            fn dummy_lookup_collection<S: Scope>(
                                _: RelId,
                            ) -> Option<Collection<S, DDValue, Weight>>
                            {
                                None
                            }

                            // We must call the streamless variant within the nested scope
                            // otherwise we force rustc to instantiate an infinitely long type
                            // since the function calls itself (a potentially infinite number of times),
                            // each requiring further nesting of the scopes (and their types)
                            let xformed = Self::streamless_xform_collection::<
                                Child<S, AltNeu<S::Timestamp>>,
                                S::Timestamp,
                                _,
                            >(
                                d_col,
                                &*xform,
                                &Arrangements {
                                    arrangements: &FnvHashMap::default(),
                                },
                                dummy_lookup_collection,
                            );

                            xformed.integrate()
                        },
                    )
                });

                Self::xform_collection(xformed, &*next, arrangements, lookup_collection)
            }
        }
    }

    fn streamless_xform_collection<'a, S, T, Lookup>(
        col: Collection<S, DDValue, Weight>,
        xform: &Option<XFormCollection>,
        arrangements: &Arrangements<'a, S, T>,
        lookup_collection: Lookup,
    ) -> Collection<S, DDValue, Weight>
    where
        S: Scope,
        S::Timestamp: Lattice + Refines<T> + ToTupleTS,
        T: Lattice + Timestamp,
        Lookup: Fn(RelId) -> Option<Collection<S, DDValue, Weight>>,
    {
        match xform {
            None => col,
            Some(ref x) => {
                Self::streamless_xform_collection_ref(&col, x, arrangements, lookup_collection)
            }
        }
    }

    fn streamless_xform_collection_ref<'a, S, T, Lookup>(
        col: &Collection<S, DDValue, Weight>,
        xform: &XFormCollection,
        arrangements: &Arrangements<'a, S, T>,
        lookup_collection: Lookup,
    ) -> Collection<S, DDValue, Weight>
    where
        S: Scope,
        S::Timestamp: Lattice + Refines<T> + ToTupleTS,
        T: Lattice + Timestamp,
        Lookup: Fn(RelId) -> Option<Collection<S, DDValue, Weight>>,
    {
        match *xform {
            XFormCollection::Arrange {
                ref debug_info,
                afun,
                ref next,
            } => {
                let arr =
                    with_prof_context(debug_info.clone(), || col.flat_map(afun).arrange_by_key());
                Self::xform_arrangement(&arr, &*next, arrangements, lookup_collection)
            }
            XFormCollection::Differentiate {
                ref debug_info,
                ref next,
            } => {
                #[allow(clippy::unnecessary_cast)]
                let one =
                    <dyn Any>::downcast_ref::<<S::Timestamp as Timestamp>::Summary>(&(1 as TS))
                        .expect("Differentiate operator used in recursive context");

                let diff = with_prof_context(debug_info.clone(), || {
                    col.concat(
                        &col.delay(move |t| one.results_in(t).expect("Integer overflow in Differentiate: maximal number of transactions exceeded")).negate())
                });

                Self::streamless_xform_collection(diff, &*next, arrangements, lookup_collection)
            }
            XFormCollection::Map {
                ref debug_info,
                mfun,
                ref next,
            } => {
                let mapped = with_prof_context(debug_info.clone(), || col.map(mfun));
                Self::streamless_xform_collection(mapped, &*next, arrangements, lookup_collection)
            }
            XFormCollection::FlatMap {
                ref debug_info,
                fmfun,
                ref next,
            } => {
                let flattened = with_prof_context(debug_info.clone(), || {
                    col.flat_map(move |x| fmfun(x).into_iter().flatten())
                });
                Self::streamless_xform_collection(
                    flattened,
                    &*next,
                    arrangements,
                    lookup_collection,
                )
            }
            XFormCollection::Filter {
                ref debug_info,
                ffun,
                ref next,
            } => {
                let filtered = with_prof_context(debug_info.clone(), || col.filter(ffun));
                Self::streamless_xform_collection(filtered, &*next, arrangements, lookup_collection)
            }
            XFormCollection::FilterMap {
                ref debug_info,
                fmfun,
                ref next,
            } => {
                let flattened = with_prof_context(debug_info.clone(), || col.flat_map(fmfun));
                Self::streamless_xform_collection(
                    flattened,
                    &*next,
                    arrangements,
                    lookup_collection,
                )
            }
            XFormCollection::Inspect {
                ref debug_info,
                ifun,
                ref next,
            } => {
                let inspect = with_prof_context(debug_info.clone(), || {
                    col.inspect(move |(v, ts, w)| ifun(v, ts.to_tuple_ts(), *w))
                });
                Self::streamless_xform_collection(inspect, &*next, arrangements, lookup_collection)
            }
            XFormCollection::StreamJoin {
                ref debug_info,
                afun,
                arrangement,
                jfun,
                ref next,
            } => {
                let join = with_prof_context(debug_info.clone(), || {
                    // arrange input collection
                    let collection_with_keys = col.flat_map(afun);
                    let arr = match arrangements.lookup_arr(arrangement) {
                        ArrangementFlavor::Local(DataflowArrangement::Map(arranged)) => arranged,
                        ArrangementFlavor::Local(DataflowArrangement::Set(_)) => {
                            panic!("StreamJoin: not a map arrangement {:?}", arrangement)
                        }
                        _ => panic!("StreamJoin in nested scope: {:?}", debug_info),
                    };
                    lookup_map(
                        &collection_with_keys,
                        arr,
                        |(k, _), key| *key = k.clone(),
                        move |v1, &w1, v2, &w2| (jfun(&v1.1, v2), w1 * w2),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                    )
                    // Filter out `None`'s.
                    // FIXME: We wouldn't need this if `lookup_map` allowed `output_func`
                    // to return `Option`.
                    .flat_map(|v| v)
                });
                Self::streamless_xform_collection(join, &*next, arrangements, lookup_collection)
            }
            XFormCollection::StreamSemijoin {
                ref debug_info,
                afun,
                arrangement,
                jfun,
                ref next,
            } => {
                let join = with_prof_context(debug_info.clone(), || {
                    // arrange input collection
                    let collection_with_keys = col.flat_map(afun);
                    let arr = match arrangements.lookup_arr(arrangement) {
                        ArrangementFlavor::Local(DataflowArrangement::Set(arranged)) => arranged,
                        ArrangementFlavor::Local(DataflowArrangement::Map(_)) => {
                            panic!("StreamSemijoin: not a set arrangement {:?}", arrangement)
                        }
                        _ => panic!("StreamSemijoin in nested scope: {:?}", debug_info),
                    };
                    lookup_map(
                        &collection_with_keys,
                        arr,
                        |(k, _), key| *key = k.clone(),
                        move |v1, &w1, _, &w2| (jfun(&v1.1), w1 * w2),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                        ().into_ddvalue(),
                    )
                    // Filter out `None`'s.
                    // FIXME: We wouldn't need this if `lookup_map` allowed `output_func`
                    // to return `Option`.
                    .flat_map(|v| v)
                });
                Self::streamless_xform_collection(join, &*next, arrangements, lookup_collection)
            }

            XFormCollection::StreamXForm { ref debug_info, .. } => {
                panic!("StreamXForm in nested scope: {:?}", debug_info);
            }
        }
    }

    fn xform_arrangement<'a, S, T, TR, LC>(
        arr: &Arranged<S, TR>,
        xform: &XFormArrangement,
        arrangements: &Arrangements<'a, S, T>,
        lookup_collection: LC,
    ) -> Collection<S, DDValue, Weight>
    where
        S: Scope,
        S::Timestamp: Lattice + Refines<T> + ToTupleTS,
        T: Lattice + Timestamp,
        TR: TraceReader<Key = DDValue, Val = DDValue, Time = S::Timestamp, R = Weight>
            + Clone
            + 'static,
        TR::Batch: BatchReader<DDValue, DDValue, S::Timestamp, Weight>,
        TR::Cursor: Cursor<DDValue, DDValue, S::Timestamp, Weight>,
        LC: Fn(RelId) -> Option<Collection<S, DDValue, Weight>>,
    {
        match *xform {
            XFormArrangement::FlatMap {
                ref debug_info,
                fmfun,
                ref next,
            } => with_prof_context(debug_info.clone(), || {
                Self::streamless_xform_collection(
                    arr.flat_map_ref(move |_, v| match fmfun(v.clone()) {
                        Some(iter) => iter,
                        None => Box::new(None.into_iter()),
                    }),
                    &*next,
                    arrangements,
                    lookup_collection,
                )
            }),
            XFormArrangement::FilterMap {
                ref debug_info,
                fmfun,
                ref next,
            } => with_prof_context(debug_info.clone(), || {
                Self::streamless_xform_collection(
                    arr.flat_map_ref(move |_, v| fmfun(v.clone())),
                    &*next,
                    arrangements,
                    lookup_collection,
                )
            }),
            XFormArrangement::Aggregate {
                ref debug_info,
                ffun,
                aggfun,
                ref next,
            } => {
                let col = with_prof_context(debug_info.clone(), || {
                    ffun.map_or_else(
                        || {
                            arr.reduce(move |key, src, dst| {
                                if let Some(x) = aggfun(key, src) {
                                    dst.push((x, Weight::one()));
                                };
                            })
                            .map(|(_, v)| v)
                        },
                        |f| {
                            arr.filter(move |_, v| f(v))
                                .reduce(move |key, src, dst| {
                                    if let Some(x) = aggfun(key, src) {
                                        dst.push((x, Weight::one()));
                                    };
                                })
                                .map(|(_, v)| v)
                        },
                    )
                });
                Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
            }
            XFormArrangement::Join {
                ref debug_info,
                ffun,
                arrangement,
                jfun,
                ref next,
            } => match arrangements.lookup_arr(arrangement) {
                ArrangementFlavor::Local(DataflowArrangement::Map(arranged)) => {
                    let col = with_prof_context(debug_info.clone(), || {
                        ffun.map_or_else(
                            || arr.join_core(&arranged, jfun),
                            |f| arr.filter(move |_, v| f(v)).join_core(&arranged, jfun),
                        )
                    });
                    Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
                }
                ArrangementFlavor::Foreign(DataflowArrangement::Map(arranged)) => {
                    let col = with_prof_context(debug_info.clone(), || {
                        ffun.map_or_else(
                            || arr.join_core(&arranged, jfun),
                            |f| arr.filter(move |_, v| f(v)).join_core(&arranged, jfun),
                        )
                    });
                    Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
                }

                _ => panic!("Join: not a map arrangement {:?}", arrangement),
            },
            XFormArrangement::Semijoin {
                ref debug_info,
                ffun,
                arrangement,
                jfun,
                ref next,
            } => match arrangements.lookup_arr(arrangement) {
                ArrangementFlavor::Local(DataflowArrangement::Set(arranged)) => {
                    let col = with_prof_context(debug_info.clone(), || {
                        ffun.map_or_else(
                            || arr.join_core(&arranged, jfun),
                            |f| arr.filter(move |_, v| f(v)).join_core(&arranged, jfun),
                        )
                    });
                    Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
                }
                ArrangementFlavor::Foreign(DataflowArrangement::Set(arranged)) => {
                    let col = with_prof_context(debug_info.clone(), || {
                        ffun.map_or_else(
                            || arr.join_core(&arranged, jfun),
                            |f| arr.filter(move |_, v| f(v)).join_core(&arranged, jfun),
                        )
                    });
                    Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
                }
                _ => panic!("Semijoin: not a set arrangement {:?}", arrangement),
            },
            XFormArrangement::Antijoin {
                ref debug_info,
                ffun,
                arrangement,
                ref next,
            } => match arrangements.lookup_arr(arrangement) {
                ArrangementFlavor::Local(DataflowArrangement::Set(arranged)) => {
                    let col = with_prof_context(debug_info.clone(), || {
                        ffun.map_or_else(
                            || antijoin_arranged(arr, &arranged).map(|(_, v)| v),
                            |f| {
                                antijoin_arranged(&arr.filter(move |_, v| f(v)), &arranged)
                                    .map(|(_, v)| v)
                            },
                        )
                    });
                    Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
                }
                ArrangementFlavor::Foreign(DataflowArrangement::Set(arranged)) => {
                    let col = with_prof_context(debug_info.clone(), || {
                        ffun.map_or_else(
                            || antijoin_arranged(arr, &arranged).map(|(_, v)| v),
                            |f| {
                                antijoin_arranged(&arr.filter(move |_, v| f(v)), &arranged)
                                    .map(|(_, v)| v)
                            },
                        )
                    });
                    Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
                }
                _ => panic!("Antijoin: not a set arrangement {:?}", arrangement),
            },
            XFormArrangement::StreamJoin {
                ref debug_info,
                ffun,
                rel,
                kfun,
                jfun,
                ref next,
            } => {
                let col = with_prof_context(debug_info.clone(), || {
                    // Map `rel` into `(key, value)` pairs, filtering out
                    // records where `kfun` returns `None`.
                    // FIXME: The key will need to be cloned below.  To avoid
                    // this overhead, we need a version of `lookup_map` that
                    // allows key function to return `Option`.
                    let kfun = kfun;
                    let jfun = jfun;
                    let collection_with_keys = lookup_collection(rel)
                        .unwrap_or_else(|| panic!("xform_arrangement: unknown relation {:?}", rel))
                        .flat_map(move |v| kfun(&v).map(|k| (k, v)));
                    // Filter the arrangement if `ffun` is supplied.
                    let join = ffun.map_or_else(
                        || {
                            lookup_map(
                                &collection_with_keys,
                                arr.clone(),
                                |(k, _), key| *key = k.clone(),
                                move |v1, &w1, v2, &w2| (jfun(v2, &v1.1), w1 * w2),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                            )
                        },
                        |f| {
                            lookup_map(
                                &collection_with_keys,
                                arr.filter(move |_, v| f(v)),
                                |(k, _), key| *key = k.clone(),
                                move |v1, &w1, v2, &w2| (jfun(v2, &v1.1), w1 * w2),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                            )
                        },
                    );

                    // Filter out `None`'s.
                    // FIXME: We wouldn't need this if `lookup_map` allowed `output_func`
                    // to return `Option`.
                    join.flat_map(|v| v)
                });
                Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
            }
            XFormArrangement::StreamSemijoin {
                ref debug_info,
                ffun,
                rel,
                kfun,
                jfun,
                ref next,
            } => {
                let col = with_prof_context(debug_info.clone(), || {
                    // Extract join key from `rel`, filtering out
                    // FIXME: The key will need to be cloned below.  To avoid
                    // this overhead, we need a version of `lookup_map` that
                    // allows key function to return `Option`.
                    let kfun = kfun;
                    let jfun = jfun;
                    let collection_keys = lookup_collection(rel)
                        .unwrap_or_else(|| panic!("xform_arrangement: unknown relation {:?}", rel))
                        .flat_map(move |v| kfun(&v));
                    // Filter the arrangement if `ffun` is supplied.
                    let join = ffun.map_or_else(
                        || {
                            lookup_map(
                                &collection_keys,
                                arr.clone(),
                                |k, key| *key = k.clone(),
                                move |_, &w1, v2, &w2| (jfun(v2), w1 * w2),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                            )
                        },
                        |f| {
                            lookup_map(
                                &collection_keys,
                                arr.filter(move |_, v| f(v)),
                                |k, key| *key = k.clone(),
                                move |_, &w1, v2, &w2| (jfun(v2), w1 * w2),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                                ().into_ddvalue(),
                            )
                        },
                    );

                    // Filter out `None`'s.
                    // FIXME: We wouldn't need this if `lookup_map` allowed `output_func`
                    // to return `Option`.
                    join.flat_map(|v| v)
                });
                Self::streamless_xform_collection(col, &*next, arrangements, lookup_collection)
            }
        }
    }

    /// Compile right-hand-side of a rule to a collection
    fn mk_rule<'a, S, T, F>(
        &self,
        rule: &Rule,
        lookup_collection: F,
        arrangements: Arrangements<'a, S, T>,
    ) -> Collection<S, DDValue, Weight>
    where
        S: Scope,
        S::Timestamp: Lattice + Refines<T> + ToTupleTS,
        T: Lattice + Timestamp,
        F: Fn(RelId) -> Option<Collection<S, DDValue, Weight>>,
    {
        match rule {
            Rule::CollectionRule {
                rel, xform: None, ..
            } => lookup_collection(*rel)
                .unwrap_or_else(|| panic!("mk_rule: unknown relation {:?}", rel)),
            Rule::CollectionRule {
                rel,
                xform: Some(x),
                ..
            } => Self::xform_collection_ref(
                &lookup_collection(*rel)
                    .unwrap_or_else(|| panic!("mk_rule: unknown relation {:?}", rel)),
                x,
                &arrangements,
                &lookup_collection,
            ),
            Rule::ArrangementRule { arr, xform, .. } => match arrangements.lookup_arr(*arr) {
                ArrangementFlavor::Local(DataflowArrangement::Map(arranged)) => {
                    Self::xform_arrangement(&arranged, xform, &arrangements, &lookup_collection)
                }
                ArrangementFlavor::Foreign(DataflowArrangement::Map(arranged)) => {
                    Self::xform_arrangement(&arranged, xform, &arrangements, &lookup_collection)
                }
                _ => panic!("Rule starts with a set arrangement {:?}", *arr),
            },
        }
    }
}

/// Interface to a running datalog computation
// This should not panic, so that the client has a chance to recover from failures
// TODO: error messages
impl RunningProgram {
    /// Controls forwarding of `TimelyEvent::Schedule` event to the CPU profiling thread.
    ///
    /// `enable = true`  - enables forwarding. This can be expensive in large dataflows.
    /// `enable = false` - disables forwarding.
    pub fn enable_cpu_profiling(&self, enable: bool) {
        if let Some(profile_cpu) = self.profile_cpu.as_ref() {
            profile_cpu.store(enable, Ordering::SeqCst);
        }
        // TODO: Log warning if self profiling is disabled
    }

    pub fn enable_timely_profiling(&self, enable: bool) {
        if let Some(profile_timely) = self.profile_timely.as_ref() {
            profile_timely.store(enable, Ordering::SeqCst);
        }
        // TODO: Log warning if self profiling is disabled
    }

    pub fn enable_change_profiling(&self, enable: bool) {
        if let Some(profile_change) = self.profile_change.as_ref() {
            profile_change.store(enable, Ordering::SeqCst);
        }
        // TODO: Log warning if self profiling is disabled
    }

    /// Terminate program, killing all worker threads.
    pub fn stop(&mut self) -> Response<()> {
        if self.worker_guards.is_none() {
            // Already stopped.
            return Ok(());
        }

        self.flush()
            .and_then(|_| self.broadcast(Msg::Stop))
            .and_then(|_| {
                self.worker_guards.take().map_or(Ok(()), |worker_guards| {
                    worker_guards
                        .join()
                        .into_iter()
                        .filter_map(Result::err)
                        .next()
                        .map_or(Ok(()), Err)
                })
            })?;

        Ok(())
    }

    /// Start a transaction. Does not return a transaction handle, as there
    /// can be at most one transaction in progress at any given time. Fails
    /// if there is already a transaction in progress.
    pub fn transaction_start(&mut self) -> Response<()> {
        if self.transaction_in_progress {
            return Err("transaction already in progress".to_string());
        }

        self.transaction_in_progress = true;
        Ok(())
    }

    /// Commit a transaction.
    pub fn transaction_commit(&mut self) -> Response<()> {
        if !self.transaction_in_progress {
            return Err("transaction_commit: no transaction in progress".to_string());
        }

        self.flush()?;
        self.delta_cleanup();
        self.transaction_in_progress = false;
        Ok(())
    }

    /// Rollback the transaction, undoing all changes.
    pub fn transaction_rollback(&mut self) -> Response<()> {
        if !self.transaction_in_progress {
            return Err("transaction_rollback: no transaction in progress".to_string());
        }

        self.flush().and_then(|_| self.delta_undo()).map(|_| {
            self.transaction_in_progress = false;
        })
    }

    /// Insert one record into input relation. Relations have set semantics, i.e.,
    /// adding an existing record is a no-op.
    pub fn insert(&mut self, relid: RelId, v: DDValue) -> Response<()> {
        self.apply_updates(iter::once(Update::Insert { relid, v }), |_| Ok(()))
    }

    /// Insert one record into input relation or replace existing record with the same key.
    pub fn insert_or_update(&mut self, relid: RelId, v: DDValue) -> Response<()> {
        self.apply_updates(iter::once(Update::InsertOrUpdate { relid, v }), |_| Ok(()))
    }

    /// Remove a record if it exists in the relation.
    pub fn delete_value(&mut self, relid: RelId, v: DDValue) -> Response<()> {
        self.apply_updates(iter::once(Update::DeleteValue { relid, v }), |_| Ok(()))
    }

    /// Remove a key if it exists in the relation.
    pub fn delete_key(&mut self, relid: RelId, k: DDValue) -> Response<()> {
        self.apply_updates(iter::once(Update::DeleteKey { relid, k }), |_| Ok(()))
    }

    /// Modify a key if it exists in the relation.
    pub fn modify_key(
        &mut self,
        relid: RelId,
        k: DDValue,
        m: Arc<dyn Mutator<DDValue> + Send + Sync>,
    ) -> Response<()> {
        self.apply_updates(iter::once(Update::Modify { relid, k, m }), |_| Ok(()))
    }

    /// Applies a single update.
    fn apply_update(
        &mut self,
        update: Update<DDValue>,
        filtered_updates: &mut Vec<Update<DDValue>>,
    ) -> Response<()> {
        let rel = self
            .relations
            .get_mut(&update.relid())
            .ok_or_else(|| format!("apply_update: unknown input relation {}", update.relid()))?;

        match rel {
            RelationInstance::Stream { delta } => {
                Self::stream_update(delta, update, filtered_updates)
            }
            RelationInstance::Multiset { elements, delta } => {
                Self::mset_update(elements, delta, update, filtered_updates)
            }
            RelationInstance::Flat { elements, delta } => {
                Self::set_update(elements, delta, update, filtered_updates)
            }
            RelationInstance::Indexed {
                key_func,
                elements,
                delta,
            } => Self::indexed_set_update(*key_func, elements, delta, update, filtered_updates),
        }
    }

    /// Apply multiple insert and delete operations in one batch.
    /// Updates can only be applied to input relations (see `struct Relation`).
    pub fn apply_updates<I, F>(&mut self, updates: I, inspect: F) -> Response<()>
    where
        I: Iterator<Item = Update<DDValue>>,
        F: Fn(&Update<DDValue>) -> Response<()>,
    {
        if !self.transaction_in_progress {
            return Err("apply_updates: no transaction in progress".to_string());
        }

        // Remove no-op updates to maintain set semantics
        let mut filtered_updates = Vec::new();
        for update in updates {
            inspect(&update)?;
            self.apply_update(update, &mut filtered_updates)?;
        }

        if filtered_updates.is_empty() {
            return Ok(());
        }

        let mut worker_round_robbin = self.worker_round_robbin.clone();

        let chunk_size = cmp::max(filtered_updates.len() / self.senders.len(), 5000);
        filtered_updates
            .chunks(chunk_size)
            .map(|chunk| Msg::Update {
                updates: chunk.to_vec(),
                timestamp: self.timestamp,
            })
            .zip(&mut worker_round_robbin)
            .try_for_each(|(update, worker_idx)| self.send(worker_idx, update))?;

        let next = worker_round_robbin.next().unwrap_or(0);
        self.worker_round_robbin = (0..self.senders.len()).cycle().skip(next);

        self.need_to_flush = true;
        Ok(())
    }

    /// Deletes all values in an input table
    pub fn clear_relation(&mut self, relid: RelId) -> Response<()> {
        if !self.transaction_in_progress {
            return Err("clear_relation: no transaction in progress".to_string());
        }

        let updates = {
            let rel = self
                .relations
                .get_mut(&relid)
                .ok_or_else(|| format!("clear_relation: unknown input relation {}", relid))?;

            match rel {
                RelationInstance::Stream { .. } => {
                    return Err("clear_relation: operation not supported for streams".to_string())
                }
                RelationInstance::Multiset { elements, .. } => {
                    let mut updates: Vec<Update<DDValue>> = Vec::with_capacity(elements.len());
                    Self::delta_undo_updates(relid, elements, &mut updates);

                    updates
                }
                RelationInstance::Flat { elements, .. } => {
                    let mut updates: Vec<Update<DDValue>> = Vec::with_capacity(elements.len());
                    for v in elements.iter() {
                        updates.push(Update::DeleteValue {
                            relid,
                            v: v.clone(),
                        });
                    }

                    updates
                }
                RelationInstance::Indexed { elements, .. } => {
                    let mut updates: Vec<Update<DDValue>> = Vec::with_capacity(elements.len());
                    for k in elements.keys() {
                        updates.push(Update::DeleteKey {
                            relid,
                            k: k.clone(),
                        });
                    }

                    updates
                }
            }
        };

        self.apply_updates(updates.into_iter(), |_| Ok(()))
    }

    /// Returns all values in the arrangement with the specified key.
    pub fn query_arrangement(&mut self, arrid: ArrId, k: DDValue) -> Response<BTreeSet<DDValue>> {
        self._query_arrangement(arrid, Some(k))
    }

    /// Returns the entire content of an arrangement.
    pub fn dump_arrangement(&mut self, arrid: ArrId) -> Response<BTreeSet<DDValue>> {
        self._query_arrangement(arrid, None)
    }

    fn _query_arrangement(
        &mut self,
        arrid: ArrId,
        k: Option<DDValue>,
    ) -> Response<BTreeSet<DDValue>> {
        // Send query and receive replies from all workers. If a key is specified, then at most
        // one worker will send a non-empty reply.
        self.broadcast(Msg::Query(arrid, k))?;

        let mut res: BTreeSet<DDValue> = BTreeSet::new();
        let mut unknown = false;
        for (worker_index, chan) in self.reply_recv.iter().enumerate() {
            let reply = chan.recv().map_err(|e| {
                format!(
                    "query_arrangement: failed to receive reply from worker {}: {:?}",
                    worker_index, e
                )
            })?;

            match reply {
                Reply::QueryRes(Some(mut vals)) => {
                    if !vals.is_empty() {
                        if res.is_empty() {
                            std::mem::swap(&mut res, &mut vals);
                        } else {
                            res.append(&mut vals);
                        }
                    }
                }
                Reply::QueryRes(None) => {
                    unknown = true;
                }
                repl => {
                    return Err(format!(
                        "query_arrangement: unexpected reply from worker {}: {:?}",
                        worker_index, repl
                    ));
                }
            }
        }

        if unknown {
            Err(format!("query_arrangement: unknown index: {:?}", arrid))
        } else {
            Ok(res)
        }
    }

    /// increment the counter associated with value `x` in the delta-set
    /// `delta(x) == false` => remove entry (equivalent to delta(x):=0)
    /// `x not in delta => `delta(x) := true`
    /// `delta(x) == true` => error
    fn delta_inc(ds: &mut DeltaSet, x: &DDValue) {
        let entry = ds.entry(x.clone());
        match entry {
            hash_map::Entry::Occupied(mut oe) => {
                // debug_assert!(!*oe.get());
                let v = oe.get_mut();
                if *v == -1 {
                    oe.remove_entry();
                } else {
                    *v += 1;
                }
            }
            hash_map::Entry::Vacant(ve) => {
                ve.insert(1);
            }
        }
    }

    /// reverse of delta_inc
    fn delta_dec(ds: &mut DeltaSet, key: &DDValue) {
        let entry = ds.entry(key.clone());
        match entry {
            hash_map::Entry::Occupied(mut oe) => {
                //debug_assert!(*oe.get());
                let v = oe.get_mut();
                if *v == 1 {
                    oe.remove_entry();
                } else {
                    *v -= 1;
                }
            }
            hash_map::Entry::Vacant(ve) => {
                ve.insert(-1);
            }
        }
    }

    /// Update delta set of an input stream relation before performing an update.
    /// `ds` is delta since start of transaction.
    /// `x` is the value being inserted or deleted.
    /// `insert` indicates type of update (`true` for insert, `false` for delete)
    fn stream_update(
        ds: &mut DeltaSet,
        update: Update<DDValue>,
        updates: &mut Vec<Update<DDValue>>,
    ) -> Response<()> {
        match &update {
            Update::Insert { v, .. } => {
                Self::delta_inc(ds, v);
            }
            Update::DeleteValue { v, .. } => {
                Self::delta_dec(ds, v);
            }
            Update::InsertOrUpdate { relid, .. } => {
                return Err(format!(
                    "Cannot perform insert_or_update operation on relation {} that does not have a primary key",
                    relid,
                ));
            }
            Update::DeleteKey { relid, .. } => {
                return Err(format!(
                    "Cannot delete by key from relation {} that does not have a primary key",
                    relid,
                ));
            }
            Update::Modify { relid, .. } => {
                return Err(format!(
                    "Cannot modify record in relation {} that does not have a primary key",
                    relid,
                ));
            }
        };
        updates.push(update);

        Ok(())
    }

    /// Update value and delta multisets of an input multiset relation before performing an update.
    /// `s` is the current content of the relation.
    /// `ds` is delta since start of transaction.
    /// `x` is the value being inserted or deleted.
    /// `insert` indicates type of update (`true` for insert, `false` for delete).
    /// Returns `true` if the update modifies the relation, i.e., it's not a no-op.
    fn mset_update(
        s: &mut ValMSet,
        ds: &mut DeltaSet,
        upd: Update<DDValue>,
        updates: &mut Vec<Update<DDValue>>,
    ) -> Response<()> {
        match &upd {
            Update::Insert { v, .. } => {
                Self::delta_inc(s, v);
                Self::delta_inc(ds, v);
            }
            Update::DeleteValue { v, .. } => {
                Self::delta_dec(s, v);
                Self::delta_dec(ds, v);
            }
            Update::InsertOrUpdate { relid, .. } => {
                return Err(format!(
                    "Cannot perform insert_or_update operation on relation {} that does not have a primary key",
                    relid
                ));
            }
            Update::DeleteKey { relid, .. } => {
                return Err(format!(
                    "Cannot delete by key from relation {} that does not have a primary key",
                    relid
                ));
            }
            Update::Modify { relid, .. } => {
                return Err(format!(
                    "Cannot modify record in relation {} that does not have a primary key",
                    relid
                ));
            }
        };
        updates.push(upd);

        Ok(())
    }

    /// Update value set and delta set of an input relation before performing an update.
    /// `s` is the current content of the relation.
    /// `ds` is delta since start of transaction.
    /// `x` is the value being inserted or deleted.
    /// `insert` indicates type of update (`true` for insert, `false` for delete).
    /// Returns `true` if the update modifies the relation, i.e., it's not a no-op.
    fn set_update(
        s: &mut ValSet,
        ds: &mut DeltaSet,
        upd: Update<DDValue>,
        updates: &mut Vec<Update<DDValue>>,
    ) -> Response<()> {
        let ok = match &upd {
            Update::Insert { v, .. } => {
                let new = s.insert(v.clone());
                if new {
                    Self::delta_inc(ds, v);
                }

                new
            }
            Update::DeleteValue { v, .. } => {
                let present = s.remove(v);
                if present {
                    Self::delta_dec(ds, v);
                }

                present
            }
            Update::InsertOrUpdate { relid, .. } => {
                return Err(format!(
                    "Cannot perform insert_or_update operation on relation {} that does not have a primary key",
                    relid,
                ));
            }
            Update::DeleteKey { relid, .. } => {
                return Err(format!(
                    "Cannot delete by key from relation {} that does not have a primary key",
                    relid,
                ));
            }
            Update::Modify { relid, .. } => {
                return Err(format!(
                    "Cannot modify record in relation {} that does not have a primary key",
                    relid,
                ));
            }
        };

        if ok {
            updates.push(upd);
        }

        Ok(())
    }

    /// insert:
    ///      key exists in `s`:
    ///          - error
    ///      key not in `s`:
    ///          - s.insert(x)
    ///          - ds(x)++;
    /// delete:
    ///      key not in `s`
    ///          - return error
    ///      key in `s` with value `v`:
    ///          - s.delete(key)
    ///          - ds(v)--
    fn indexed_set_update(
        key_func: fn(&DDValue) -> DDValue,
        s: &mut IndexedValSet,
        ds: &mut DeltaSet,
        upd: Update<DDValue>,
        updates: &mut Vec<Update<DDValue>>,
    ) -> Response<()> {
        match upd {
            Update::Insert { relid, v } => match s.entry(key_func(&v)) {
                hash_map::Entry::Occupied(_) => Err(format!(
                    "Insert: duplicate key '{:?}' in value '{:?}'",
                    key_func(&v),
                    v
                )),
                hash_map::Entry::Vacant(ve) => {
                    ve.insert(v.clone());
                    Self::delta_inc(ds, &v);
                    updates.push(Update::Insert { relid, v });

                    Ok(())
                }
            },

            Update::InsertOrUpdate { relid, v } => match s.entry(key_func(&v)) {
                hash_map::Entry::Occupied(mut oe) => {
                    // Delete old value.
                    let old = oe.get().clone();
                    Self::delta_dec(ds, oe.get());
                    updates.push(Update::DeleteValue { relid, v: old });

                    // Insert new value.
                    Self::delta_inc(ds, &v);
                    updates.push(Update::Insert {
                        relid,
                        v: v.clone(),
                    });

                    // Update store
                    *oe.get_mut() = v;

                    Ok(())
                }
                hash_map::Entry::Vacant(ve) => {
                    ve.insert(v.clone());
                    Self::delta_inc(ds, &v);
                    updates.push(Update::Insert { relid, v });

                    Ok(())
                }
            },

            Update::DeleteValue { relid, v } => match s.entry(key_func(&v)) {
                hash_map::Entry::Occupied(oe) => {
                    if *oe.get() != v {
                        Err(format!("DeleteValue: key exists but with a different value. Value specified: '{:?}'; existing value: '{:?}'", v, oe.get()))
                    } else {
                        Self::delta_dec(ds, oe.get());
                        oe.remove_entry();
                        updates.push(Update::DeleteValue { relid, v });
                        Ok(())
                    }
                }
                hash_map::Entry::Vacant(_) => {
                    Err(format!("DeleteValue: key not found '{:?}'", key_func(&v)))
                }
            },

            Update::DeleteKey { relid, k } => match s.entry(k.clone()) {
                hash_map::Entry::Occupied(oe) => {
                    let old = oe.get().clone();
                    Self::delta_dec(ds, oe.get());
                    oe.remove_entry();
                    updates.push(Update::DeleteValue { relid, v: old });
                    Ok(())
                }
                hash_map::Entry::Vacant(_) => Err(format!("DeleteKey: key not found '{:?}'", k)),
            },

            Update::Modify { relid, k, m } => match s.entry(k.clone()) {
                hash_map::Entry::Occupied(mut oe) => {
                    let new = oe.get_mut();
                    let old: DDValue = (*new).clone();
                    m.mutate(new)?;
                    Self::delta_dec(ds, &old);
                    updates.push(Update::DeleteValue { relid, v: old });
                    Self::delta_inc(ds, new);
                    updates.push(Update::Insert {
                        relid,
                        v: new.clone(),
                    });

                    Ok(())
                }
                hash_map::Entry::Vacant(_) => Err(format!("Modify: key not found '{:?}'", k)),
            },
        }
    }

    /// Returns a reference to indexed input relation content.
    /// If called in the middle of a transaction, returns state snapshot including changes
    /// made by the current transaction.
    pub fn get_input_relation_index(&self, relid: RelId) -> Response<&IndexedValSet> {
        match self.relations.get(&relid) {
            None => Err(format!("unknown relation {}", relid)),
            Some(RelationInstance::Indexed { elements, .. }) => Ok(elements),
            Some(_) => Err(format!("not an indexed relation {}", relid)),
        }
    }

    /// Returns a reference to a flat input relation content.
    /// If called in the middle of a transaction, returns state snapshot including changes
    /// made by the current transaction.
    pub fn get_input_relation_data(&self, relid: RelId) -> Response<&ValSet> {
        match self.relations.get(&relid) {
            None => Err(format!("unknown relation {}", relid)),
            Some(RelationInstance::Flat { elements, .. }) => Ok(elements),
            Some(_) => Err(format!("not a flat relation {}", relid)),
        }
    }

    /// Returns a reference to an input multiset content.
    /// If called in the middle of a transaction, returns state snapshot including changes
    /// made by the current transaction.
    pub fn get_input_multiset_data(&self, relid: RelId) -> Response<&ValMSet> {
        match self.relations.get(&relid) {
            None => Err(format!("unknown relation {}", relid)),
            Some(RelationInstance::Multiset { elements, .. }) => Ok(elements),
            Some(_) => Err(format!("not a flat relation {}", relid)),
        }
    }

    /*
    /// Returns a reference to delta accumulated by the current transaction
    pub fn relation_delta(&mut self, relid: RelId) -> Response<&DeltaSet<V>> {
        if !self.transaction_in_progress {
            return resp_from_error!("no transaction in progress");
        };

        self.flush().and_then(move |_| {
            match self.relations.get_mut(&relid) {
                None => resp_from_error!("unknown relation"),
                Some(rel) => Ok(&rel.delta)
            }
        })
    }
    */

    /// Send message to a worker thread.
    fn send(&self, worker_index: usize, msg: Msg) -> Response<()> {
        match self.senders[worker_index].send(msg) {
            Ok(()) => {
                // Worker may be blocked in `step_or_park`. Unpark it to ensure
                // the message is received.
                self.worker_guards.as_ref().unwrap().guards()[worker_index]
                    .thread()
                    .unpark();

                Ok(())
            }

            Err(_) => Err(format!(
                "failed to communicate with timely dataflow thread {}",
                worker_index
            )),
        }
    }

    /// Broadcast message to all worker threads.
    fn broadcast(&self, msg: Msg) -> Response<()> {
        for worker_index in 0..self.senders.len() {
            self.send(worker_index, msg.clone())?;
        }

        Ok(())
    }

    /// Clear delta sets of all input relations on transaction commit.
    fn delta_cleanup(&mut self) {
        for rel in self.relations.values_mut() {
            rel.delta_mut().clear();
        }
    }

    fn delta_undo_updates(relid: RelId, ds: &DeltaSet, updates: &mut Vec<Update<DDValue>>) {
        // first delete, then insert to avoid duplicate key
        // errors in `apply_updates()`
        for (k, w) in ds {
            if *w >= 0 {
                for _ in 0..*w {
                    updates.push(Update::DeleteValue {
                        relid,
                        v: k.clone(),
                    });
                }
            }
        }

        for (k, w) in ds {
            if *w < 0 {
                for _ in 0..(-*w) {
                    updates.push(Update::Insert {
                        relid,
                        v: k.clone(),
                    });
                }
            }
        }
    }

    /// Reverse all changes recorded in delta sets to rollback the transaction.
    fn delta_undo(&mut self) -> Response<()> {
        let mut updates = Vec::with_capacity(self.relations.len());
        for (relid, rel) in &self.relations {
            Self::delta_undo_updates(*relid, rel.delta(), &mut updates);
        }

        // println!("updates: {:?}", updates);
        self.apply_updates(updates.into_iter(), |_| Ok(()))
            .and_then(|_| self.flush())
            .map(|_| {
                /* validation: all deltas must be empty */
                for rel in self.relations.values() {
                    //println!("delta: {:?}", *d);
                    debug_assert!(rel.delta().is_empty());
                }
            })
    }

    /// Propagates all changes through the dataflow pipeline.
    fn flush(&mut self) -> Response<()> {
        if !self.need_to_flush {
            return Ok(());
        }

        self.broadcast(Msg::Flush {
            advance_to: self.timestamp + 1,
        })
        .and_then(|()| {
            self.timestamp += 1;
            self.need_to_flush = false;
            self.await_flush_ack()
        })
    }

    /// Wait for all workers to complete the `Flush` command.  This guarantees
    /// that all outputs have been produced and we have successfully committed
    /// the current transaction.
    fn await_flush_ack(&self) -> Response<()> {
        for (worker_index, receiver) in self.reply_recv.iter().enumerate() {
            match receiver.recv() {
                Err(_) => {
                    return Err(format!(
                        "failed to receive flush ack message from worker {}",
                        worker_index
                    ))
                }
                Ok(Reply::FlushAck) => (),
                Ok(msg) => {
                    return Err(format!(
                        "received unexpected reply to flush request from worker {}: {:?}",
                        worker_index, msg,
                    ))
                }
            }
        }
        Ok(())
    }
}

impl Drop for RunningProgram {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
