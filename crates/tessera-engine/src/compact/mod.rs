//! The fold (compaction). It writes a new prefix whose base holds everything the live prefix
//! holds except deleted entities. [`plan_fold`] runs on the executor against the live generation
//! and is pure. [`execute`] runs on one dedicated thread, off the request pool, and runs its
//! passes in sequence. Publication is `Executor::publish_fold`. Merge and coalesce are suspended
//! while a fold is in flight.
//!
//! # Which deletions a fold retires
//!
//! A deletion's overlay entry is removed only by the fold that removed the entity's rows and
//! postings. The plan's tombstone set ([`FoldPlan::tombstones`]) is what the passes run over, but
//! it is not the set that retires. A flush that planned before the delete was accepted can
//! publish the entity's row and postings while the fold runs. The fold carries that segment
//! forward, and removing the overlay entry would then expose a deleted item.
//!
//! So retirement is computed at publication: [`executed`] is the tombstone set minus every entity
//! a carried-forward artefact names ([`CarriedForward`]). `CarriedForward` must name at least
//! those entities. Naming too many keeps a tombstone for another fold, which is safe. Naming too
//! few exposes a deleted item. It takes each carried segment's and locator extent's whole entity
//! range: a flush publishes its segment, tier, run and locator extent over one contiguous range,
//! so the range covers all four, including an item with no terms, which no tier names.

mod attributes;
mod cost;
mod execute;
mod plan;
mod retire;
mod schedule;

pub(crate) use cost::Staircase;
pub use cost::PassCost;
pub(crate) use execute::{execute, next_prefix_name, CompletedFold, FoldContext};
pub(crate) use plan::{plan_fold, FoldPlan, FoldResources, NoFold};
pub(crate) use retire::{executed, CarriedForward};
pub use schedule::CompactionSchedule;
pub(crate) use schedule::{due, DeadBytes, FoldTrigger, Gauges};
