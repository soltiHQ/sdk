use tokio::process::{Child, Command};

pub fn cmd_program(program: &str, args: &[String]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args.iter().map(|s| s.as_str()));
    cmd
}

#[cfg(target_family = "unix")]
pub async fn kill_graceful(child: &mut Child) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        if let Some(id) = child.id() {
            let _ = kill(Pid::from_raw(id as i32), Signal::SIGTERM);
        }
    }
    let _ = child.kill().await;
    Ok(())
}

#[cfg(target_family = "windows")]
pub async fn kill_graceful(child: &mut Child) -> std::io::Result<()> {
    child.kill().await
}
