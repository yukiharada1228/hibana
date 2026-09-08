//! Bounded compilation in a credential-free subprocess. This is resource isolation,
//! not a separate tenant security boundary (the child still has the Worker's UID).
use anyhow::{ensure, Context};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{oneshot, OwnedSemaphorePermit},
};

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
    permit: OwnedSemaphorePermit,
    active: prometheus::IntGauge,
) -> anyhow::Result<Vec<u8>> {
    let mut command = isolated_command(std::env::current_exe()?.as_os_str());
    command
        .arg(FLAG)
        .arg(limits.memory_mib.to_string())
        .arg(limits.timeout_secs.to_string());
    supervise(
        command,
        bytes,
        Duration::from_secs(limits.timeout_secs),
        permit,
        active,
    )
    .await
}

fn isolated_command(executable: &std::ffi::OsStr) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .current_dir("/")
        .env("RAYON_NUM_THREADS", "1");
    command
}

async fn supervise(
    mut command: Command,
    bytes: Vec<u8>,
    deadline: Duration,
    permit: OwnedSemaphorePermit,
    active: prometheus::IntGauge,
) -> anyhow::Result<Vec<u8>> {
    ensure!(bytes.len() <= MAX_INPUT, "Compiler input exceeds 32 MiB");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Cannot start compiler")?;
    let mut stdin = child.stdin.take().context("Compiler stdin missing")?;
    let stdout = child.stdout.take().context("Compiler stdout missing")?;
    let (mut sender, receiver) = oneshot::channel();
    // This supervisor outlives a cancelled invocation. Never release the slot
    // until the child has been killed and reaped.
    tokio::spawn(async move {
        let _permit = permit;
        let _active = Active::new(active);
        let work = async {
            let writer = async move {
                stdin.write_all(&bytes).await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            };
            let reader = async {
                let mut output = Vec::new();
                stdout
                    .take((MAX_OUTPUT + 1) as u64)
                    .read_to_end(&mut output)
                    .await?;
                ensure!(
                    output.len() <= MAX_OUTPUT,
                    "Compiler output exceeds 128 MiB"
                );
                Ok::<_, anyhow::Error>(output)
            };
            let (_, output) =
                tokio::try_join!(async { writer.await.map_err(anyhow::Error::from) }, reader)?;
            let status = child.wait().await?;
            ensure!(
                status.success(),
                "Compiler exited abnormally (invalid component or resource limit)"
            );
            ensure!(!output.is_empty(), "Compiler returned an empty artifact");
            Ok(output)
        };
        let result = tokio::select! {
            _ = sender.closed() => Err(anyhow::anyhow!("Compilation cancelled")),
            result = tokio::time::timeout(deadline, work) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("Compilation deadline exceeded"))),
        };
        if result.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let _ = sender.send(result);
    });
    receiver.await.context("Compiler supervisor stopped")?
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
    use std::sync::Arc;
    fn metric() -> prometheus::IntGauge {
        prometheus::IntGauge::new("test_compile", "test").unwrap()
    }

    #[tokio::test]
    async fn compiler_command_does_not_inherit_parent_environment() {
        let output = isolated_command(std::ffi::OsStr::new("/usr/bin/env"))
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"RAYON_NUM_THREADS=1\n");
    }

    #[tokio::test]
    async fn timeout_reaps_child_before_releasing_capacity() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let result = supervise(
            command,
            vec![],
            Duration::from_millis(40),
            slots.clone().acquire_owned().await.unwrap(),
            metric(),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("deadline"));
        tokio::task::yield_now().await;
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancellation_reaps_child_and_releases_capacity() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let active = metric();
        let task = tokio::spawn(supervise(
            command,
            vec![],
            Duration::from_secs(30),
            slots.clone().acquire_owned().await.unwrap(),
            active.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while active.get() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        let _permit = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.get(), 0);
    }

    #[tokio::test]
    async fn nonzero_exit_cannot_supply_a_trusted_artifact() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf untrusted; exit 1"]);
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        assert!(supervise(
            command,
            vec![],
            Duration::from_secs(2),
            slots.acquire_owned().await.unwrap(),
            metric()
        )
        .await
        .is_err());
    }
}
