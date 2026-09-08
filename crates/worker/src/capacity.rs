//! Reserve the full approved guest linear-memory limit before claiming execution.
//! This budget excludes JIT, compiled code, host buffers and runtime overhead.
use crate::metrics::Metrics;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MIB: u64 = 1024 * 1024;
pub(crate) struct MemoryBudget {
    permits: Arc<Semaphore>,
    metrics: Arc<Metrics>,
}
pub(crate) struct MemoryReservation {
    _permit: OwnedSemaphorePermit,
    bytes: i64,
    metrics: Arc<Metrics>,
}
impl MemoryBudget {
    pub(crate) fn new(mib: u32, metrics: Arc<Metrics>) -> Self {
        metrics
            .guest_memory_budget_bytes
            .set(i64::from(mib) * MIB as i64);
        Self {
            permits: Arc::new(Semaphore::new(mib as usize)),
            metrics,
        }
    }
    pub(crate) fn try_reserve(&self, bytes: u64) -> Option<MemoryReservation> {
        let mib = u32::try_from(bytes.div_ceil(MIB)).ok()?;
        if mib == 0 {
            return None;
        }
        let permit = self.permits.clone().try_acquire_many_owned(mib).ok()?;
        let bytes = i64::from(mib) * MIB as i64;
        self.metrics.guest_memory_reserved_bytes.add(bytes);
        Some(MemoryReservation {
            _permit: permit,
            bytes,
            metrics: self.metrics.clone(),
        })
    }
}
impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.metrics.guest_memory_reserved_bytes.sub(self.bytes);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn aggregate_limit_rounding_and_cancelled_task_release() {
        let metrics = Metrics::init();
        let budget = MemoryBudget::new(2, metrics.clone());
        assert!(budget.try_reserve(0).is_none());
        assert!(budget.try_reserve(u64::MAX).is_none());
        let first = budget.try_reserve(1).unwrap();
        assert!(budget.try_reserve(MIB + 1).is_none());
        let second = budget.try_reserve(MIB).unwrap();
        assert!(budget.try_reserve(1).is_none());
        let task = tokio::spawn(async move {
            let _hold = second;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert_eq!(metrics.guest_memory_reserved_bytes.get(), MIB as i64);
        drop(first);
        assert_eq!(metrics.guest_memory_reserved_bytes.get(), 0);
        assert!(budget.try_reserve(2 * MIB).is_some());
    }
}
