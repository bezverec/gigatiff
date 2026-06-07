#[cfg(feature = "desktop")]
pub mod cli;
pub(crate) mod core;
#[cfg(feature = "desktop")]
pub(crate) mod gui;
pub(crate) mod options;
#[cfg(feature = "server")]
pub mod server;
