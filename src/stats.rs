use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// [`Relay`] connection stats
#[derive(Debug, Clone)]
pub struct RelayConnectionStats {
    attempts: Arc<AtomicUsize>,
    success: Arc<AtomicUsize>,
    connected_at: Arc<AtomicU64>,
}

impl Default for RelayConnectionStats {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayConnectionStats {
    /// New connections stats
    pub fn new() -> Self {
        Self {
            attempts: Arc::new(AtomicUsize::new(0)),
            success: Arc::new(AtomicUsize::new(0)),
            connected_at: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The number of times a connection has been attempted
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    /// The number of times a connection has been successfully established
    pub fn success(&self) -> usize {
        self.success.load(Ordering::SeqCst)
    }

    /// Get the UNIX timestamp of the last started connection
    pub fn connected_at(&self) -> u64 {
        self.connected_at.load(Ordering::SeqCst)
    }

    pub(crate) fn new_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn new_success(&self) {
        self.success.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .connected_at
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |_| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                Some(now)
            });
    }
}
