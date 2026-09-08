//! Private compiled-artifact storage with a byte budget. Never store tenant cwasm.
use std::{
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
    if incoming > budget {
        return Err(io::Error::other("Compiled artifact exceeds cache budget"));
    }
    let mut files: Vec<(SystemTime, PathBuf, u64)> = Vec::new();
    let mut total = incoming;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.is_file() && owned_file(&entry.path()) {
            total = total.saturating_add(meta.len());
            files.push((meta.modified()?, entry.path(), meta.len()));
        }
    }
    let mut count = files.len();
    let max_count = DISK_ENTRIES - usize::from(incoming > 0);
    files.sort_by_key(|item| item.0);
    for (_, path, size) in files {
        if total <= budget && count <= max_count {
            break;
        }
        std::fs::remove_file(path)?;
        total = total.saturating_sub(size);
        count -= 1;
    }
    Ok(())
}

pub(crate) fn write(dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    prune(dir, bytes.len() as u64, DISK_BUDGET)?;
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
