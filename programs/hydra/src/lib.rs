#![no_std]

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint;

mod helpers;
mod processor;

pub use hydra_api::{base::ID as BASE_ID, ephemeral::ID as EPHEMERAL_ID};
