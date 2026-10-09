//! Compaction (the fold) writes a new prefix holding everything the live prefix holds except
//! deleted entities. [`plan_fold`] is pure and runs on the executor, [`execute`] runs the passes
//! on one dedicated thread, and `Executor::publish_fold` publishes. Merge and coalesce wait for it.
//!
//! A deletion's overlay entry is removed only by the fold that removed the entity's rows and
//! postings. The passes run over the plan's tombstone set, but the set that retires is computed at
//! publication by [`executed`]: the tombstone set minus every entity a carried-forward artefact
//! names. A flush that planned before the delete can publish the entity's row while the fold
//! runs. The fold carries that row forward, and retiring the entry would expose a deleted item.

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
