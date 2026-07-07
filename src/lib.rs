//! idea-vault — localhost ideation tool. Module graph per docs/02-module-reference.md (D4/D5):
//! one-way deps, nothing depends on `web`; markdown is truth, SQLite is a rebuildable index.

pub mod ai;
pub mod app;
pub mod concepts;
pub mod config;
pub mod domain;
pub mod index;
pub mod memory;
pub mod vault;
pub mod web;
