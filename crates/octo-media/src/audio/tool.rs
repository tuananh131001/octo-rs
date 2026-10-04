//! Running ffmpeg or fpcalc the way the C# `Process` blocks did: from the temp folder, stdout
//! and stderr redirected, stdin inherited, and killed when the caller gives up on it.

use std::io;
use std::process::{ExitStatus, Stdio};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// What a tool left behind once it exited.
pub(crate) struct ToolRun {
    pub exit_code: i32,
    /// Empty when the run was told to discard stdout.
    pub stdout: Vec<u8>,
    /// Decoded as UTF-8 with replacement characters, as `StreamReader` does.
    pub stderr: String,
}

/// Starts `program` with `arguments` from the temp folder. An error here is the "did not start"
/// case every caller latches as "the binary is missing".
pub(crate) fn spawn(program: &str, arguments: &[&str]) -> io::Result<Child> {
    Command::new(program)
        .args(arguments)
        .current_dir(std::env::temp_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A dropped run (a timeout, a caller that went away) must not leave the tool decoding.
        .kill_on_drop(true)
        .spawn()
}

/// Reads stdout and stderr to the end side by side, then waits for the exit. `keep_stdout`
/// false copies stdout to nowhere, as `CopyToAsync(Stream.Null)` did.
pub(crate) async fn collect(child: &mut Child, keep_stdout: bool) -> io::Result<ToolRun> {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let read_stdout = async {
        let mut buffer = Vec::new();
        if keep_stdout {
            stdout.read_to_end(&mut buffer).await?;
        } else {
            tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await?;
        }
        Ok::<_, io::Error>(buffer)
    };
    let read_stderr = async {
        let mut buffer = Vec::new();
        stderr.read_to_end(&mut buffer).await?;
        Ok::<_, io::Error>(String::from_utf8_lossy(&buffer).into_owned())
    };
    let (stdout, stderr) = tokio::try_join!(read_stdout, read_stderr)?;
    let status = child.wait().await?;
    Ok(ToolRun {
        exit_code: exit_code(status),
        stdout,
        stderr,
    })
}

/// `Process.Kill(entireProcessTree: true)` when it has not exited yet, best effort. Neither
/// ffmpeg nor fpcalc starts children, so the process itself is the whole tree.
pub(crate) fn kill(child: &mut Child) {
    if let Ok(None) = child.try_wait() {
        let _ = child.start_kill();
    }
}

/// `Process.ExitCode`: the exit status, or 128 plus the signal for a process a signal ended.
fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(-1)
}
