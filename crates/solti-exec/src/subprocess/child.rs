//! Child-process handle shared by the portable and macOS spawn paths.

use std::{
    io,
    pin::Pin,
    process::ExitStatus,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, ReadBuf};

/// Type-erased stdout or stderr reader owned by one child.
pub(super) struct ChildOutput(Pin<Box<dyn AsyncRead + Send>>);

impl ChildOutput {
    pub(super) fn new(reader: impl AsyncRead + Send + 'static) -> Self {
        Self(Box::pin(reader))
    }
}

impl AsyncRead for ChildOutput {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.0.as_mut().poll_read(cx, buffer)
    }
}

/// Child handle independent of the primitive used to create the process.
pub(super) struct ProcessChild {
    inner: ProcessChildInner,
}

enum ProcessChildInner {
    Tokio(tokio::process::Child),
    #[cfg(target_os = "macos")]
    Macos(MacosChild),
}

impl From<tokio::process::Child> for ProcessChild {
    fn from(child: tokio::process::Child) -> Self {
        Self {
            inner: ProcessChildInner::Tokio(child),
        }
    }
}

impl ProcessChild {
    #[cfg(target_os = "macos")]
    pub(super) fn from_macos(
        pid: libc::pid_t,
        stdout: impl AsyncRead + Send + 'static,
        stderr: impl AsyncRead + Send + 'static,
    ) -> Self {
        Self {
            inner: ProcessChildInner::Macos(MacosChild {
                pid: Some(pid),
                status: None,
                stdout: Some(ChildOutput::new(stdout)),
                stderr: Some(ChildOutput::new(stderr)),
            }),
        }
    }

    pub(super) fn id(&self) -> Option<u32> {
        match &self.inner {
            ProcessChildInner::Tokio(child) => child.id(),
            #[cfg(target_os = "macos")]
            ProcessChildInner::Macos(child) => child.pid.map(|pid| pid as u32),
        }
    }

    pub(super) fn take_stdout(&mut self) -> Option<ChildOutput> {
        match &mut self.inner {
            ProcessChildInner::Tokio(child) => child.stdout.take().map(ChildOutput::new),
            #[cfg(target_os = "macos")]
            ProcessChildInner::Macos(child) => child.stdout.take(),
        }
    }

    pub(super) fn take_stderr(&mut self) -> Option<ChildOutput> {
        match &mut self.inner {
            ProcessChildInner::Tokio(child) => child.stderr.take().map(ChildOutput::new),
            #[cfg(target_os = "macos")]
            ProcessChildInner::Macos(child) => child.stderr.take(),
        }
    }

    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        match &mut self.inner {
            ProcessChildInner::Tokio(child) => child.start_kill(),
            #[cfg(target_os = "macos")]
            ProcessChildInner::Macos(child) => child.start_kill(),
        }
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.inner {
            ProcessChildInner::Tokio(child) => child.try_wait(),
            #[cfg(target_os = "macos")]
            ProcessChildInner::Macos(child) => child.try_wait(),
        }
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.inner {
            ProcessChildInner::Tokio(child) => child.wait().await,
            #[cfg(target_os = "macos")]
            ProcessChildInner::Macos(child) => child.wait().await,
        }
    }
}

#[cfg(target_os = "macos")]
struct MacosChild {
    pid: Option<libc::pid_t>,
    status: Option<ExitStatus>,
    stdout: Option<ChildOutput>,
    stderr: Option<ChildOutput>,
}

#[cfg(target_os = "macos")]
impl MacosChild {
    fn start_kill(&mut self) -> io::Result<()> {
        let Some(pid) = self.pid else {
            return Ok(());
        };
        // SAFETY: `pid` identifies the child still owned by this handle.
        if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        use std::os::unix::process::ExitStatusExt as _;

        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let Some(pid) = self.pid else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "macOS subprocess has no process id",
            ));
        };

        let mut raw_status = 0;
        // SAFETY: `pid` is owned by this handle and `raw_status` is writable.
        let result = unsafe { libc::waitpid(pid, &mut raw_status, libc::WNOHANG) };
        if result == 0 {
            return Ok(None);
        }
        if result < 0 {
            return Err(io::Error::last_os_error());
        }

        let status = ExitStatus::from_raw(raw_status);
        self.pid = None;
        self.status = Some(status);
        Ok(Some(status))
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigchld = signal(SignalKind::child())?;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if sigchld.recv().await.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "SIGCHLD listener closed before macOS subprocess exit",
                ));
            }
        }
    }
}
