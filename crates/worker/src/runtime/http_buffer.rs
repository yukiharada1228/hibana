//! Account for outbound HTTP payload allocations until the last Bytes owner drops.
//! These budgets are independent of guest linear memory and include buffer growth.
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wasmtime_wasi_http::bindings::http::types::ErrorCode;

const INVOCATION_BYTES: usize = 128 * 1024 * 1024;
const WORKER_BYTES: usize = 256 * 1024 * 1024;

pub(super) fn worker_budget() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(WORKER_BYTES))
}

#[derive(Clone)]
pub(super) struct Budget {
    invocation: Arc<Semaphore>,
    worker: Arc<Semaphore>,
}

impl Budget {
    pub(super) fn new(worker: Arc<Semaphore>) -> Self {
        Self {
            invocation: Arc::new(Semaphore::new(INVOCATION_BYTES)),
            worker,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(invocation: usize, worker: Arc<Semaphore>) -> Self {
        Self {
            invocation: Arc::new(Semaphore::new(invocation)),
            worker,
        }
    }

    fn reserve(&self, bytes: usize) -> Result<Reservation, ErrorCode> {
        let exhausted =
            || ErrorCode::InternalError(Some("outbound HTTP buffer capacity exhausted".into()));
        let bytes = u32::try_from(bytes).map_err(|_| exhausted())?;
        let invocation = self
            .invocation
            .clone()
            .try_acquire_many_owned(bytes)
            .map_err(|_| exhausted())?;
        let worker = self
            .worker
            .clone()
            .try_acquire_many_owned(bytes)
            .map_err(|_| exhausted())?;
        Ok(Reservation {
            _invocation: invocation,
            _worker: worker,
        })
    }
}

struct Reservation {
    _invocation: OwnedSemaphorePermit,
    _worker: OwnedSemaphorePermit,
}

pub(super) struct Buffer {
    data: Vec<u8>,
    reservation: Option<Reservation>,
    budget: Budget,
}

impl Buffer {
    pub(super) fn new(budget: Budget) -> Self {
        Self {
            data: Vec::new(),
            reservation: None,
            budget,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.data.len()
    }

    pub(super) fn extend(&mut self, chunk: &[u8]) -> Result<(), ErrorCode> {
        let len = self.data.len() + chunk.len();
        if len > self.data.capacity() {
            let capacity = len.next_power_of_two();
            // Reserve the entire replacement while the old allocation is alive.
            // In-place Vec growth could otherwise temporarily exceed the budget.
            let reservation = self.budget.reserve(capacity)?;
            let mut data = Vec::with_capacity(capacity);
            data.extend_from_slice(&self.data);
            self.data = data;
            self.reservation = Some(reservation);
        }
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    pub(super) fn into_bytes(self) -> Bytes {
        // A guard on Full/IncomingResponse would release too early: Wasmtime may
        // retain a data frame after dropping that body. Bytes owns the guard too.
        Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_frames_share_invocation_and_worker_budgets_until_the_last_drop() {
        let worker = Arc::new(Semaphore::new(64));
        let first = Budget::for_test(32, worker.clone());
        let second = Budget::for_test(64, worker.clone());
        let mut a = Buffer::new(first.clone());
        a.extend(&[1; 32]).unwrap();
        let data = a.into_bytes();
        let slice = data.slice(0..1);
        let mut another = Buffer::new(first.clone());
        assert!(another.extend(&[1]).is_err());
        let mut b = Buffer::new(second.clone());
        b.extend(&[2; 32]).unwrap();
        let mut full_worker = Buffer::new(second.clone());
        assert!(full_worker.extend(&[1]).is_err());
        assert_eq!(
            second.invocation.available_permits(),
            32,
            "failed worker reservation returns invocation capacity"
        );
        drop(data);
        assert!(
            another.extend(&[1]).is_err(),
            "even a slice retains the allocation"
        );
        drop(slice);
        another.extend(&[1]).unwrap();
        drop((another, b, full_worker));
        assert_eq!(worker.available_permits(), 64);
        assert_eq!(first.invocation.available_permits(), 32);
    }

    #[tokio::test]
    async fn growth_and_cancellation_release_every_reservation() {
        let worker = Arc::new(Semaphore::new(128));
        let budget = Budget::for_test(64, worker.clone());
        let mut buffer = Buffer::new(budget.clone());
        buffer.extend(&[1; 17]).unwrap();
        assert_eq!(
            worker.available_permits(),
            96,
            "account allocated capacity, not length"
        );
        assert!(
            buffer.extend(&[2; 16]).is_err(),
            "growth counts the old and new allocations"
        );
        assert_eq!(buffer.as_ref(), &[1; 17]);
        drop(buffer);
        let entered = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let entered = entered.clone();
            async move {
                let mut buffer = Buffer::new(budget);
                buffer.extend(&[3; 33]).unwrap();
                entered.notify_one();
                std::future::pending::<()>().await;
                drop(buffer);
            }
        });
        entered.notified().await;
        assert_eq!(worker.available_permits(), 64);
        task.abort();
        let _ = task.await;
        assert_eq!(worker.available_permits(), 128);
    }
}
