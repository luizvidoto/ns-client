use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// [`Relay`] connection stats
#[derive(Debug, Clone)]
pub struct RelayConnectionStats {
    attempts: usize,
    success: usize,
    connected_at: u64,
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
            attempts: 0,
            success: 0,
            connected_at: 0,
        }
    }

    /// The number of times a connection has been attempted
    pub fn attempts(&self) -> usize {
        self.attempts
    }

    /// The number of times a connection has been successfully established
    pub fn success(&self) -> usize {
        self.success
    }

    /// Get the UNIX timestamp of the last started connection
    pub fn connected_at(&self) -> u64 {
        self.connected_at
    }

    pub(crate) fn new_attempt(&mut self) {
        self.attempts += 1;
    }

    pub(crate) fn new_success(&mut self) {
        self.success += 1;
        self.attempts = 0;
        self.connected_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
    }
}
