//! mcpurge: find, inspect, stop and purge MCP servers across every AI agent
//! configured on this machine.
//!
//! The crate is split so the engine is usable without the GUI:
//!
//! * [`model`]   - domain types shared by everything.
//! * [`registry`] - data-driven list of where clients keep their config.
//! * [`scan`]    - registry probe, content sweep, parsing, orchestration.
//! * [`proc`]    - matching servers to live processes, and stopping them.
//! * [`files`]   - locating a server's files, and judging what is safe to delete.
//! * [`purge`]   - backup, surgical config edits, trash.
//! * [`theme`]   - the taupe palettes, light and dark.
//! * [`menu`]    - the menu bar, native on macOS and in-window elsewhere.
//! * [`update`]  - the signed self-updater.
//! * [`app`]     - the egui front end.

pub mod app;
pub mod files;
pub mod menu;
pub mod model;
pub mod proc;
pub mod purge;
pub mod registry;
pub mod scan;
pub mod theme;
pub mod update;
