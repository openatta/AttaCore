//! `LocalProcess` — a child process on this machine.

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::interface::exec::{
    ExecError, ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec,
};

pub struct LocalProcess;

#[async_trait::async_trait]
impl Process for LocalProcess {
    async fn spawn(
        &self,
        spec: ProcessSpec,
        _cancel: CancellationToken,
    ) -> Result<Box<dyn ProcessHandle>, ExecError> {
        let mut cmd = tokio::process::Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // A handle that is dropped without being waited on must not leave a
        // process behind: cancellation drops the handle, and a leaked child
        // would keep writing to a pipe nobody reads.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecError::failed(format!("{}: {e}", spec.program)))?;

        if let Some(bytes) = spec.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(&bytes).await;
                // Dropped here rather than at the end of the call: a child
                // reading to EOF would otherwise wait for a pipe that stays
                // open as long as this handle does.
                drop(stdin);
            }
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(Box::new(LocalHandle {
            child,
            output: Some(interleave(stdout, stderr)),
        }))
    }
}

/// Both pipes as one tagged stream.
///
/// `select` rather than chaining, so a command that writes to stderr while
/// stdout is quiet is not held up behind it.
fn interleave(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
) -> BoxStream<'static, Result<OutputChunk, ExecError>> {
    fn chunks<R: AsyncReadExt + Unpin + Send + 'static>(
        reader: Option<R>,
        stream: OutputStream,
    ) -> BoxStream<'static, Result<OutputChunk, ExecError>> {
        let Some(reader) = reader else {
            return futures::stream::empty().boxed();
        };
        // The state goes to `None` after a read error, which ends the stream.
        // Yielding the error and staying alive would hand a consumer that logs
        // and continues an endless supply of the same broken pipe.
        futures::stream::unfold(Some((reader, Vec::new())), move |state| async move {
            let (mut reader, mut buf) = state?;
            buf.resize(8192, 0);
            match reader.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    let chunk = OutputChunk {
                        stream,
                        bytes: std::mem::take(&mut buf),
                    };
                    Some((Ok(chunk), Some((reader, buf))))
                }
                Err(e) => Some((Err(ExecError::failed(e)), None)),
            }
        })
        .boxed()
    }
    futures::stream::select(
        chunks(stdout, OutputStream::Stdout),
        chunks(stderr, OutputStream::Stderr),
    )
    .boxed()
}

struct LocalHandle {
    child: tokio::process::Child,
    output: Option<BoxStream<'static, Result<OutputChunk, ExecError>>>,
}

impl ProcessHandle for LocalHandle {
    fn output(&mut self) -> Option<BoxStream<'static, Result<OutputChunk, ExecError>>> {
        self.output.take()
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.child.kill().await;
        })
    }

    fn wait(&mut self) -> BoxFuture<'_, Result<ExitStatus, ExecError>> {
        Box::pin(async move {
            let s = self.child.wait().await.map_err(ExecError::failed)?;
            Ok(ExitStatus {
                code: s.code(),
                success: s.success(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(spec: ProcessSpec) -> (String, String, ExitStatus) {
        let mut h = LocalProcess
            .spawn(spec, CancellationToken::new())
            .await
            .unwrap();
        let mut out = h.output().expect("first call yields the stream");
        let (mut o, mut e) = (Vec::new(), Vec::new());
        while let Some(chunk) = out.next().await {
            let c = chunk.unwrap();
            match c.stream {
                OutputStream::Stdout => o.extend(c.bytes),
                OutputStream::Stderr => e.extend(c.bytes),
            }
        }
        let status = h.wait().await.unwrap();
        (
            String::from_utf8_lossy(&o).into_owned(),
            String::from_utf8_lossy(&e).into_owned(),
            status,
        )
    }

    fn sh(command: &str) -> ProcessSpec {
        ProcessSpec::new("bash", std::env::temp_dir()).args(["-c".to_string(), command.to_string()])
    }

    #[tokio::test]
    async fn both_pipes_arrive_tagged_and_the_exit_status_comes_back() {
        let (out, err, status) = run(sh("echo hi; echo bad >&2; exit 3")).await;
        assert_eq!(out.trim(), "hi");
        assert_eq!(err.trim(), "bad");
        assert_eq!(status.code, Some(3));
        assert!(!status.success);
    }

    #[tokio::test]
    async fn the_stream_is_taken_once() {
        let mut h = LocalProcess
            .spawn(sh("true"), CancellationToken::new())
            .await
            .unwrap();
        assert!(h.output().is_some());
        assert!(h.output().is_none(), "the stream owns the pipes");
        let _ = h.wait().await;
    }

    #[tokio::test]
    async fn stdin_reaches_the_child_and_then_ends() {
        let mut spec = sh("cat");
        spec.stdin = Some(b"fed in".to_vec());
        let (out, _, status) = run(spec).await;
        assert_eq!(out, "fed in");
        assert!(
            status.success,
            "a child reading to EOF has to see the pipe close"
        );
    }

    /// Output arrives while the process is still running, which is the
    /// property a whole-output provider would silently remove.
    #[tokio::test]
    async fn output_arrives_before_the_process_exits() {
        let mut h = LocalProcess
            .spawn(
                sh("echo first; sleep 5; echo never"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut out = h.output().unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), out.next())
            .await
            .expect("the first chunk must not wait for the exit")
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&first.bytes).trim(), "first");
        h.kill().await;
    }
}
