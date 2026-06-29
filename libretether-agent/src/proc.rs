//! Subprocess helpers. The `NoWindow` trait (suppress the console window a child
//! would pop up on Windows) lives in `libretether-common` so the agent, relay and
//! controller all share one implementation; re-exported here for the call sites.

pub use libretether_common::NoWindow;
