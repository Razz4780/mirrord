//! Memory budgets for client data buffered inside the agent.
//!
//! Actor mailboxes in ractor are unbounded, so data messages must not be cast
//! without limit - a fast peer (or client) could otherwise balloon the agent's
//! memory. Every data chunk travelling through the agent reserves its size from a
//! per-client, per-direction [`MemoryBudget`] *before* being cast, and carries the
//! resulting [`BudgetPermit`] inside the message. The permit is dropped only after
//! the chunk has left the agent (written to the peer socket, or flushed to the
//! client connection), which caps the total bytes in flight and converts the cap
//! into plain TCP backpressure at the reading task.
//!
//! This mirrors the `Throttle`/`Throttled` semantics of mirrord-agent (same
//! per-direction limits), just exposed as an async reservation instead of
//! `Stream`/`Sink` wrappers.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How much peer->client data one client session can buffer in memory.
pub const TO_CLIENT_LIMIT: usize = 512 * 1024;
/// How much client->peer data one client session can buffer in memory.
pub const FROM_CLIENT_LIMIT: usize = 512 * 1024;

/// Byte budget shared by all data of one direction of one client session.
///
/// Clones share the same underlying budget.
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    semaphore: Arc<Semaphore>,
    max: usize,
}

impl MemoryBudget {
    pub fn new(max: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max)),
            max,
        }
    }

    /// Reserves `bytes` from this budget, waiting until enough is free.
    ///
    /// Reservations larger than the whole budget are clamped, so a single
    /// oversized chunk cannot deadlock the session.
    pub async fn reserve(&self, bytes: usize) -> BudgetPermit {
        let permits = u32::try_from(bytes.min(self.max)).unwrap_or(u32::MAX);
        let permit = self
            .semaphore
            .clone()
            .acquire_many_owned(permits)
            .await
            .expect("the budget semaphore is never closed");
        BudgetPermit { _permit: permit }
    }
}

/// Proof of a [`MemoryBudget`] reservation. Returns the bytes to the budget on drop.
#[derive(Debug)]
pub struct BudgetPermit {
    _permit: OwnedSemaphorePermit,
}
