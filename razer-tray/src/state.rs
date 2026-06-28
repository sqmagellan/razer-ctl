//! The tray's device-state data model now lives in `librazer::state` (it's pure,
//! GUI-free logic, so it sits in the library where it can be unit-tested on any
//! host against a mock transport). This shim keeps the existing `crate::state::*`
//! paths working unchanged across menu.rs / program.rs / main.rs.
pub use librazer::state::*;
