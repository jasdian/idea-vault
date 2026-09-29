//! Gated build plans (docs/adr/0029): the capstone's output parsed into a typed plan, checked by
//! deterministic gates, and persisted as an artifact.

pub mod finish;
pub mod gates;
pub mod plan;
