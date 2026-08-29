//! Background mux process. The TUI attaches; this owns the PTYs.

use crate::agents::ToolName;
use crate::attach;
use crate::control;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn pid_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("daemon.pid")
}

pub fn log_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("daemon.log")
}

#[cfg(unix)]
pub fn is_running() -> bool {
    use std::os::unix::net::UnixStream;
    UnixStream::connect(control::mux_sock_path()).is_ok()
}

#[cfg(not(unix))]
pub fn is_running() -> bool {
    false
}

pub fn become_session_leader() {
    #[cfg(unix)]
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

pub fn write_pid() {
    let path = pid_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, std::process::id().to_string());
}

pub fn clear_pid() {
    let _ = std::fs::remove_file(pid_path());
}

#[cfg(unix)]
pub fn spawn_daemon(tool: ToolName) -> anyhow::Result<()> {
    if is_running() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let log = log_path();
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let logf = std::fs::File::create(&log)?;
    let errf = logf.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__daemon").arg("--tool").arg(tool.slug());
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(logf));
    cmd.stderr(Stdio::from(errf));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    for _ in 0..50 {
        if is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "ah daemon did not start; see {}",
        log.display()
    )
}

#[cfg(not(unix))]
pub fn spawn_daemon(_tool: ToolName) -> anyhow::Result<()> {
    anyhow::bail!("ah server needs macOS or Linux")
}

pub fn stop() -> anyhow::Result<()> {
    if !is_running() {
        println!("No ah server.");
        return Ok(());
    }
    let _ = control::request("stop", None, None, None);
    for _ in 0..30 {
        if !is_running() {
            clear_pid();
            let _ = std::fs::remove_file(attach::attach_sock_path());
            println!("Stopped.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Ok(text) = std::fs::read_to_string(pid_path()) {
        if let Ok(pid) = text.trim().parse::<i32>() {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            let _ = pid;
        }
    }
    clear_pid();
    let _ = std::fs::remove_file(control::mux_sock_path());
    let _ = std::fs::remove_file(attach::attach_sock_path());
    control::clear_live();
    println!("Stopped.");
    Ok(())
}

pub fn status() {
    if is_running() {
        if let Some(live) = control::read_live() {
            let n = live.agents.len();
            println!("ah server is running ({n} agent{}).", if n == 1 { "" } else { "s" });
            for a in &live.agents {
                let mark = if a.focused { '*' } else { ' ' };
                println!("{mark} {:>2}  {:<22} {:<8} {}", a.index, a.name, a.status, a.tool);
            }
        } else {
            println!("ah server is running.");
        }
    } else {
        println!("No ah server.");
    }
}
