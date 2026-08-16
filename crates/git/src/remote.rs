//! Remotes: fetch, pull, push, and the credential plumbing they need.
//!
//! ## Why the `git` binary
//!
//! Transport is the strongest case for shelling out. `git fetch` and `git push`
//! negotiate packs, honour `url.*.insteadOf`, run `pre-push` hooks, respect
//! proxy configuration, drive `git-lfs` filters, and speak to ssh-agent — all
//! from the user's own configuration. gix implements the protocol but not that
//! surrounding contract.
//!
//! ## Credentials
//!
//! SSH remotes need nothing: the child process inherits `SSH_AUTH_SOCK` and
//! ssh-agent answers.
//!
//! HTTPS remotes need the OAuth token, and **it is passed through the
//! environment, never through argv**. Everything in `/proc/<pid>/cmdline` is
//! world-readable on Linux, so a token in a command line is visible to every
//! user on the machine. The environment block is readable only by the same
//! user, which is the best a subprocess design can do.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use crate::{GitError, Repo};

/// A credential helper that answers from `$FORQEN_TOKEN`.
///
/// Installed with `-c` for one invocation rather than written to the user's
/// config, so nothing is left behind if the process dies mid-push.
const TOKEN_HELPER: &str = "!f() { test \"$1\" = get && \
     printf 'username=x-access-token\\npassword=%s\\n' \"$FORQEN_TOKEN\"; }; f";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

impl Remote {
    /// True when this remote is reached over SSH rather than HTTPS.
    ///
    /// Covers both `ssh://host/path` and the scp-like `git@host:path`, which
    /// has no scheme and so is not recognised by URL parsing.
    pub fn is_ssh(&self) -> bool {
        let u = &self.push_url;
        u.starts_with("ssh://")
            || u.starts_with("git@")
            || (!u.contains("://") && u.contains(':') && !u.starts_with('/'))
    }
}

/// Progress reported while a transfer runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Phase name as git reports it: "Receiving objects", "Resolving deltas".
    pub phase: String,
    /// Completion percentage, when git reports one.
    pub percent: Option<u8>,
}

/// Callback for progress updates. Called from the calling thread.
pub type ProgressSink<'a> = &'a mut dyn FnMut(Progress);

pub fn list(repo: &Repo) -> Result<Vec<Remote>, GitError> {
    let out = capture(repo, &["remote", "-v"])?;
    let text = String::from_utf8_lossy(&out);

    // `remote -v` prints two lines per remote — one (fetch), one (push) — and
    // they can differ when pushurl is set.
    let mut by_name: HashMap<String, Remote> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url), Some(kind)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let entry = by_name.entry(name.to_string()).or_insert_with(|| Remote {
            name: name.to_string(),
            fetch_url: url.to_string(),
            push_url: url.to_string(),
        });
        match kind {
            "(fetch)" => entry.fetch_url = url.to_string(),
            "(push)" => entry.push_url = url.to_string(),
            _ => {}
        }
    }

    let mut remotes: Vec<_> = by_name.into_values().collect();
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(remotes)
}

pub fn add(repo: &Repo, name: &str, url: &str) -> Result<(), GitError> {
    capture(repo, &["remote", "add", name, url]).map(|_| ())
}

pub fn remove(repo: &Repo, name: &str) -> Result<(), GitError> {
    capture(repo, &["remote", "remove", name]).map(|_| ())
}

/// Fetch from a remote, or from all remotes when `remote` is `None`.
pub fn fetch(
    repo: &Repo,
    remote: Option<&str>,
    token: Option<&str>,
    progress: ProgressSink<'_>,
) -> Result<(), GitError> {
    let mut args: Vec<&str> = vec!["fetch", "--progress", "--prune"];
    match remote {
        Some(r) => args.push(r),
        None => args.push("--all"),
    }
    run_with_progress(repo, &args, token, progress)
}

/// Pull: fetch then integrate.
///
/// `rebase` chooses between merge and rebase explicitly rather than leaving it
/// to `pull.rebase`, because the button that triggers this says which one it
/// does and the config must not silently contradict the label.
pub fn pull(
    repo: &Repo,
    remote: Option<&str>,
    rebase: bool,
    token: Option<&str>,
    progress: ProgressSink<'_>,
) -> Result<(), GitError> {
    let mut args: Vec<&str> = vec!["pull", "--progress"];
    args.push(if rebase { "--rebase" } else { "--no-rebase" });
    if let Some(r) = remote {
        args.push(r);
    }
    run_with_progress(repo, &args, token, progress)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushMode {
    Normal,
    /// `--force-with-lease`: refuses if the remote moved since the last fetch.
    ///
    /// The only force offered. Plain `--force` overwrites a colleague's push
    /// with no warning; the lease turns that into an error the user can act on.
    ForceWithLease,
}

pub fn push(
    repo: &Repo,
    remote: &str,
    branch: &str,
    mode: PushMode,
    set_upstream: bool,
    token: Option<&str>,
    progress: ProgressSink<'_>,
) -> Result<(), GitError> {
    let mut args: Vec<&str> = vec!["push", "--progress"];
    if mode == PushMode::ForceWithLease {
        args.push("--force-with-lease");
    }
    if set_upstream {
        args.push("--set-upstream");
    }
    args.push(remote);
    args.push(branch);
    run_with_progress(repo, &args, token, progress)
}

/// Whether the current branch has an upstream configured.
///
/// Checked before pushing so the UI can offer "publish branch" instead of
/// letting git fail with advice about `--set-upstream`.
pub fn has_upstream(repo: &Repo, branch: &str) -> bool {
    capture(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            &format!("{branch}@{{upstream}}"),
        ],
    )
    .is_ok()
}

/// Run a transfer, streaming git's progress output to `sink`.
///
/// git writes progress to **stderr**, using `\r` to overwrite one line rather
/// than `\n`, so this reads bytes and splits on both. A line-based reader shows
/// nothing until the transfer finishes.
fn run_with_progress(
    repo: &Repo,
    args: &[&str],
    token: Option<&str>,
    sink: ProgressSink<'_>,
) -> Result<(), GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workdir);

    if let Some(t) = token {
        // Token via env, never argv — /proc/<pid>/cmdline is world-readable.
        cmd.env("FORQEN_TOKEN", t);
        cmd.arg("-c")
            .arg(format!("credential.helper={TOKEN_HELPER}"));
    }

    // Never block on an interactive prompt: with no usable credential, git
    // would otherwise sit waiting for a terminal that does not exist and the
    // operation would appear to hang.
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take().expect("stderr piped above");

    let mut reader = BufReader::new(stderr);
    let mut collected = String::new();
    let mut chunk = Vec::new();

    loop {
        chunk.clear();
        // Split on \r as well as \n: progress lines are \r-terminated.
        let n = read_until_either(&mut reader, &mut chunk)?;
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&chunk);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        collected.push_str(line);
        collected.push('\n');
        if let Some(p) = parse_progress(line) {
            sink(p);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(GitError::Walk(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or("?"),
            collected.trim()
        )));
    }
    Ok(())
}

/// `read_until` for two delimiters at once.
fn read_until_either<R: BufRead>(reader: &mut R, out: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(total);
        }
        match available.iter().position(|b| *b == b'\n' || *b == b'\r') {
            Some(i) => {
                out.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                return Ok(total + i + 1);
            }
            None => {
                out.extend_from_slice(available);
                let len = available.len();
                reader.consume(len);
                total += len;
            }
        }
    }
}

/// Extract a phase and percentage from one git progress line.
///
/// Shape: `Receiving objects:  73% (1234/1690), 1.2 MiB | 3.4 MiB/s`
fn parse_progress(line: &str) -> Option<Progress> {
    let (phase, rest) = line.split_once(':')?;
    let phase = phase.trim();
    if phase.is_empty() {
        return None;
    }

    let percent = rest
        .split_once('%')
        .and_then(|(head, _)| head.trim().rsplit(' ').next()?.parse::<u8>().ok())
        .filter(|p| *p <= 100);

    // Lines like "remote: Enumerating objects" carry no percentage but are
    // still worth showing, so a missing percentage is not a rejection.
    Some(Progress {
        phase: phase.to_string(),
        percent,
    })
}

fn capture(repo: &Repo, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(GitError::Walk(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(out.stdout)
}

/// Fetch a pull request's head into a local branch.
///
/// Uses the `refs/pull/<n>/head` refspec that GitHub publishes on the *base*
/// repository, rather than adding the contributor's fork as a remote.
///
/// That distinction matters. Adding a remote per contributor accumulates
/// remotes nobody prunes, breaks when the fork is deleted or renamed, and needs
/// separate credentials for a private fork. `refs/pull/*` lives on the
/// repository the user already has access to, so one refspec covers same-repo
/// branches and forks alike — including forks that have since been deleted,
/// where the fork remote would not resolve at all.
///
/// The local branch is namespaced (`pr/<n>`) because two open pull requests
/// from different forks routinely share a head branch name like `patch-1`.
pub fn fetch_pull_request(
    repo: &Repo,
    remote: &str,
    number: u64,
    local_branch: &str,
    token: Option<&str>,
    progress: ProgressSink<'_>,
) -> Result<(), GitError> {
    let refspec = format!("refs/pull/{number}/head:refs/heads/{local_branch}");
    // `--force` so re-fetching an updated pull request moves the local branch
    // instead of refusing on a non-fast-forward — a rebased PR is the normal
    // case, not an error.
    run_with_progress(
        repo,
        &["fetch", "--progress", "--force", remote, &refspec],
        token,
        progress,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;
    use crate::Repo;

    // --- progress parsing, no process involved ------------------------------

    #[test]
    fn parses_phase_and_percentage() {
        let p = parse_progress("Receiving objects:  73% (1234/1690), 1.2 MiB | 3.4 MiB/s").unwrap();
        assert_eq!(p.phase, "Receiving objects");
        assert_eq!(p.percent, Some(73));
    }

    #[test]
    fn parses_a_hundred_percent() {
        let p = parse_progress("Resolving deltas: 100% (500/500), done.").unwrap();
        assert_eq!(p.phase, "Resolving deltas");
        assert_eq!(p.percent, Some(100));
    }

    #[test]
    fn keeps_phases_that_have_no_percentage() {
        let p = parse_progress("remote: Enumerating objects: 42, done.").unwrap();
        assert_eq!(p.phase, "remote");
        assert_eq!(p.percent, None, "no % sign means no percentage, not zero");
    }

    #[test]
    fn ignores_lines_that_are_not_progress() {
        assert!(parse_progress("").is_none());
        assert!(parse_progress("no colon here").is_none());
        assert!(parse_progress(": leading colon").is_none());
    }

    #[test]
    fn a_nonsense_percentage_is_dropped_rather_than_shown() {
        // Guards the progress bar against a value it cannot represent.
        let p = parse_progress("Weird phase: 250% (1/1)").unwrap();
        assert_eq!(p.percent, None);
    }

    #[test]
    fn splits_on_carriage_returns_so_progress_streams() {
        // git overwrites one line with \r; a \n-only reader shows nothing until
        // the transfer ends.
        let data = b"Receiving objects:  10% (1/10)\rReceiving objects: 100% (10/10)\n";
        let mut reader = std::io::BufReader::new(&data[..]);

        let mut first = Vec::new();
        read_until_either(&mut reader, &mut first).unwrap();
        assert!(String::from_utf8_lossy(&first).contains("10%"));

        let mut second = Vec::new();
        read_until_either(&mut reader, &mut second).unwrap();
        assert!(String::from_utf8_lossy(&second).contains("100%"));
    }

    #[test]
    fn ssh_detection_covers_scp_syntax_and_urls() {
        let mk = |url: &str| Remote {
            name: "origin".into(),
            fetch_url: url.into(),
            push_url: url.into(),
        };
        assert!(mk("git@github.com:owner/repo.git").is_ssh());
        assert!(mk("ssh://git@github.com/owner/repo.git").is_ssh());
        assert!(!mk("https://github.com/owner/repo.git").is_ssh());
        assert!(!mk("/srv/git/repo.git").is_ssh(), "a local path is not ssh");
    }

    // --- against real repositories ------------------------------------------

    /// A bare repository standing in for a remote. Exercises the real transfer
    /// path with no network involved.
    fn bare_remote() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            // `-b main` matters: without it the bare repo's HEAD points at
            // `master`, so cloning after a push of `main` resolves HEAD to a
            // nonexistent ref, checks out nothing, and leaves the clone with no
            // local branch to push back.
            .args(["init", "--bare", "-q", "-b", "main"])
            .arg(dir.path())
            .output()
            .unwrap();
        dir
    }

    fn noop(_: Progress) {}

    #[test]
    fn add_list_and_remove_remotes() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        assert!(list(&repo).unwrap().is_empty());

        add(&repo, "origin", "https://example.invalid/r.git").unwrap();
        add(&repo, "upstream", "git@example.invalid:o/r.git").unwrap();

        let remotes = list(&repo).unwrap();
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin", "sorted by name");
        assert!(!remotes[0].is_ssh());
        assert!(remotes[1].is_ssh());

        remove(&repo, "upstream").unwrap();
        assert_eq!(list(&repo).unwrap().len(), 1);
    }

    #[test]
    fn push_then_fetch_round_trips_through_a_bare_remote() {
        let remote_dir = bare_remote();
        let dir = fixture(2);
        let repo = Repo::open(dir.path()).unwrap();

        add(&repo, "origin", remote_dir.path().to_str().unwrap()).unwrap();
        assert!(!has_upstream(&repo, "main"), "nothing tracked yet");

        let mut seen: Vec<Progress> = Vec::new();
        push(
            &repo,
            "origin",
            "main",
            PushMode::Normal,
            true,
            None,
            &mut |p| seen.push(p),
        )
        .unwrap();

        assert!(
            has_upstream(&repo, "main"),
            "--set-upstream must configure it"
        );

        // The remote now holds the commits.
        let out = Command::new("git")
            .arg("-C")
            .arg(remote_dir.path())
            .args(["rev-list", "--count", "main"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2");

        // A second clone can fetch them back.
        let clone_dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", "-q"])
            .arg(remote_dir.path())
            .arg(clone_dir.path())
            .output()
            .unwrap();
        let clone = Repo::open(clone_dir.path()).unwrap();
        fetch(&clone, Some("origin"), None, &mut noop).unwrap();
    }

    #[test]
    fn force_with_lease_refuses_when_the_remote_moved() {
        let remote_dir = bare_remote();

        // Two clones of the same remote.
        let a = fixture(1);
        let repo_a = Repo::open(a.path()).unwrap();
        add(&repo_a, "origin", remote_dir.path().to_str().unwrap()).unwrap();
        push(
            &repo_a,
            "origin",
            "main",
            PushMode::Normal,
            true,
            None,
            &mut noop,
        )
        .unwrap();

        let b_dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", "-q"])
            .arg(remote_dir.path())
            .arg(b_dir.path())
            .output()
            .unwrap();
        for (k, v) in [("user.name", "B"), ("user.email", "b@e.invalid")] {
            Command::new("git")
                .arg("-C")
                .arg(b_dir.path())
                .args(["config", k, v])
                .output()
                .unwrap();
        }

        // B pushes a commit A has never seen.
        std::fs::write(b_dir.path().join("b.txt"), "from b\n").unwrap();
        let repo_b = Repo::open(b_dir.path()).unwrap();
        crate::stage::stage_file(&repo_b, "b.txt").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(b_dir.path())
            .args(["commit", "-q", "-m", "b work"])
            .output()
            .unwrap();
        push(
            &repo_b,
            "origin",
            "main",
            PushMode::Normal,
            false,
            None,
            &mut noop,
        )
        .unwrap();

        // A rewrites its history and force-pushes with a lease. The lease is
        // stale, so this must be refused rather than destroying B's commit.
        std::fs::write(a.path().join("f.txt"), "rewritten\n").unwrap();
        crate::stage::stage_file(&repo_a, "f.txt").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(a.path())
            .args(["commit", "-q", "--amend", "-m", "rewritten"])
            .output()
            .unwrap();

        let result = push(
            &repo_a,
            "origin",
            "main",
            PushMode::ForceWithLease,
            false,
            None,
            &mut noop,
        );
        assert!(
            result.is_err(),
            "a stale lease must refuse, or a colleague's push is silently lost"
        );

        // B's commit survives on the remote.
        let out = Command::new("git")
            .arg("-C")
            .arg(remote_dir.path())
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("b work"),
            "the refused push must not have overwritten anything"
        );
    }

    #[test]
    fn pull_brings_down_a_commit_made_elsewhere() {
        let remote_dir = bare_remote();
        let a = fixture(1);
        let repo_a = Repo::open(a.path()).unwrap();
        add(&repo_a, "origin", remote_dir.path().to_str().unwrap()).unwrap();
        push(
            &repo_a,
            "origin",
            "main",
            PushMode::Normal,
            true,
            None,
            &mut noop,
        )
        .unwrap();

        let b_dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", "-q"])
            .arg(remote_dir.path())
            .arg(b_dir.path())
            .output()
            .unwrap();
        for (k, v) in [("user.name", "B"), ("user.email", "b@e.invalid")] {
            Command::new("git")
                .arg("-C")
                .arg(b_dir.path())
                .args(["config", k, v])
                .output()
                .unwrap();
        }
        std::fs::write(b_dir.path().join("shared.txt"), "hello\n").unwrap();
        let repo_b = Repo::open(b_dir.path()).unwrap();
        crate::stage::stage_file(&repo_b, "shared.txt").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(b_dir.path())
            .args(["commit", "-q", "-m", "from b"])
            .output()
            .unwrap();
        push(
            &repo_b,
            "origin",
            "main",
            PushMode::Normal,
            false,
            None,
            &mut noop,
        )
        .unwrap();

        pull(&repo_a, Some("origin"), false, None, &mut noop).unwrap();
        assert!(
            a.path().join("shared.txt").exists(),
            "pull should have brought the file down"
        );
    }

    #[test]
    fn fetches_a_pull_request_into_a_namespaced_branch() {
        // Simulate what GitHub publishes: the base repository carries the PR
        // head at refs/pull/<n>/head, so no fork remote is involved.
        let remote_dir = bare_remote();
        let a = fixture(2);
        let repo_a = Repo::open(a.path()).unwrap();
        add(&repo_a, "origin", remote_dir.path().to_str().unwrap()).unwrap();
        push(
            &repo_a,
            "origin",
            "main",
            PushMode::Normal,
            true,
            None,
            &mut noop,
        )
        .unwrap();

        // A contributor's commit, published only under refs/pull/7/head.
        std::fs::write(a.path().join("contrib.txt"), "from a fork\n").unwrap();
        crate::stage::stage_file(&repo_a, "contrib.txt").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(a.path())
            .args(["commit", "-q", "-m", "contributed"])
            .env("GIT_AUTHOR_NAME", "C")
            .env("GIT_AUTHOR_EMAIL", "c@e.invalid")
            .env("GIT_COMMITTER_NAME", "C")
            .env("GIT_COMMITTER_EMAIL", "c@e.invalid")
            .output()
            .unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(a.path())
            .args(["push", "origin", "HEAD:refs/pull/7/head"])
            .output()
            .unwrap();
        assert!(out.status.success(), "seeding refs/pull failed");

        // A fresh clone has no knowledge of the contributor at all.
        let clone_dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", "-q"])
            .arg(remote_dir.path())
            .arg(clone_dir.path())
            .output()
            .unwrap();
        let clone = Repo::open(clone_dir.path()).unwrap();

        fetch_pull_request(&clone, "origin", 7, "pr/7", None, &mut noop).unwrap();

        let refs: Vec<_> = crate::refs::list(&clone)
            .unwrap()
            .into_iter()
            .map(|r| r.short)
            .collect();
        assert!(
            refs.contains(&"pr/7".to_string()),
            "expected a pr/7 branch, got {refs:?}"
        );

        // And it carries the contributor's commit.
        let out = Command::new("git")
            .arg("-C")
            .arg(clone_dir.path())
            .args(["log", "--format=%s", "pr/7", "-1"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "contributed");
    }

    #[test]
    fn re_fetching_a_rewritten_pull_request_moves_the_branch() {
        let remote_dir = bare_remote();
        let a = fixture(1);
        let repo_a = Repo::open(a.path()).unwrap();
        add(&repo_a, "origin", remote_dir.path().to_str().unwrap()).unwrap();
        push(
            &repo_a,
            "origin",
            "main",
            PushMode::Normal,
            true,
            None,
            &mut noop,
        )
        .unwrap();

        let seed = |msg: &str| {
            std::fs::write(a.path().join("p.txt"), format!("{msg}\n")).unwrap();
            crate::stage::stage_file(&repo_a, "p.txt").unwrap();
            Command::new("git")
                .arg("-C")
                .arg(a.path())
                .args(["commit", "-q", "--amend", "-m", msg])
                .env("GIT_AUTHOR_NAME", "C")
                .env("GIT_AUTHOR_EMAIL", "c@e.invalid")
                .env("GIT_COMMITTER_NAME", "C")
                .env("GIT_COMMITTER_EMAIL", "c@e.invalid")
                .output()
                .unwrap();
            Command::new("git")
                .arg("-C")
                .arg(a.path())
                .args(["push", "--force", "origin", "HEAD:refs/pull/9/head"])
                .output()
                .unwrap();
        };

        seed("first version");
        let clone_dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["clone", "-q"])
            .arg(remote_dir.path())
            .arg(clone_dir.path())
            .output()
            .unwrap();
        let clone = Repo::open(clone_dir.path()).unwrap();
        fetch_pull_request(&clone, "origin", 9, "pr/9", None, &mut noop).unwrap();

        // The contributor rebases and force-pushes — the normal case.
        seed("rewritten version");
        fetch_pull_request(&clone, "origin", 9, "pr/9", None, &mut noop)
            .expect("a rebased pull request must not fail on non-fast-forward");

        let out = Command::new("git")
            .arg("-C")
            .arg(clone_dir.path())
            .args(["log", "--format=%s", "pr/9", "-1"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "rewritten version"
        );
    }

    #[test]
    fn a_failing_transfer_reports_gits_own_message() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        add(&repo, "origin", "/nonexistent/path/to/repo.git").unwrap();

        let err = fetch(&repo, Some("origin"), None, &mut noop).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("does not appear to be a git repository")
                || msg.contains("repository")
                || msg.contains("Could not read"),
            "unexpected message: {msg}"
        );
    }
}
