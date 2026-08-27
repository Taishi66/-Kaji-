use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::utils::bytes_to_hex;

pub trait IdGen: Send + Sync {
    fn next_message_id(&self) -> String;
}

pub struct SessionIdGen {
    seed: String,
    counter: AtomicU64,
}

impl SessionIdGen {
    pub fn new(seed: &str) -> Self {
        Self {
            seed: seed.to_string(),
            counter: AtomicU64::new(0),
        }
    }
}

impl IdGen for SessionIdGen {
    fn next_message_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut h = Sha256::new();
        h.update(self.seed.as_bytes());
        h.update(b":");
        h.update(n.to_le_bytes());
        let hex = bytes_to_hex(h.finalize());
        let short: String = hex.chars().take(32).collect();
        format!("msg_{short}")
    }
}

pub struct RandomIdGen;

impl IdGen for RandomIdGen {
    #[allow(clippy::disallowed_methods)]
    fn next_message_id(&self) -> String {
        format!("msg_{}", uuid::Uuid::new_v4())
    }
}

pub fn default_idgen() -> Arc<dyn IdGen> {
    Arc::new(RandomIdGen)
}
