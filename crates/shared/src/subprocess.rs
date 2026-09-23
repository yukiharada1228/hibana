//! Bounded I/O and cancellation for Hibana's validator/compiler subprocesses.
//! Resource guards stay owned until the child has exited and been reaped.
use std::{ffi::OsStr, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::oneshot,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot start subprocess")]
    Spawn(#[source] std::io::Error),
    #[error("subprocess I/O failed")]
    Io(#[from] std::io::Error),
    #[error("subprocess deadline exceeded")]
    Timeout,
    #[error("subprocess output limit exceeded")]
    OutputLimit,
    #[error("subprocess exited abnormally: {0}")]
    Exit(std::process::ExitStatus),
    #[error("subprocess supervisor stopped")]
    Stopped,
}

/// Callers explicitly add only the environment needed by the child.
pub fn isolated_command(executable: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(executable);
    command.env_clear().current_dir("/");
    command
}

pub async fn output(
    mut command: Command,
    input: &[u8],
    deadline: Duration,
    max_output: usize,
    guard: impl Send + 'static,
) -> Result<Vec<u8>, Error> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(Error::Spawn)?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (mut sender, receiver) = oneshot::channel();
    // The supervisor outlives the caller on cancellation. Input stays borrowed
    // by the caller; a large upload does not need a second allocation.
    tokio::spawn(async move {
        let work = async {
            let mut output = Vec::new();
            stdout
                .take((max_output as u64).saturating_add(1))
                .read_to_end(&mut output)
                .await?;
            if output.len() > max_output {
                return Err(Error::OutputLimit);
            }
            let status = child.wait().await?;
            if !status.success() {
                return Err(Error::Exit(status));
            }
            Ok(output)
        };
        let result = tokio::select! {
            _ = sender.closed() => Err(Error::Stopped),
            result = tokio::time::timeout(deadline, work) => result.unwrap_or(Err(Error::Timeout)),
        };
        if result.is_err() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        drop(guard);
        let _ = sender.send(result);
    });
    let writer = async move {
        let result = stdin.write_all(input).await;
        drop(stdin);
        result
    };
    let (written, result) = tokio::join!(writer, receiver);
    let output = result.map_err(|_| Error::Stopped)??;
    written?;
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    fn shell(script: &str) -> Command {
        let mut command = isolated_command("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn children_receive_only_explicit_environment() {
        let mut command = isolated_command("/usr/bin/env");
        command.env("HIBANA_TEST_CHILD", "explicit");
        assert_eq!(
            output(command, b"", Duration::from_secs(2), 1024, ())
                .await
                .unwrap(),
            b"HIBANA_TEST_CHILD=explicit\n"
        );
    }

    #[tokio::test]
    async fn drains_output_while_writing_input_larger_than_the_pipe_buffer() {
        let input = vec![b'x'; 256 * 1024];
        let output = output(
            shell("head -c 262144 /dev/zero; cat"),
            &input,
            Duration::from_secs(3),
            input.len() * 2,
            (),
        )
        .await
        .unwrap();
        assert_eq!(&output[..input.len()], vec![0; input.len()]);
        assert_eq!(&output[input.len()..], input);
    }

    #[tokio::test]
    async fn blocked_stdin_is_inside_the_deadline() {
        let slots = Arc::new(Semaphore::new(1));
        let input = vec![0; 1024 * 1024];
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            output(
                shell("exec sleep 30"),
                &input,
                Duration::from_millis(50),
                32,
                slots.clone().acquire_owned().await.unwrap(),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(Error::Timeout)));
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn excess_output_and_failed_exits_never_return_payloads() {
        let result = output(
            shell("exec yes private-fixture"),
            b"",
            Duration::from_secs(2),
            32,
            (),
        )
        .await;
        assert!(matches!(result, Err(Error::OutputLimit)));
        let result = output(
            shell("printf private-fixture; exit 7"),
            b"",
            Duration::from_secs(2),
            32,
            (),
        )
        .await;
        assert!(matches!(result, Err(Error::Exit(status)) if status.code() == Some(7)));
    }

    #[tokio::test]
    async fn cancellation_reaps_child_before_releasing_the_slot() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = shell("echo $$ > \"$1\"; exec sleep 30");
        command.arg("fixture").arg(&pid_file);
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let task =
            tokio::spawn(
                async move { output(command, b"", Duration::from_secs(30), 32, permit).await },
            );
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(value) = tokio::fs::read_to_string(&pid_file).await {
                    if let Ok(pid) = value.trim().parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        let _permit = tokio::time::timeout(Duration::from_secs(3), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        let status = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(
            !status.success(),
            "child still exists after its slot was released"
        );
    }

    #[tokio::test]
    async fn spawn_failure_releases_the_slot() {
        let slots = Arc::new(Semaphore::new(1));
        let result = output(
            isolated_command("/missing/hibana-test-program"),
            b"",
            Duration::from_secs(1),
            32,
            slots.clone().acquire_owned().await.unwrap(),
        )
        .await;
        assert!(matches!(result, Err(Error::Spawn(_))));
        assert_eq!(slots.available_permits(), 1);
    }
}
