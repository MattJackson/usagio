//! Subcommand handlers (`usagio <cmd>`): one module per command family.

pub(crate) mod capture;
pub(crate) mod context;
pub(crate) mod install;
pub(crate) mod internal;
pub(crate) mod list;
pub(crate) mod renew;
pub(crate) mod report;
pub(crate) mod rm;
pub(crate) mod switch;
pub(crate) mod token;
pub(crate) mod watch;

#[cfg(test)]
mod tests;
