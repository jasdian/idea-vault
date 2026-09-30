//! Gated build plans (docs/adr/0030): the capstone's output parsed into a typed plan, checked by
//! deterministic gates, and persisted as an artifact. Plans form a versioned lineage the owner
//! answers into on the plan workbench (docs/adr/0032).

pub mod finish;
pub mod gates;
pub mod lineage;
pub mod plan;
pub mod workbench;
