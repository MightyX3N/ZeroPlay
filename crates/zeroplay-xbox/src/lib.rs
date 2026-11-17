//! Xbox Remote Play for ZeroPlay.
//!
//! This crate signs in to a Microsoft account, lists the consoles on it,
//! starts a home-streaming session and connects to the console over WebRTC.
//!
//! The protocol knowledge comes from these MIT-licensed projects by UnknownSKL
//! (Jim Kroon): Greenlight, xbox-xcloud-player, xal-node and xbox-webapi-node.

pub mod api;
pub mod auth;
pub mod consoles;
pub mod input;
pub mod session;
pub mod teredo;
pub use reqwest;
