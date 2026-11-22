//! Native language runner subprocess. The private cwd is not a filesystem sandbox.
use std::{
    ffi::OsString, fs::Permissions, io, os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio,
};
use tempfile::{Builder, TempDir};
use tokio::process::{Child, Command};

#[derive(Debug)]
pub struct RunnerCommand {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
}

pub(crate) struct RunnerProcess {
    pub child: Child,
    _scratch: TempDir,
}

impl RunnerCommand {
    pub(crate) fn spawn(&self) -> io::Result<RunnerProcess> {
        // Resolve relative overrides before changing the child's working directory.
        let executable = std::path::absolute(&self.executable)?;
        let scratch = Builder::new()
            .prefix("cant-runner-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir()?;
        let child = Command::new(executable)
            .args(&self.args)
            .current_dir(scratch.path())
            .env_clear()
            .env("HOME", scratch.path())
            .env("TMPDIR", scratch.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        Ok(RunnerProcess {
            child,
            _scratch: scratch,
        })
    }
}

impl RunnerProcess {
    pub async fn terminate(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn child_has_private_cwd_and_no_daemon_environment() {
        let command = RunnerCommand {
            executable: "/bin/sh".into(),
            args: ["-c", "test \"$HOME\" = \"$TMPDIR\" && test \"$HOME\" -ef . && test -z \"${USER+x}\" && printf '%s' \"$1\"", "sh", "one literal arg"].map(Into::into).into(),
        };
        let mut process = command.spawn().unwrap();
        let scratch = process._scratch.path().to_owned();
        assert_eq!(scratch.metadata().unwrap().permissions().mode() & 0o077, 0);
        let mut text = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .await
            .unwrap();
        assert!(process.child.wait().await.unwrap().success());
        assert_eq!(text, "one literal arg");
        drop(process);
        assert!(!scratch.exists());
    }
}
