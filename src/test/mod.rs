//! behaviors the design promises, asserted over synthetic zones shaped like
//! a real fleet's: proxied records reported with ttl 1, quoted and bare TXT,
//! ACME challenges, a commented dummy record, Email Routing's MX and TXT.

pub(crate) mod fake;
mod fixture;
mod plan;
mod run;
