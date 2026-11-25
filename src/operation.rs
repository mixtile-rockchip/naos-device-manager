use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Gate(Arc<Mutex<Option<String>>>);

#[derive(Debug, thiserror::Error)]
#[error("another device operation is active")]
pub struct Busy;

pub struct Lease(Arc<Mutex<Option<String>>>);

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_acquire(&self, operation: &str) -> Result<Lease, Busy> {
        let mut active = self.0.lock().expect("operation gate poisoned");
        if active.is_some() {
            return Err(Busy);
        }
        *active = Some(operation.to_owned());
        Ok(Lease(Arc::clone(&self.0)))
    }

    pub fn active(&self) -> Option<String> {
        self.0.lock().expect("operation gate poisoned").clone()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        *self.0.lock().expect("operation gate poisoned") = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_excludes_concurrent_operations_until_dropped() {
        let gate = Gate::new();
        let lease = gate.try_acquire("install").unwrap();
        let other = gate.clone();
        std::thread::spawn(move || {
            assert_eq!(other.active().as_deref(), Some("install"));
            assert!(other.try_acquire("reboot").is_err());
        })
        .join()
        .unwrap();
        drop(lease);
        assert!(gate.try_acquire("reboot").is_ok());
    }
}
