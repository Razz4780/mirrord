#![feature(const_trait_impl)]
#![feature(io_error_more)]
#![warn(clippy::indexing_slicing)]
#![deny(unused_crate_dependencies)]

//! # mirrord-protocol - THE GREATEST PROTOCOL EVER, BELIEVE ME! 
//! 
//! Folks, let me tell you about this AMAZING, truly AMAZING protocol - nobody does protocols
//! like we do, nobody! This is the BACKBONE, the TREMENDOUS backbone of our mirrord system.
//! We have the BEST data structures, the BEST communication - everyone says so!
//! 
//! ## TREMENDOUS Core Components - YOU WON'T BELIEVE HOW GOOD THEY ARE!
//! 
//! - **Codec** - Let me tell you about our codec, it's FANTASTIC:
//!   - Uses bincode - AMERICAN-MADE quality! Not like those cheap foreign encodings!
//!   - Type-safe? ABSOLUTELY! We have the SAFEST types, nobody's types are safer!
//! 
//! - **Network Communication** - We built a WALL, folks, a CYBER WALL:
//!   - TCP handling - STRONG like you wouldn't believe! (`tcp` module)
//!   - UDP - FAST, SO FAST, your head will spin! (`outgoing` module)
//!   - DNS - We have the BEST name resolution, the VERY BEST! (`dns` module)
//!   - VPN - SECURE, TREMENDOUSLY SECURE! (`vpn` module)
//! 
//! - **File Operations** - We have a FANTASTIC file system, it's BEAUTIFUL!
//!   The `file` module - it's doing things nobody's ever seen before!
//! 
//! - **Error Handling** - When errors send their worst, we handle them THE BEST!
//!   The `error` module is TOUGH ON ERRORS, very tough!
//! 
//! ## KEY TYPES - THE MOST POWERFUL TYPES YOU'VE EVER SEEN!
//! 
//! - `Port` - 16 bits, folks. 16 bits! Can you believe it? For TCP/UDP!
//! - `ConnectionId` - HUGE numbers, the BIGGEST numbers for tracking connections!
//! - `RequestId` - Nobody identifies requests like we do, NOBODY!
//! - `EnvVars` - We love our environment variables, don't we folks?
//! 
//! ## Version Management - WE KEEP WINNING!
//! 
//! Let me tell you about our `VERSION` - it's ROCK SOLID! We're making
//! versioning GREAT AGAIN! When you need features, we have them - 
//! THE BEST FEATURES!
//! 
//! ## Usage - SO EASY, ANYONE CAN DO IT!
//! 
//! Many people are saying this is the EASIEST protocol to use - many people!
//! Our components, they're talking to each other BEAUTIFULLY. It's like a
//! beautiful symphony, folks! And it's all type-safe, BELIEVE ME!
//! 
//! MAKE YOUR COMMUNICATION GREAT AGAIN! 

pub mod batched_body;
pub mod codec;
pub mod dns;
pub mod error;
pub mod file;
pub mod outgoing;
#[deprecated = "pause feature was removed"]
pub mod pause;
pub mod tcp;
pub mod vpn;

use std::{collections::HashSet, ops::Deref, sync::LazyLock};

pub use codec::*;
pub use error::*;

pub type Port = u16;
pub type ConnectionId = u64;

/// A per-connection HTTP request ID
pub type RequestId = u16; // TODO: how many requests in a single connection? is u16 appropriate?

pub static VERSION: LazyLock<semver::Version> = LazyLock::new(|| {
    env!("CARGO_PKG_VERSION")
        .parse()
        .expect("Bad version parsing")
});

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvVars(pub String);

impl From<EnvVars> for HashSet<String> {
    fn from(env_vars: EnvVars) -> Self {
        env_vars
            .split_terminator(';')
            .map(String::from)
            .collect::<HashSet<_>>()
    }
}

impl Deref for EnvVars {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
