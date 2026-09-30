//! Watchdog task that prunes sessions idle for too long.
//!
//! The WS handler is expected to remove its own session from the map when
//! the connection drops. This watchdog is a safety net for the case where
//! a handler task is somehow blocked without unregistering.

use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info};
use uuid::Uuid;

use crate::session::{unregister, SessionMap};

/// Run the watchdog loop forever.
///
/// `idle_timeout` is the inactivity threshold; sessions whose
/// `last_activity_ms` is older than `now - idle_timeout` are removed.
///
/// The loop ticks every `idle_timeout / 3` (bounded to `[1s, 30s]`).
pub async fn run(map: SessionMap, idle_timeout: Duration) {
    let tick = (idle_timeout / 3).clamp(Duration::from_secs(1), Duration::from_secs(30));
    info!(?idle_timeout, tick = ?tick, "watchdog started");
    let mut ticker = interval(tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        sweep(&map, idle_timeout);
    }
}

/// One pass over the session map.
pub fn sweep(map: &SessionMap, idle_timeout: Duration) {
    let idle_ms = idle_timeout.as_millis() as u64;
    let mut to_remove: Vec<Uuid> = Vec::new();
    for entry in map.iter() {
        if entry.value().is_idle(idle_ms) {
            to_remove.push(*entry.key());
        }
    }
    for id in to_remove {
        if unregister(map, id).is_some() {
            debug!(%id, "watchdog removed idle session");
        }
    }
}
