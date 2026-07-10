//! reeve server extensions (docs/build-charter.md CODE BOUNDARY):
//! whole modules behind cargo features, default ON; core never depends
//! on ext items. Each module wires itself into router.rs/render.rs
//! under its own `cfg(feature = "ext-<name>")` gates.

#[cfg(feature = "ext-channel")]
pub mod channel;
#[cfg(feature = "ext-rollouts")]
pub mod rollouts;
#[cfg(feature = "ext-secrets")]
pub mod secrets;
#[cfg(feature = "ext-sse")]
pub mod sse;
#[cfg(feature = "ext-terminal")]
pub mod terminal;
