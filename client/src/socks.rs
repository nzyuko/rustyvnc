use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use uuid::Uuid;

#[derive(Debug)]
pub struct SocksOut {
    pub conn_id: Uuid,
    pub agent_id: Uuid,
    pub index: i32,
    pub job_id: String,
    pub token: Uuid,
    pub data: Vec<u8>,
    pub close: bool,
}

#[derive(Clone)]
pub struct WakeSignal {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl WakeSignal {
    pub fn new() -> Option<Self> {
        Some(Self {
            inner: Arc::new((Mutex::new(false), Condvar::new())),
        })
    }

    pub fn clone_ref(&self) -> Self {
        self.clone()
    }

    #[allow(dead_code)]
    pub fn notify(&self) {
        let (lock, cv) = &*self.inner;
        if let Ok(mut ready) = lock.lock() {
            *ready = true;
            cv.notify_one();
        }
    }

    #[allow(dead_code)]
    pub fn wait(&self, timeout: Duration) {
        let (lock, cv) = &*self.inner;
        if let Ok(ready) = lock.lock() {
            let waited = cv.wait_timeout_while(ready, timeout, |ready| !*ready);
            let Ok((mut ready, _)) = waited else {
                return;
            };
            *ready = false;
        }
    }
}
