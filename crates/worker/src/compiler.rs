//! Bounded compilation in a credential-free subprocess. This is resource isolation,
//! not a separate tenant security boundary (the child still has the Worker's UID).
use anyhow::ensure;
#[cfg(target_os = "linux")]
use anyhow::Context;
use hibana_shared::subprocess;
use std::time::Duration;
use tokio::{process::Command, sync::OwnedSemaphorePermit};

pub(crate) const FLAG: &str = "--compile-stdin";
pub(crate) const MAX_INPUT: usize = 32 * 1024 * 1024;
pub(crate) const MAX_OUTPUT: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub memory_mib: u64,
    pub timeout_secs: u64,
}

pub(crate) async fn compile(
    bytes: Vec<u8>,
    limits: Limits,
    permit: std::sync::Arc<OwnedSemaphorePermit>,
    active: prometheus::IntGauge,
) -> anyhow::Result<Vec<u8>> {
    let mut command = isolated_command(std::env::current_exe()?.as_os_str());
    command
        .arg(FLAG)
        .arg(limits.memory_mib.to_string())
        .arg(limits.timeout_secs.to_string());
    ensure!(bytes.len() <= MAX_INPUT, "Compiler input exceeds 32 MiB");
    let output = subprocess::output(
        command,
        &bytes,
        Duration::from_secs(limits.timeout_secs),
        MAX_OUTPUT,
        (permit, Active::new(active)),
    )
    .await?;
    ensure!(!output.is_empty(), "Compiler returned an empty artifact");
    Ok(output)
}

fn isolated_command(executable: &std::ffi::OsStr) -> Command {
    let mut command = subprocess::isolated_command(executable);
    command.env("RAYON_NUM_THREADS", "1");
    command
}

struct Active(prometheus::IntGauge);
impl Active {
    fn new(metric: prometheus::IntGauge) -> Self {
        metric.inc();
        Self(metric)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// Called synchronously before Tokio, tracing, database clients or settings.
pub(crate) fn run(args: &[String]) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    ensure!(args.len() == 3, "Invalid compiler arguments");
    let memory: u64 = args[1].parse()?;
    let seconds: u64 = args[2].parse()?;
    ensure!(
        (256..=4096).contains(&memory) && (1..=300).contains(&seconds),
        "Invalid compiler limits"
    );
    set_limits(memory, seconds)?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_INPUT, "Oversized compiler input");
    let output = crate::runtime::build_engine()?.precompile_component(&bytes)?;
    ensure!(output.len() <= MAX_OUTPUT, "Oversized compiler output");
    std::io::stdout().write_all(&output)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_limits(memory_mib: u64, seconds: u64) -> anyhow::Result<()> {
    for (resource, value) in [
        (libc::RLIMIT_AS, memory_mib * 1024 * 1024),
        (libc::RLIMIT_CPU, seconds),
        (libc::RLIMIT_CORE, 0),
        (libc::RLIMIT_NOFILE, 64),
        (libc::RLIMIT_FSIZE, 0),
    ] {
        let limit = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        // SAFETY: valid resource and pointer, called before threads are created.
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot apply compiler resource limits");
        }
    }
    // Fail closed if these protections cannot be installed.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
    {
        return Err(std::io::Error::last_os_error()).context("Cannot restrict compiler process");
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn set_limits(_: u64, _: u64) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compiler_command_does_not_inherit_parent_environment() {
        let output = isolated_command(std::ffi::OsStr::new("/usr/bin/env"))
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"RAYON_NUM_THREADS=1\n");
    }
}
