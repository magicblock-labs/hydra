pub mod common;

#[cfg(not(feature = "ephemeral"))]
mod base;
#[cfg(not(feature = "ephemeral"))]
pub use base::*;

#[cfg(feature = "ephemeral")]
mod ephemeral;
#[cfg(feature = "ephemeral")]
pub use ephemeral::*;
