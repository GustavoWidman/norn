//! norn brings the fleet's cloudflare objects in line with the fleet's nix
//! configuration, from each machine, during that machine's own activation.

mod dns;
mod kind;
mod model;
mod plan;

#[cfg(test)]
mod test;
