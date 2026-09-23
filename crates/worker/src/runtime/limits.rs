//! Per-execution aggregate allocation limits.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};
pub(super) const MAX_TABLE_ELEMENTS: usize = 100_000;
const MAX_CORE_INSTANCES: usize = 256;
pub(crate) struct MeteredLimits {
    inner: StoreLimits,
    pub(crate) peak_memory_bytes: Arc<AtomicU64>,
    max_memory_bytes: usize,
    reserved_memory_bytes: usize,
    last_memory_growth: usize,
    reserved_table_elements: usize,
    last_table_growth: usize,
}

impl MeteredLimits {
    pub(crate) fn new(max_memory_bytes: usize, peak_memory_bytes: Arc<AtomicU64>) -> Self {
        Self {
            inner: StoreLimitsBuilder::new()
                .memory_size(max_memory_bytes)
                .table_elements(MAX_TABLE_ELEMENTS)
                .instances(MAX_CORE_INSTANCES)
                .memories(MAX_CORE_INSTANCES)
                .tables(MAX_CORE_INSTANCES)
                .build(),
            peak_memory_bytes,
            max_memory_bytes,
            reserved_memory_bytes: 0,
            last_memory_growth: 0,
            reserved_table_elements: 0,
            last_table_growth: 0,
        }
    }
}

impl ResourceLimiter for MeteredLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        self.last_memory_growth = 0;
        let growth = desired.saturating_sub(current);
        if growth
            > self
                .max_memory_bytes
                .saturating_sub(self.reserved_memory_bytes)
        {
            return Ok(false);
        }
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if allowed {
            self.reserved_memory_bytes += growth;
            self.last_memory_growth = growth;
            // A Store can contain several core memories. Account for their sum,
            // not just the largest memory. Failed allocations may overestimate peak.
            self.peak_memory_bytes
                .fetch_max(self.reserved_memory_bytes as u64, Ordering::Relaxed);
        }
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        self.last_table_growth = 0;
        let growth = desired.saturating_sub(current);
        if growth > MAX_TABLE_ELEMENTS.saturating_sub(self.reserved_table_elements) {
            return Ok(false);
        }
        let allowed = self.inner.table_growing(current, desired, maximum)?;
        if allowed {
            self.reserved_table_elements += growth;
            self.last_table_growth = growth;
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        self.reserved_memory_bytes -= std::mem::take(&mut self.last_memory_growth);
        self.inner.memory_grow_failed(error)
    }

    fn table_grow_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        self.reserved_table_elements -= std::mem::take(&mut self.last_table_growth);
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}
