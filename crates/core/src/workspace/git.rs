use anyhow::Result;
use std::path::Path;

use protocol::{BranchInfo, ChangedFile, CommitInfo, GitState, RemoteBranchInfo, SshTarget, TagInfo};

use super::ssh;

// ---------------------------------------------------------------------------
// Public entry point — dispatches to SSH-batched or local parallel path
// ---------------------------------------------------------------------------

pub async fn refresh_git(repo: &Path, ssh: Option<&SshTarget>) -> Result<GitState> {
    match ssh {
        Some(target) => refresh_git_ssh(repo, target).await,
        None => refresh_git_local(repo).await,
    }
}

// ---------------------------------------------------------------------------
// SSH batched path — single SSH process for all 7 queries
// ---------------------------------------------------------------------------

async fn refresh_git_ssh(repo: &Path, target: &SshTarget) -> Result<GitState> {
    let format = "%h\x1f%s\x1f%an\x1f%cr";
    let commands: Vec<String> = vec![
        // 0: branch
        "git rev-parse --abbrev-ref HEAD 2>/dev/null || echo ''".to_string(),
        // 1: status
        "git status --porcelain=v1 2>/dev/null || echo ''".to_string(),
        // 2: upstream name
        "git rev-parse --abbrev-ref --symbolic-full-name @{upstream} 2>/dev/null || echo ''"
            .to_string(),
        // 3: ahead/behind
        "git rev-list --left-right --count HEAD...@{upstream} 2>/dev/null || echo ''".to_string(),
        // 4: recent commits
        format!(
            "git log -20 --format='{}' 2>/dev/null || echo ''",
            format
        ),
        // 5: local branches
        "git for-each-ref --format='%(HEAD) %(refname:short) %(upstream:track)' refs/heads/ 2>/dev/null || echo ''".to_string(),
        // 6: remote branches
        "git for-each-ref --format='%(refname:short)' refs/remotes/ 2>/dev/null || echo ''"
            .to_string(),
        // 7: tags
        "git tag --sort=-creatordate --format='%(refname:short)\x1f%(objectname:short)\x1f%(creatordate:relative)' 2>/dev/null | head -20 || echo ''".to_string(),
    ];

    let out = ssh::build_batch_command(target, repo, &commands)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let sections: Vec<&str> = stdout.split(ssh::BATCH_DELIM).collect();

    // Helper: get section by index, trimmed
    let section = |i: usize| -> &str {
        sections.get(i).map(|s| s.trim()).unwrap_or("")
    };

    let branch = parse_branch_output(section(0));
    let changed = parse_status_output(section(1));
    let upstream = parse_upstream_name(section(2));
    let (ahead, behind) = if upstream.is_some() {
        parse_ahead_behind(section(3))
    } else {
        (None, None)
    };
    let recent_commits = parse_commits_output(section(4));
    let local_branches = parse_local_branches_output(section(5));
    let remote_branches = parse_remote_branches_output(section(6));
    let tags = parse_tags_output(section(7));

    Ok(GitState {
        branch,
        upstream,
        ahead,
        behind,
        changed,
        recent_commits,
        local_branches,
        remote_branches,
        tags,
    })
}

// ---------------------------------------------------------------------------
// Local parallel path — unchanged logic, uses tokio::join!
// ---------------------------------------------------------------------------

async fn refresh_git_local(repo: &Path) -> Result<GitState> {
    let branch_fut = ssh::build_command(None, repo, "git", &["rev-parse", "--abbrev-ref", "HEAD"])
        .output();

    let status_fut =
        ssh::build_command(None, repo, "git", &["status", "--porcelain=v1"]).output();

    let upstream_fut = get_upstream_status(repo);
    let commits_fut = get_recent_commits(repo, 20);
    let local_branches_fut = get_local_branches(repo);
    let remote_branches_fut = get_remote_branches(repo);
    let tags_fut = get_tags(repo);

    let (branch_out, status_out, (upstream, ahead, behind), recent_commits, local_branches, remote_branches, tags) =
        tokio::join!(branch_fut, status_fut, upstream_fut, commits_fut, local_branches_fut, remote_branches_fut, tags_fut);

    let branch = match branch_out {
        Ok(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        }
        _ => None,
    };

    let mut changed = Vec::new();
    if let Ok(out) = status_out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(file) = parse_porcelain_line(line) {
                    changed.push(file);
                }
            }
        }
    }

    Ok(GitState {
        branch,
        upstream,
        ahead,
        behind,
        changed,
        recent_commits,
        local_branches,
        remote_branches,
        tags,
    })
}

// ---------------------------------------------------------------------------
// Pure parsing functions (shared by both paths)
// ---------------------------------------------------------------------------

fn parse_branch_output(output: &str) -> Option<String> {
    let s = output.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn parse_status_output(output: &str) -> Vec<ChangedFile> {
    output
        .lines()
        .filter_map(parse_porcelain_line)
        .collect()
}

fn parse_upstream_name(output: &str) -> Option<String> {
    let s = output.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn parse_ahead_behind(output: &str) -> (Option<u32>, Option<u32>) {
    let text = output.trim();
    let parts: Vec<&str> = text.split('\t').collect();
    if parts.len() == 2 {
        let a = parts[0].parse::<u32>().unwrap_or(0);
        let b = parts[1].parse::<u32>().unwrap_or(0);
        (Some(a), Some(b))
    } else {
        (None, None)
    }
}

fn parse_commits_output(output: &str) -> Vec<CommitInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '\x1f').collect();
            if parts.len() == 4 {
                Some(CommitInfo {
                    hash: parts[0].to_string(),
                    message: parts[1].to_string(),
                    author: parts[2].to_string(),
                    date: parts[3].to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn parse_tags_output(output: &str) -> Vec<TagInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(3, '\x1f').collect();
            if parts.len() == 3 {
                Some(TagInfo {
                    name: parts[0].to_string(),
                    hash: parts[1].to_string(),
                    date: parts[2].to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn parse_local_branches_output(output: &str) -> Vec<BranchInfo> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.trim().is_empty() {
                return None;
            }
            let is_head = line.starts_with('*');
            let rest = &line[2..];
            let (name, track) = if let Some(bracket_start) = rest.find('[') {
                let name = rest[..bracket_start].trim().to_string();
                let track_str = &rest[bracket_start..];
                let (ahead, behind) = parse_track_info(track_str);
                (name, (ahead, behind))
            } else {
                (rest.trim().to_string(), (None, None))
            };
            if name.is_empty() {
                return None;
            }
            Some(BranchInfo {
                name,
                is_head,
                ahead: track.0,
                behind: track.1,
            })
        })
        .collect()
}

fn parse_remote_branches_output(output: &str) -> Vec<RemoteBranchInfo> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim().ends_with("/HEAD"))
        .map(|line| RemoteBranchInfo {
            full_name: line.trim().to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Local-only async helpers (used by refresh_git_local)
// ---------------------------------------------------------------------------

async fn get_upstream_status(
    repo: &Path,
) -> (Option<String>, Option<u32>, Option<u32>) {
    let upstream_out = ssh::build_command(
        None,
        repo,
        "git",
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .output()
    .await;

    let upstream = match upstream_out {
        Ok(out) if out.status.success() => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if name.is_empty() {
                return (None, None, None);
            }
            Some(name)
        }
        _ => return (None, None, None),
    };

    let count_out = ssh::build_command(
        None,
        repo,
        "git",
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )
    .output()
    .await;

    let (ahead, behind) = match count_out {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let parts: Vec<&str> = text.trim().split('\t').collect();
            if parts.len() == 2 {
                let a = parts[0].parse::<u32>().unwrap_or(0);
                let b = parts[1].parse::<u32>().unwrap_or(0);
                (Some(a), Some(b))
            } else {
                (Some(0), Some(0))
            }
        }
        _ => (None, None),
    };

    (upstream, ahead, behind)
}

async fn get_recent_commits(repo: &Path, count: usize) -> Vec<CommitInfo> {
    let format = "%h\x1f%s\x1f%an\x1f%cr";
    let count_arg = format!("-{count}");
    let format_arg = format!("--format={format}");
    let out = ssh::build_command(None, repo, "git", &["log", &count_arg, &format_arg])
        .output()
        .await;

    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }

    parse_commits_output(&String::from_utf8_lossy(&out.stdout))
}

async fn get_local_branches(repo: &Path) -> Vec<BranchInfo> {
    let out = ssh::build_command(
        None,
        repo,
        "git",
        &[
            "for-each-ref",
            "--format=%(HEAD) %(refname:short) %(upstream:track)",
            "refs/heads/",
        ],
    )
    .output()
    .await;

    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }

    parse_local_branches_output(&String::from_utf8_lossy(&out.stdout))
}

async fn get_remote_branches(repo: &Path) -> Vec<RemoteBranchInfo> {
    let out = ssh::build_command(
        None,
        repo,
        "git",
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes/"],
    )
    .output()
    .await;

    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }

    parse_remote_branches_output(&String::from_utf8_lossy(&out.stdout))
}

async fn get_tags(repo: &Path) -> Vec<TagInfo> {
    let format_arg = "--format=%(refname:short)\x1f%(objectname:short)\x1f%(creatordate:relative)";
    let out = ssh::build_command(
        None,
        repo,
        "git",
        &["tag", "--sort=-creatordate", format_arg],
    )
    .output()
    .await;

    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }

    let text = String::from_utf8_lossy(&out.stdout);
    parse_tags_output(&text)
        .into_iter()
        .take(20)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn parse_porcelain_line(line: &str) -> Option<ChangedFile> {
    if line.len() < 3 {
        return None;
    }

    let bytes = line.as_bytes();
    let index_status = bytes[0] as char;
    let worktree_status = bytes[1] as char;
    let path = line[3..].trim().to_string();
    if path.is_empty() {
        return None;
    }

    Some(ChangedFile {
        path,
        index_status,
        worktree_status,
    })
}

fn parse_track_info(info: &str) -> (Option<u32>, Option<u32>) {
    // Parses "[ahead N]", "[behind N]", "[ahead N, behind M]", or "[gone]"
    let trimmed = info.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed == "gone" || trimmed.is_empty() {
        return (None, None);
    }
    let mut ahead = None;
    let mut behind = None;
    for part in trimmed.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.trim().parse::<u32>().ok();
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.trim().parse::<u32>().ok();
        }
    }
    (ahead, behind)
}

// ---------------------------------------------------------------------------
// Public single-command operations (unchanged — these are user-initiated)
// ---------------------------------------------------------------------------

pub async fn diff_file(repo: &Path, file: &str, ssh: Option<&SshTarget>) -> Result<String> {
    let out = ssh::build_command(ssh, repo, "git", &["diff", "--", file])
        .output()
        .await?;

    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !text.trim().is_empty() {
        return Ok(text);
    }

    let tracked = ssh::build_command(ssh, repo, "git", &["ls-files", "--error-unmatch", "--", file])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if tracked {
        return Ok(text);
    }

    // For untracked files, check existence and read content
    if ssh.is_some() {
        // SSH: use remote commands to check and read
        let exists_out = ssh::build_command(ssh, repo, "test", &["-e", file])
            .output()
            .await;
        if !exists_out.map(|o| o.status.success()).unwrap_or(false) {
            return Ok(text);
        }

        let is_dir_out = ssh::build_command(ssh, repo, "test", &["-d", file])
            .output()
            .await;
        if is_dir_out.map(|o| o.status.success()).unwrap_or(false) {
            return Ok(format!(
                "Untracked directory: {file}\n(no file-level diff available)\n"
            ));
        }

        let cat_out = ssh::build_command(ssh, repo, "cat", &[file])
            .output()
            .await?;
        let bytes = cat_out.stdout;
        if bytes.iter().any(|b| *b == 0) {
            return Ok(format!("Binary file added: {file}\n"));
        }

        let mut diff = String::new();
        diff.push_str(&format!("diff --git a/{file} b/{file}\n"));
        diff.push_str("new file mode 100644\n");
        diff.push_str("--- /dev/null\n");
        diff.push_str(&format!("+++ b/{file}\n"));
        diff.push_str("@@ -0,0 +1 @@\n");
        for line in String::from_utf8_lossy(&bytes).lines() {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
        Ok(diff)
    } else {
        // Local: use filesystem directly
        let full_path = repo.join(file);
        if !full_path.exists() {
            return Ok(text);
        }
        if full_path.is_dir() {
            return Ok(format!(
                "Untracked directory: {file}\n(no file-level diff available)\n"
            ));
        }

        let bytes = std::fs::read(&full_path)?;
        if bytes.iter().any(|b| *b == 0) {
            return Ok(format!("Binary file added: {file}\n"));
        }

        let mut diff = String::new();
        diff.push_str(&format!("diff --git a/{file} b/{file}\n"));
        diff.push_str("new file mode 100644\n");
        diff.push_str("--- /dev/null\n");
        diff.push_str(&format!("+++ b/{file}\n"));
        diff.push_str("@@ -0,0 +1 @@\n");
        for line in String::from_utf8_lossy(&bytes).lines() {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
        Ok(diff)
    }
}

pub async fn diff_commit(repo: &Path, hash: &str, ssh: Option<&SshTarget>) -> Result<String> {
    let out = ssh::build_command(ssh, repo, "git", &["show", hash, "--format="])
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub async fn stage_file(repo: &Path, file: &str, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["add", "--", file])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn unstage_file(repo: &Path, file: &str, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["reset", "HEAD", "--", file])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git reset failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn stage_all(repo: &Path, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["add", "-A"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git add -A failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn unstage_all(repo: &Path, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["reset", "HEAD"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git reset failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn create_branch(repo: &Path, branch: &str, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["checkout", "-b", branch])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git checkout -b failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn checkout_branch(repo: &Path, branch: &str, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["checkout", branch])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn checkout_remote_branch(
    repo: &Path,
    remote_branch: &str,
    local_name: &str,
    ssh: Option<&SshTarget>,
) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["checkout", "-b", local_name, remote_branch])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn git_push(repo: &Path, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["push", "-u", "origin", "HEAD"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("git push failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

pub async fn git_pull(repo: &Path, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["pull"])
        .output()
        .await?;
    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let needs_stash = stderr.contains("Please commit your changes or stash them")
        || stderr.contains("Your local changes to the following files would be overwritten");

    if !needs_stash {
        anyhow::bail!("git pull failed: {}", stderr);
    }

    // Auto stash-pull-pop
    let stash_out = ssh::build_command(ssh, repo, "git", &["stash"])
        .output()
        .await?;
    if !stash_out.status.success() {
        anyhow::bail!(
            "git stash failed before pull: {}",
            String::from_utf8_lossy(&stash_out.stderr)
        );
    }

    let pull_out = ssh::build_command(ssh, repo, "git", &["pull"])
        .output()
        .await?;
    if !pull_out.status.success() {
        // Try to restore stash even if pull fails
        let _ = ssh::build_command(ssh, repo, "git", &["stash", "pop"])
            .output()
            .await;
        anyhow::bail!(
            "git pull failed after stash: {}",
            String::from_utf8_lossy(&pull_out.stderr)
        );
    }

    let pop_out = ssh::build_command(ssh, repo, "git", &["stash", "pop"])
        .output()
        .await?;
    if !pop_out.status.success() {
        anyhow::bail!(
            "Pulled successfully but stash pop had conflicts: {}",
            String::from_utf8_lossy(&pop_out.stderr)
        );
    }

    Ok(())
}

pub async fn git_fetch(repo: &Path, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["fetch"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("git fetch failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

pub async fn commit(repo: &Path, message: &str, ssh: Option<&SshTarget>) -> Result<()> {
    let out = ssh::build_command(ssh, repo, "git", &["commit", "-m", message])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub async fn discard_file(
    repo: &Path,
    file: &str,
    index_status: char,
    worktree_status: char,
    ssh: Option<&SshTarget>,
) -> Result<()> {
    if index_status == '?' && worktree_status == '?' {
        // Untracked file — remove it
        let out = ssh::build_command(ssh, repo, "rm", &["-rf", "--", file])
            .output()
            .await?;
        if !out.status.success() {
            anyhow::bail!("rm failed: {}", String::from_utf8_lossy(&out.stderr));
        }
    } else {
        // If staged, unstage first
        if index_status != ' ' && index_status != '?' {
            let out = ssh::build_command(ssh, repo, "git", &["reset", "HEAD", "--", file])
                .output()
                .await?;
            if !out.status.success() {
                anyhow::bail!(
                    "git reset failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        // Restore working tree
        let out = ssh::build_command(ssh, repo, "git", &["checkout", "--", file])
            .output()
            .await?;
        if !out.status.success() {
            anyhow::bail!(
                "git checkout failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(())
}

pub async fn git_stash(repo: &Path, message: Option<&str>, ssh: Option<&SshTarget>) -> Result<()> {
    let out = if let Some(msg) = message.filter(|m| !m.trim().is_empty()) {
        ssh::build_command(ssh, repo, "git", &["stash", "push", "-m", msg])
            .output()
            .await?
    } else {
        ssh::build_command(ssh, repo, "git", &["stash", "push"])
            .output()
            .await?
    };
    if !out.status.success() {
        anyhow::bail!(
            "git stash failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}
