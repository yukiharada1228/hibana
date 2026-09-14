//! Private compiled-artifact storage with a byte budget. Never store tenant cwasm.
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};
pub(crate) const DISK_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
const DISK_ENTRIES: usize = 256;
pub(crate) const MEMORY_BUDGET: usize = 256 * 1024 * 1024;

fn owned_file(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
        s.len() == 70 && s.ends_with(".cwasm") && s[..64].bytes().all(|b| b.is_ascii_hexdigit())
    })
}

pub(crate) fn prune(dir: &Path, incoming: u64, budget: u64) -> io::Result<()> {
    prune_unprotected(dir, incoming, budget, &HashSet::new(), None)
}

fn prune_unprotected(
    dir: &Path,
    incoming: u64,
    budget: u64,
    protected: &HashSet<String>,
    replacing: Option<&Path>,
) -> io::Result<()> {
    if incoming > budget {
        return Err(io::Error::other("Compiled artifact exceeds cache budget"));
    }
    let mut files: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
    let mut total = incoming;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.is_file() && owned_file(&entry.path()) && replacing != Some(entry.path().as_path())
        {
            total = total.saturating_add(meta.len());
            files.push((meta.modified()?, entry.path(), meta.len()));
        }
    }
    let mut count = files.len();
    let max_count = DISK_ENTRIES - usize::from(incoming > 0);
    files.sort_by_key(|item| item.0);
    let mut victims = Vec::new();
    for (_, path, size) in files {
        if total <= budget && count <= max_count {
            break;
        }
        let digest = path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
        if protected.contains(digest) {
            continue;
        }
        victims.push(path);
        total = total.saturating_sub(size);
        count -= 1;
    }
    // Refuse the new preparation without evicting anything if the retained set
    // cannot fit. In-memory LRU eviction is safe because these files stay available.
    if total > budget || count > max_count {
        return Err(io::Error::other(
            "Active artifacts fill Worker cache; deployment rejected",
        ));
    }
    for path in victims {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub(crate) fn write(
    dir: &Path,
    target: &Path,
    bytes: &[u8],
    protected: &HashSet<String>,
) -> io::Result<()> {
    prune_unprotected(
        dir,
        bytes.len() as u64,
        DISK_BUDGET,
        protected,
        Some(target),
    )?;
    // One writer per cache; create_new prevents following a pre-existing link.
    let temp = dir.join(format!(".compiler-{}.tmp", std::process::id()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, target)
    })();
    let _ = std::fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_and_reserved_artifacts_survive_count_and_byte_pressure() {
        let dir = std::env::temp_dir().join(format!(
            "hibana-retention-{}",
            hibana_shared::new_version_id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let protected: HashSet<String> = (0..DISK_ENTRIES).map(|n| format!("{n:064x}")).collect();
        for digest in &protected {
            std::fs::write(dir.join(format!("{digest}.cwasm")), [0; 8]).unwrap();
        }
        let extra = dir.join(format!("{:064x}.cwasm", DISK_ENTRIES));
        assert!(write(&dir, &extra, &[1], &protected).is_err());
        assert!(prune_unprotected(&dir, 1, (DISK_ENTRIES * 8) as u64, &protected, None).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), DISK_ENTRIES);
        assert!(!extra.exists());
        // Refreshing the same protected digest must not consume an extra entry.
        write(
            &dir,
            &dir.join(format!("{:064x}.cwasm", 0)),
            &[2; 8],
            &protected,
        )
        .unwrap();
        let mut released = protected;
        released.remove(&format!("{:064x}", 0));
        write(&dir, &extra, &[1], &released).unwrap();
        assert!(!dir.join(format!("{:064x}.cwasm", 0)).exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), DISK_ENTRIES);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn byte_budget_prunes_only_owned_regular_files() {
        let dir = std::env::temp_dir().join(format!(
            "hibana-cache-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let first = dir.join(format!("{}.cwasm", "a".repeat(64)));
        let second = dir.join(format!("{}.cwasm", "b".repeat(64)));
        std::fs::write(&first, [0; 8]).unwrap();
        std::fs::File::open(&first)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::write(&second, [0; 8]).unwrap();
        std::fs::write(dir.join("unrelated"), [0; 16]).unwrap();
        prune(&dir, 4, 12).unwrap();
        assert!(!first.exists());
        assert!(second.exists() && dir.join("unrelated").exists());
        assert!(prune(&dir, 13, 12).is_err());
        for index in 0..DISK_ENTRIES + 1 {
            std::fs::write(dir.join(format!("{index:064x}.cwasm")), [0]).unwrap();
        }
        prune(&dir, 1, 1024 * 1024).unwrap();
        let count = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| owned_file(&e.as_ref().unwrap().path()))
            .count();
        assert_eq!(count, DISK_ENTRIES - 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
