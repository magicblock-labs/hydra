#![no_std]

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint;

mod helpers;
mod processor;

pub use hydra_api::ID;
