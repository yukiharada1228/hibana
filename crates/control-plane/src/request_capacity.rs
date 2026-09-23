//! Local capacity guards shared by public requests and deployment uploads.
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::{Semaphore, SemaphorePermit};

pub(crate) struct RequestCapacity {
    slots: Semaphore,
    tenants: Mutex<HashMap<String, usize>>,
}

impl RequestCapacity {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            slots: Semaphore::new(max),
            tenants: Mutex::default(),
        }
    }

    pub(crate) fn reserve(&self) -> Option<SemaphorePermit<'_>> {
        self.slots.try_acquire().ok()
    }

    pub(crate) fn reserve_tenant<'a>(
        &'a self,
        tenant: &'a str,
        max: usize,
    ) -> Option<TenantRequest<'a>> {
        let mut tenants = self.tenants.lock().expect("request capacity lock");
        if tenants.get(tenant).copied().unwrap_or(0) >= max {
            return None;
        }
        *tenants.entry(tenant.into()).or_default() += 1;
        Some(TenantRequest {
            capacity: self,
            tenant,
        })
    }
}

pub(crate) struct TenantRequest<'a> {
    capacity: &'a RequestCapacity,
    tenant: &'a str,
}

impl Drop for TenantRequest<'_> {
    fn drop(&mut self) {
        let mut tenants = self.capacity.tenants.lock().expect("request capacity lock");
        let count = tenants.get_mut(self.tenant).expect("reserved tenant");
        *count -= 1;
        if *count == 0 {
            tenants.remove(self.tenant);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[tokio::test]
    async fn capacity_is_shared_and_cancelled_receivers_release_their_slots() {
        let capacity = Arc::new(RequestCapacity::new(8));
        let permits: Vec<_> = (0..8).map(|_| capacity.reserve().unwrap()).collect();
        assert!(capacity.reserve().is_none());
        drop(permits);
        let first = capacity.reserve_tenant("first", 1).unwrap();
        assert!(capacity.reserve_tenant("first", 1).is_none());
        assert!(capacity.reserve_tenant("disabled", 0).is_none());
        let second = capacity.reserve_tenant("second", 1).unwrap();
        drop((first, second));
        assert!(capacity.tenants.lock().unwrap().is_empty());

        let entered = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let capacity = capacity.clone();
            let entered = entered.clone();
            async move {
                let _global = capacity.reserve().unwrap();
                let _tenant = capacity.reserve_tenant("cancelled", 1).unwrap();
                entered.notify_one();
                std::future::pending::<()>().await;
            }
        });
        entered.notified().await;
        task.abort();
        let _ = task.await;
        assert_eq!(capacity.slots.available_permits(), 8);
        assert!(capacity.tenants.lock().unwrap().is_empty());
    }
}
