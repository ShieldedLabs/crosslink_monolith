#![allow(missing_docs)]
#![forbid(unsafe_code)]
//! `ZingoLib`
//! Zingo backend library

pub mod config;
pub mod data;
pub mod error;
pub mod grpc_client;
pub mod grpc_connector;
pub mod lightclient;
pub mod utils;
pub mod wallet;

#[cfg(test)]
pub mod mocks;
#[cfg(any(test, feature = "testutils"))]
pub mod testutils;

#[macro_use]
extern crate rust_embed;
/// Embedded zcash-params for mobile devices.
#[derive(RustEmbed)]
#[folder = "zcash-params/"]
pub struct SaplingParams;
