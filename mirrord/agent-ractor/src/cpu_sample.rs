//! Self-sampling of the agent's CPU usage, for benchmarking.
//!
//! When [`ENV`] is set to a sampling period in milliseconds, a background task
//! periodically reads this process's cumulative CPU time from `/proc/self/stat`
//! and prints `CPUSAMPLE <epoch_ms> <utime+stime ticks>` lines (USER_HZ = 100)
//! to stdout, where the pod log collects them. The benchmark harness slices the
//! series by the client's start/end timestamps, so no exec-into-the-pod is
//! needed to measure agent CPU.
//!
//! Plain `println!` instead of `tracing`, so samples flow regardless of the log
//! filter. The identical module is planted in mirrord-agent for the benchmark,
//! keeping the (negligible) sampling overhead the same on both sides.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ENV: &str = "MIRRORD_AGENT_CPU_SAMPLE_MS";

/// Cumulative utime+stime of this process, in USER_HZ ticks.
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The comm field is parenthesized and may contain spaces; fields of interest
    // come after its closing paren: utime and stime are fields 14 and 15 of the
    // full line, i.e. 12 and 13 after the comm.
    let (_, after_comm) = stat.rsplit_once(')')?;
    let mut fields = after_comm.split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis()
}

/// Spawns the sampling task if the env var is set. Must be called from within a
/// tokio runtime.
pub fn spawn_if_configured() {
    let Some(period_ms) = std::env::var(ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|period| *period > 0)
    else {
        return;
    };

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(period_ms));
        loop {
            interval.tick().await;
            if let Some(ticks) = cpu_ticks() {
                println!("CPUSAMPLE {} {ticks}", epoch_ms());
            }
        }
    });
}
