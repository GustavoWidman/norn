//! norn brings the fleet's cloudflare objects in line with the fleet's nix
//! configuration, from each machine, during that machine's own activation.
//!
//! the binary's commands are the crate's surface, plus [cloudflare], whose
//! client types carry what a run may do.

pub mod cloudflare;
mod desired;
mod dns;
mod kind;
mod model;
mod plan;
mod report;
mod run;
mod status;

#[cfg(test)]
mod test;

pub use desired::Problems;
pub use run::{Timing, check, converge, report, status};
