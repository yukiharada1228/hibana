//! Private compiled-artifact storage with a byte budget. Never store tenant cwasm.
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};
pub(crate) const DISK_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const DISK_ENTRIES: usize = 256;
pub(crate) const MEMORY_BUDGET: usize = 256 * 1024 * 1024;

fn owned_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_suffix(".cwasm"))
        .is_some_and(hibana_shared::preparation::valid_digest)
}

pub(crate) fn prune(dir: &Path, incoming: u64, budget: u64) -> io::Result<()> {
    prune_for_write(dir, incoming, budget, None)
}

fn prune_for_write(
    dir: &Path,
    incoming: u64,
    budget: u64,
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
        victims.push(path);
        total = total.saturating_sub(size);
        count -= 1;
    }
    for path in victims {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub(crate) fn write(dir: &Path, target: &Path, bytes: &[u8], budget: u64) -> io::Result<()> {
    prune_for_write(dir, bytes.len() as u64, budget, Some(target))?;
    // Stay in the private cache directory, on the target's filesystem.
    // tempfile owns creation, atomic replacement and cleanup on failure.
    let mut file = tempfile::Builder::new()
        .prefix(".compiler-")
        .tempfile_in(dir)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_artifacts_can_be_evicted_at_the_entry_limit() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        for n in 0..DISK_ENTRIES {
            std::fs::write(dir.join(format!("{n:064x}.cwasm")), [0; 8]).unwrap();
        }
        let extra = dir.join(format!("{:064x}.cwasm", DISK_ENTRIES));
        write(dir, &extra, &[1], DISK_BUDGET).unwrap();
        assert!(extra.exists());
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), DISK_ENTRIES);
        write(dir, &extra, &[2], DISK_BUDGET).unwrap();
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), DISK_ENTRIES);
    }
    #[test]
    fn byte_budget_prunes_only_owned_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let first = dir.join(format!("{}.cwasm", "a".repeat(64)));
        let second = dir.join(format!("{}.cwasm", "b".repeat(64)));
        std::fs::write(&first, [0; 8]).unwrap();
        std::fs::File::open(&first)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        std::fs::write(&second, [0; 8]).unwrap();
        std::fs::write(dir.join("unrelated"), [0; 16]).unwrap();
        prune(dir, 4, 12).unwrap();
        assert!(!first.exists());
        assert!(second.exists() && dir.join("unrelated").exists());
        assert!(prune(dir, 13, 12).is_err());
        for index in 0..DISK_ENTRIES + 1 {
            std::fs::write(dir.join(format!("{index:064x}.cwasm")), [0]).unwrap();
        }
        prune(dir, 1, 1024 * 1024).unwrap();
        let count = std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| owned_file(&e.as_ref().unwrap().path()))
            .count();
        assert_eq!(count, DISK_ENTRIES - 1);
    }

    #[test]
    fn writes_preserve_unowned_temporary_files_and_clean_up_after_failure() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let stale = dir.join(format!(".compiler-{}.tmp", std::process::id()));
        std::fs::write(&stale, b"unowned").unwrap();
        let target = dir.join(format!("{}.cwasm", "a".repeat(64)));
        // A directory cannot be atomically replaced by the compiled file.
        std::fs::create_dir(&target).unwrap();
        assert!(write(dir, &target, b"compiled", DISK_BUDGET).is_err());
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 2);
        std::fs::remove_dir(&target).unwrap();
        write(dir, &target, b"compiled", DISK_BUDGET).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"compiled");
        assert_eq!(std::fs::read(&stale).unwrap(), b"unowned");
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), 2);
    }
}
