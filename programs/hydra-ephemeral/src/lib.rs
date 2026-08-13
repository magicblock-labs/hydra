#![no_std]

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint;

mod processor;

pub use hydra_api::ephemeral::*;
