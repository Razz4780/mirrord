//! Small helpers shared by the actors.

use tokio::task::JoinHandle;

/// Aborts the wrapped task when dropped.
///
/// Actors hold their IO tasks through this guard inside their state. When an actor
/// exits - including the hard-kill path used by ractor to tear down a supervision
/// subtree, which skips `post_stop` - the state is dropped and the task dies with it.
#[derive(Debug)]
pub struct TaskGuard(JoinHandle<()>);

impl TaskGuard {
    pub fn new(handle: JoinHandle<()>) -> Self {
        Self(handle)
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Identifies a client connected to this agent. Used in actor names and logs.
pub type ClientId = u32;
