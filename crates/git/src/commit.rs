//! Committing, and the identity/signing configuration around it.
//!
//! Always through the `git` binary, never through gix. A commit is where the
//! most user configuration converges: `pre-commit` and `commit-msg` hooks,
//! `commit.gpgsign`, `gpg.format = ssh`, `user.signingkey`, `commit.template`,
//! `core.hooksPath`, and gitattributes filters on the staged content. Any of
//! these silently skipped produces a commit the user did not ask for — an
//! unsigned commit in a repository that requires signatures, or a commit that
//! bypassed the lint hook that was supposed to stop it.

use std::process::Command;

use crate::{GitError, Repo};

#[derive(Clone, Debug, Default)]
pub struct CommitOptions {
    /// Replace the previous commit instead of adding one.
    pub amend: bool,
    /// Force signing on. Leave `false` to honour `commit.gpgsign`, which is
    /// what the user configured.
    pub sign: bool,
    /// Skip `pre-commit` and `commit-msg` hooks. Exposed because hooks do
    /// occasionally need overriding, but never defaulted on.
    pub no_verify: bool,
    /// `Co-authored-by:` trailers, appended as git formats them.
    pub co_authors: Vec<String>,
    /// Commit as this author, `Name <email>`. `None` uses the repo config.
    pub author: Option<String>,
}

/// Create a commit from the current index.
///
/// The message is passed via `-F -` on stdin rather than `-m`: a message
/// containing anything shell-adjacent, or simply a very long one, is otherwise
/// at the mercy of argument limits and quoting.
pub fn commit(repo: &Repo, message: &str, opts: &CommitOptions) -> Result<String, GitError> {
    if message.trim().is_empty() && !opts.amend {
        return Err(GitError::Object("commit message is empty".into()));
    }

    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;

    let mut full = message.trim_end().to_string();
    if !opts.co_authors.is_empty() {
        // Trailers must be separated from the body by a blank line or git will
        // fold them into the message text instead of parsing them.
        full.push_str("\n\n");
        for who in &opts.co_authors {
            full.push_str(&format!("Co-authored-by: {who}\n"));
        }
    }
    full.push('\n');

    let mut args: Vec<String> = vec!["commit".into(), "-F".into(), "-".into()];
    if opts.amend {
        args.push("--amend".into());
    }
    if opts.sign {
        args.push("-S".into());
    }
    if opts.no_verify {
        args.push("--no-verify".into());
    }
    if let Some(author) = &opts.author {
        args.push("--author".into());
        args.push(author.clone());
    }

    let out = run_with_stdin(workdir, &args, &full)?;
    if !out.status.success() {
        // Hook rejections write to stderr and are the message the user needs.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(GitError::Walk(format!("commit failed: {detail}")));
    }

    head_sha(repo)
}

/// Whether this repository is configured to sign commits.
pub fn signing_configured(repo: &Repo) -> bool {
    config(repo, "commit.gpgsign")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Signing format in use: `openpgp` (the default), `ssh`, or `x509`.
pub fn signing_format(repo: &Repo) -> String {
    config(repo, "gpg.format").unwrap_or_else(|| "openpgp".into())
}

/// Configured author identity, if any.
///
/// Checked before enabling the commit button: committing without one fails with
/// a wall of text about `git config --global user.email`, which is better shown
/// as a prompt than as a post-hoc error.
pub fn identity(repo: &Repo) -> Option<(String, String)> {
    Some((config(repo, "user.name")?, config(repo, "user.email")?))
}

/// Contents of `commit.template`, if configured.
pub fn message_template(repo: &Repo) -> Option<String> {
    let path = config(repo, "commit.template")?;
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(rest))?
    } else {
        std::path::PathBuf::from(path)
    };
    std::fs::read_to_string(expanded).ok()
}

fn config(repo: &Repo, key: &str) -> Option<String> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn head_sha(repo: &Repo) -> Result<String, GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !out.status.success() {
        return Err(GitError::Walk("could not resolve HEAD".into()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_with_stdin(
    workdir: &std::path::Path,
    args: &[String],
    stdin_data: &str,
) -> Result<std::process::Output, GitError> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    child
        .stdin
        .as_mut()
        .expect("stdin piped above")
        .write_all(stdin_data.as_bytes())?;

    Ok(child.wait_with_output()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;
    use crate::{stage, Repo};

    fn repo_with_identity() -> (tempfile::TempDir, Repo) {
        let dir = fixture(1);
        for (k, v) in [("user.name", "Fixture"), ("user.email", "f@e.invalid")] {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["config", k, v])
                .output()
                .unwrap();
        }
        let repo = Repo::open(dir.path()).unwrap();
        (dir, repo)
    }

    fn last_message(repo: &Repo) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo.workdir().unwrap())
            .args(["log", "-1", "--format=%B"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn commits_staged_content_and_returns_the_sha() {
        let (_d, repo) = repo_with_identity();
        std::fs::write(repo.workdir().unwrap().join("new.txt"), "hello\n").unwrap();
        stage::stage_file(&repo, "new.txt").unwrap();

        let sha = commit(&repo, "add new.txt", &CommitOptions::default()).unwrap();
        assert_eq!(sha.len(), 40, "expected a full sha, got {sha:?}");
        assert!(last_message(&repo).starts_with("add new.txt"));
    }

    #[test]
    fn an_empty_message_is_refused_before_spawning_git() {
        let (_d, repo) = repo_with_identity();
        let err = commit(&repo, "   \n ", &CommitOptions::default()).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn co_authors_become_trailers_after_a_blank_line() {
        let (_d, repo) = repo_with_identity();
        std::fs::write(repo.workdir().unwrap().join("a.txt"), "x\n").unwrap();
        stage::stage_file(&repo, "a.txt").unwrap();

        let opts = CommitOptions {
            co_authors: vec!["Ada <ada@example.invalid>".into()],
            ..Default::default()
        };
        commit(&repo, "pair work", &opts).unwrap();

        let msg = last_message(&repo);
        assert!(
            msg.contains("Co-authored-by: Ada <ada@example.invalid>"),
            "{msg}"
        );
        assert!(
            msg.contains("pair work\n\nCo-authored-by:"),
            "a trailer needs a blank line before it or git folds it into the \
             body:\n{msg}"
        );
    }

    #[test]
    fn amend_replaces_rather_than_adds() {
        let (_d, repo) = repo_with_identity();
        let before = count_commits(&repo);

        std::fs::write(repo.workdir().unwrap().join("b.txt"), "y\n").unwrap();
        stage::stage_file(&repo, "b.txt").unwrap();
        commit(&repo, "first wording", &CommitOptions::default()).unwrap();
        assert_eq!(count_commits(&repo), before + 1);

        let opts = CommitOptions {
            amend: true,
            ..Default::default()
        };
        commit(&repo, "better wording", &opts).unwrap();

        assert_eq!(
            count_commits(&repo),
            before + 1,
            "amend must not add a commit"
        );
        assert!(last_message(&repo).starts_with("better wording"));
    }

    #[test]
    fn a_failing_pre_commit_hook_surfaces_its_output() {
        let (_d, repo) = repo_with_identity();
        let hooks = repo.git_dir().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'lint failed: no' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        std::fs::write(repo.workdir().unwrap().join("c.txt"), "z\n").unwrap();
        stage::stage_file(&repo, "c.txt").unwrap();

        let err = commit(&repo, "should be blocked", &CommitOptions::default()).unwrap_err();
        assert!(
            err.to_string().contains("lint failed"),
            "the hook's own message is what the user needs to see: {err}"
        );
    }

    #[test]
    fn no_verify_bypasses_the_hook() {
        let (_d, repo) = repo_with_identity();
        let hooks = repo.git_dir().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        std::fs::write(repo.workdir().unwrap().join("d.txt"), "w\n").unwrap();
        stage::stage_file(&repo, "d.txt").unwrap();

        let opts = CommitOptions {
            no_verify: true,
            ..Default::default()
        };
        assert!(commit(&repo, "forced through", &opts).is_ok());
    }

    #[test]
    fn identity_is_read_from_config() {
        let (_d, repo) = repo_with_identity();
        let (name, email) = identity(&repo).unwrap();
        assert_eq!(name, "Fixture");
        assert_eq!(email, "f@e.invalid");
    }

    #[test]
    fn signing_defaults_to_off_and_openpgp() {
        let (_d, repo) = repo_with_identity();
        assert!(!signing_configured(&repo));
        assert_eq!(signing_format(&repo), "openpgp");
    }

    #[test]
    fn signing_config_is_reported_when_set() {
        let (dir, repo) = repo_with_identity();
        for (k, v) in [("commit.gpgsign", "true"), ("gpg.format", "ssh")] {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["config", k, v])
                .output()
                .unwrap();
        }
        assert!(signing_configured(&repo));
        assert_eq!(signing_format(&repo), "ssh");
    }

    fn count_commits(repo: &Repo) -> usize {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo.workdir().unwrap())
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }
}
