//! Thin remote: local `ah` is the client; SSH starts `ah` on the host.

use std::process::Command;

/// `ssh -t <args...> -- ah` so the remote mux owns the PTYs and this
/// terminal is just the attach surface (plus local clipboard / keys).
pub fn ssh_argv(args: &[String]) -> Vec<String> {
    let mut v = vec!["ssh".into(), "-t".into()];
    if let Some(dash) = args.iter().position(|a| a == "--") {
        v.extend(args.iter().cloned());
        if args[dash + 1..].is_empty() {
            v.push("ah".into());
        }
    } else {
        v.extend(args.iter().cloned());
        v.push("--".into());
        v.push("ah".into());
    }
    v
}

pub fn run(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("ah remote needs a host, e.g. `ah remote user@host`");
    }
    if crate::agents::which("ssh").is_none() {
        anyhow::bail!("ssh is not on PATH (needed for `ah remote`)");
    }
    let argv = ssh_argv(args);
    let status = Command::new(&argv[0]).args(&argv[1..]).status()?;
    if !status.success() {
        anyhow::bail!("remote session exited {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_inserts_double_dash_and_ah() {
        assert_eq!(
            ssh_argv(&["user@host".into()]),
            vec!["ssh", "-t", "user@host", "--", "ah"]
        );
        assert_eq!(
            ssh_argv(&["-p".into(), "2222".into(), "me@box".into()]),
            vec!["ssh", "-t", "-p", "2222", "me@box", "--", "ah"]
        );
        assert_eq!(
            ssh_argv(&["user@host".into(), "--".into(), "ah".into(), "server".into()]),
            vec!["ssh", "-t", "user@host", "--", "ah", "server"]
        );
    }
}
