//! Shared cache GC. Only the reserved native-cache prefix is disposable.
use super::Storage;
use hibana_shared::{compiled_cache as protocol, FaasError};
use sea_orm::{
    sea_query::{Expr, Func, Query},
    ConnectionTrait, DatabaseConnection, TransactionTrait,
};
use std::time::Duration;

struct Entry {
    key: String,
    bytes: u64,
    modified: i64,
}

impl Storage {
    pub(crate) async fn put_compiled(
        &self,
        pool: &DatabaseConnection,
        key: &str,
        bytes: Vec<u8>,
    ) -> Result<(), FaasError> {
        // One publisher across control-plane replicas; never queue cache writes.
        // This lock touches no tenant rows. Losing the DB guard cancels publication.
        tokio::time::timeout(Duration::from_secs(12), async {
            let tx = pool.begin().await.map_err(|_| FaasError::Unavailable)?;
            let row = tx
                .query_one(
                    &Query::select()
                        .expr_as(
                            Func::cust("pg_try_advisory_xact_lock")
                                .args([Expr::val(0x48494241_i32), Expr::val(1_i32)]),
                            "acquired",
                        )
                        .to_owned(),
                )
                .await
                .map_err(|_| FaasError::Unavailable)?;
            if !row
                .and_then(|r| r.try_get::<bool>("", "acquired").ok())
                .unwrap_or(false)
            {
                return Err(FaasError::Unavailable);
            }
            // Normal idle-in-transaction timeout is 15s; the whole mutation above
            // is bounded to 12s, including S3 I/O and connection acquisition.
            let entries = self.compiled_entries().await?;
            let remove = victims(
                entries,
                key,
                bytes.len() as u64,
                protocol::BUDGET_BYTES,
                protocol::MAX_ENTRIES,
            )?;
            for key in remove {
                self.delete_object(&key).await?;
            }
            self.put_object(key, bytes, "application/octet-stream")
                .await?;
            tx.commit().await.map_err(|_| FaasError::Unavailable)?;
            Ok(())
        })
        .await
        .map_err(|_| FaasError::Unavailable)?
    }

    async fn compiled_entries(&self) -> Result<Vec<Entry>, FaasError> {
        let mut entries = Vec::new();
        let mut continuation = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(protocol::PREFIX)
                .max_keys(1000)
                .set_continuation_token(continuation.clone())
                .send()
                .await
                .map_err(|_| FaasError::Unavailable)?;
            for object in page.contents() {
                let key = object.key().ok_or(FaasError::Unavailable)?;
                let suffix = key
                    .strip_prefix(protocol::PREFIX)
                    .ok_or(FaasError::Unavailable)?;
                let (runtime, wasm) = suffix.split_once('/').ok_or(FaasError::Unavailable)?;
                let wasm = wasm.strip_suffix(".cwasm").ok_or(FaasError::Unavailable)?;
                // Refuse malformed entries; never delete unrelated bucket contents.
                if !protocol::valid_id(runtime)
                    || !protocol::valid_id(wasm)
                    || entries.len() >= 4096
                {
                    return Err(FaasError::Unavailable);
                }
                entries.push(Entry {
                    key: key.into(),
                    bytes: object
                        .size()
                        .and_then(|n| u64::try_from(n).ok())
                        .ok_or(FaasError::Unavailable)?,
                    modified: object.last_modified().ok_or(FaasError::Unavailable)?.secs(),
                });
            }
            if page.is_truncated() != Some(true) {
                break;
            }
            let next = page
                .next_continuation_token()
                .ok_or(FaasError::Unavailable)?
                .to_owned();
            if continuation.as_ref() == Some(&next) {
                return Err(FaasError::Unavailable);
            }
            continuation = Some(next);
        }
        Ok(entries)
    }
}

fn victims(
    mut entries: Vec<Entry>,
    replacing: &str,
    incoming: u64,
    budget: u64,
    limit: usize,
) -> Result<Vec<String>, FaasError> {
    if incoming > budget || limit == 0 {
        return Err(FaasError::Unavailable);
    }
    entries.retain(|entry| entry.key != replacing);
    entries.sort_by(|a, b| (a.modified, &a.key).cmp(&(b.modified, &b.key)));
    let mut total = incoming.saturating_add(
        entries
            .iter()
            .fold(0u64, |sum, e| sum.saturating_add(e.bytes)),
    );
    let mut count = entries.len() + 1;
    let mut removed = Vec::new();
    for entry in entries {
        if total <= budget && count <= limit {
            break;
        }
        total -= entry.bytes;
        count -= 1;
        removed.push(entry.key);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entries() -> Vec<Entry> {
        [("new", 30, 3), ("old", 40, 1), ("middle", 20, 2)]
            .into_iter()
            .map(|(key, bytes, modified)| Entry {
                key: key.into(),
                bytes,
                modified,
            })
            .collect()
    }
    #[test]
    fn gc_bounds_bytes_and_count_without_double_counting_replacements() {
        assert_eq!(
            victims(entries(), "incoming", 60, 100, 256).unwrap(),
            ["old", "middle"]
        );
        assert_eq!(victims(entries(), "incoming", 1, 1000, 3).unwrap(), ["old"]);
        assert!(victims(entries(), "old", 45, 100, 3).unwrap().is_empty());
        assert_eq!(
            victims(entries(), "old", 100, 100, 3).unwrap(),
            ["middle", "new"]
        );
        assert!(victims(entries(), "incoming", 101, 100, 256).is_err());
    }
}
