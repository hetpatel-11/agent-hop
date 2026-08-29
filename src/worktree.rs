//! First-class `git worktree` workspaces.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
}

pub fn list(repo: &str) -> anyhow::Result<Vec<Worktree>> {
    let out = Command::new("git")
        .args(["-C", repo, "worktree", "list", "--porcelain"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_porcelain(text: &str) -> Vec<Worktree> {
    let mut trees = Vec::new();
    let mut path = String::new();
    let mut head = String::new();
    let mut branch = None;
    let mut bare = false;
    let mut detached = false;
    let flush = |trees: &mut Vec<Worktree>,
                 path: &mut String,
                 head: &mut String,
                 branch: &mut Option<String>,
                 bare: &mut bool,
                 detached: &mut bool| {
        if path.is_empty() {
            return;
        }
        trees.push(Worktree {
            path: std::mem::take(path),
            head: std::mem::take(head),
            branch: branch.take(),
            bare: *bare,
            detached: *detached,
        });
        *bare = false;
        *detached = false;
    };
    for line in text.lines() {
        if line.is_empty() {
            flush(&mut trees, &mut path, &mut head, &mut branch, &mut bare, &mut detached);
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            flush(&mut trees, &mut path, &mut head, &mut branch, &mut bare, &mut detached);
            path = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.trim_start_matches("refs/heads/").to_string());
        } else if line == "bare" {
            bare = true;
        } else if line == "detached" {
            detached = true;
        }
    }
    flush(&mut trees, &mut path, &mut head, &mut branch, &mut bare, &mut detached);
    trees
}

pub fn add(repo: &str, name: &str, branch: Option<&str>, dest: Option<&str>) -> anyhow::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("worktree name is required");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        anyhow::bail!("worktree name must be a single path segment");
    }
    let dest = match dest {
        Some(p) => PathBuf::from(p),
        None => default_worktree_path(repo, name)?,
    };
    if dest.exists() {
        anyhow::bail!("path already exists: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new("git");
    cmd.args(["-C", repo, "worktree", "add"]);
    if let Some(branch) = branch.filter(|s| !s.is_empty()) {
        cmd.args(["-b", branch]);
    }
    cmd.arg(&dest);
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(dest)
}

pub fn remove(repo: &str, path: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(["-C", repo, "worktree", "remove", "--force", path])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn default_worktree_path(repo: &str, name: &str) -> anyhow::Result<PathBuf> {
    let root = Path::new(repo);
    let parent = root.parent().unwrap_or(root);
    let stem = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    Ok(parent.join(format!("{stem}-{name}")))
}

pub fn print_list(repo: &str) -> anyhow::Result<()> {
    let trees = list(repo)?;
    if trees.is_empty() {
        println!("No worktrees.");
        return Ok(());
    }
    for t in trees {
        let branch = t
            .branch
            .as_deref()
            .map(|b| format!("[{b}]"))
            .unwrap_or_else(|| if t.detached { "(detached)".into() } else { String::new() });
        println!("{:<48} {} {branch}", t.path, short_head(&t.head));
    }
    Ok(())
}

fn short_head(head: &str) -> String {
    head.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_two_worktrees() {
        let text = "\
worktree /tmp/repo
HEAD abcdef0123456789
branch refs/heads/main

worktree /tmp/repo-fix
HEAD 1111111111111111
branch refs/heads/fix
";
        let trees = parse_porcelain(text);
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[0].path, "/tmp/repo");
        assert_eq!(trees[0].branch.as_deref(), Some("main"));
        assert_eq!(trees[1].path, "/tmp/repo-fix");
        assert_eq!(trees[1].branch.as_deref(), Some("fix"));
    }

    #[test]
    fn parse_detached() {
        let text = "worktree /tmp/x\nHEAD deadbeef\ndetached\n";
        let trees = parse_porcelain(text);
        assert_eq!(trees.len(), 1);
        assert!(trees[0].detached);
        assert!(trees[0].branch.is_none());
    }
}
