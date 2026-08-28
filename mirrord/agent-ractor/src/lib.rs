//! # mirrord-agent-ractor
//!
//! A spike port of the mirrord-agent DNS + outgoing traffic features onto the
//! [`ractor`] actor framework, replacing the hand-rolled mix of tokio channels,
//! `SelectAll`s, abort handles and buffered stream/sink wrappers with a
//! supervision tree of small, single-purpose actors.
//!
//! ## Actor tree
//!
//! ```text
//! agent                               (accepts client TCP connections)
//! └── client-{N}                      (owns the client socket write half + session state)
//!     ├── client-{N}.dns              (per-client DNS lookups, ordered responses)
//!     ├── client-{N}.tcp-out          (routes TCP/UNIX outgoing traffic)
//!     │   └── client-{N}.tcp-out.conn-{M}   (one actor per outgoing connection)
//!     └── client-{N}.udp-out          (routes UDP outgoing traffic)
//!         └── client-{N}.udp-out.conn-{M}
//! ```
//!
//! Socket reads cannot live inside an actor's message loop, so each actor that
//! reads from a socket owns a plain tokio task that decodes/reads and casts the
//! results to the right actor. Those tasks are aborted via [`util::TaskGuard`]s
//! held in actor state, which also covers the hard-kill path (ractor kills the
//! whole subtree when a supervisor exits, skipping `post_stop`).
//!
//! Only the targetless mode is supported: no iptables, no network namespace
//! switching, no incoming traffic features.

#[cfg(target_os = "linux")]
pub mod agent;
#[cfg(target_os = "linux")]
pub mod budget;
#[cfg(target_os = "linux")]
pub mod client;
#[cfg(target_os = "linux")]
pub mod codec;
#[cfg(target_os = "linux")]
pub mod cpu_sample;
#[cfg(target_os = "linux")]
pub mod dns;
#[cfg(target_os = "linux")]
pub mod outgoing;
#[cfg(target_os = "linux")]
pub mod util;
