use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    Path(String),
    Cancelled,
    Unavailable,
}

#[derive(Debug)]
pub(crate) enum PickerError {
    Io(std::io::Error),
    Timeout(tokio::time::error::Elapsed),
    Encoding(std::string::FromUtf8Error),
    Exit(std::process::ExitStatus),
}

impl std::fmt::Display for PickerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("local folder picker failed"),
            Self::Timeout(_) => formatter.write_str("local folder picker timed out; retry Browse"),
            Self::Encoding(_) => formatter.write_str("local folder picker returned invalid text"),
            Self::Exit(status) => write!(formatter, "local folder picker failed ({status})"),
        }
    }
}

impl std::error::Error for PickerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Timeout(error) => Some(error),
            Self::Encoding(error) => Some(error),
            Self::Exit(_) => None,
        }
    }
}

pub(crate) async fn pick(start: &str) -> Result<Selection, PickerError> {
    let mut command = Command::new("kdialog");
    command.arg("--getexistingdirectory");
    if !start.is_empty() {
        command.arg(start);
    }
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::other("folder picker parent exited"));
            }
            Ok(())
        });
    }
    run(command, Duration::from_secs(120)).await
}

async fn run(mut command: Command, deadline: Duration) -> Result<Selection, PickerError> {
    let mut child = match command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Selection::Unavailable)
        }
        Err(error) => return Err(PickerError::Io(error)),
    };
    let mut stdout = child.stdout.take().expect("piped child stdout").take(8193);
    let mut output = Vec::new();
    let result = tokio::time::timeout(deadline, async {
        let (status, _) = tokio::try_join!(child.wait(), stdout.read_to_end(&mut output))?;
        Ok::<_, std::io::Error>(status)
    })
    .await;
    let status = match result {
        Ok(Ok(status)) => status,
        failure => {
            match tokio::time::timeout(Duration::from_secs(2), child.kill()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "could not reap local folder picker"),
                Err(_) => tracing::warn!("local folder picker cleanup timed out"),
            }
            return Err(match failure {
                Err(source) => PickerError::Timeout(source),
                Ok(Err(source)) => PickerError::Io(source),
                Ok(Ok(_)) => unreachable!(),
            });
        }
    };
    match status.code() {
        Some(0) => {
            if output.len() > 8192 {
                return Err(PickerError::Io(std::io::Error::other(
                    "local folder picker output is too large",
                )));
            }
            let output = String::from_utf8(output).map_err(PickerError::Encoding)?;
            let path = output.strip_suffix('\n').unwrap_or(&output).to_owned();
            if path.is_empty() {
                Ok(Selection::Cancelled)
            } else {
                Ok(Selection::Path(path))
            }
        }
        Some(1) => Ok(Selection::Cancelled),
        _ => Err(PickerError::Exit(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn selection_preserves_spaces_and_distinguishes_cancel_failure_and_unavailable() {
        assert_eq!(
            run(shell("printf '/tmp/folder \\n'"), Duration::from_secs(1))
                .await
                .unwrap(),
            Selection::Path("/tmp/folder ".into())
        );
        assert_eq!(
            run(shell("exit 1"), Duration::from_secs(1)).await.unwrap(),
            Selection::Cancelled
        );
        assert!(matches!(
            run(shell("exit 2"), Duration::from_secs(1)).await,
            Err(PickerError::Exit(_))
        ));
        assert_eq!(
            run(
                Command::new("/nonexistent/g-client-picker"),
                Duration::from_secs(1)
            )
            .await
            .unwrap(),
            Selection::Unavailable
        );
        let error = run(shell("printf '\\377'"), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, PickerError::Encoding(_)));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[tokio::test]
    async fn stalled_picker_does_not_block_runtime_and_is_reaped_on_timeout() {
        let root = std::env::temp_dir().join(format!(
            "gdrive-picker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let pidfile = root.join("pid");
        let mut command = shell("echo $$ > \"$1\"; exec sleep 30");
        command.arg("fixture").arg(&pidfile);
        let started = tokio::time::Instant::now();
        let (result, heartbeat) = tokio::join!(run(command, Duration::from_millis(150)), async {
            tokio::task::yield_now().await;
            true
        },);
        assert!(heartbeat && started.elapsed() < Duration::from_secs(2));
        let error = result.unwrap_err();
        assert!(matches!(error, PickerError::Timeout(_)));
        assert!(std::error::Error::source(&error).is_some());
        let pid: libc::pid_t = std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_only_the_owned_picker() {
        let root = std::env::temp_dir().join(format!(
            "gdrive-picker-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let pidfile = root.join("pid");
        let mut command = shell("echo $$ > \"$1\"; exec sleep 30");
        command.arg("fixture").arg(&pidfile);
        let task = tokio::spawn(run(command, Duration::from_secs(5)));
        let pid: libc::pid_t = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match std::fs::read_to_string(&pidfile) {
                    Ok(text) => {
                        if let Ok(pid) = text.trim().parse() {
                            return pid;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("could not read owned picker PID: {error}"),
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let errno = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return std::io::Error::last_os_error().raw_os_error();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(errno, Some(libc::ESRCH));
        std::fs::remove_dir_all(root).unwrap();
    }
}
