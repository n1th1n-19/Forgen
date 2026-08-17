//! Interactive rebase.
//!
//! This is the module the whole two-backend design exists for. `gix-rebase` is
//! published at version `0.0.0` — an empty placeholder — so there is nothing to
//! call, and reimplementing rebase means reimplementing hook execution,
//! gitattributes filters, commit signing, and the rerere cache. Getting any of
//! those wrong silently rewrites someone's history incorrectly.
//!
//! So the plan is written as a todo list and handed to `git rebase -i` with
//! `GIT_SEQUENCE_EDITOR` pointed at a command that just copies it into place.
//! git then performs the rebase it would have performed anyway; the interface
//! only decides what the list says.

use std::io::Write;
use std::process::Command;

use crate::{GitError, ObjectId, Repo};

/// What to do with one commit during a rebase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep it as it is.
    Pick,
    /// Keep the change, stop to edit the message.
    Reword,
    /// Stop after applying, so the tree can be amended.
    Edit,
    /// Combine into the previous commit, keeping both messages.
    Squash,
    /// Combine into the previous commit, discarding this message.
    Fixup,
    /// Drop the commit entirely.
    Drop,
}

impl Action {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }

    /// Whether this action folds the commit into the one above it.
    pub fn is_fold(self) -> bool {
        matches!(self, Self::Squash | Self::Fixup)
    }

    pub fn all() -> [Action; 6] {
        [
            Self::Pick,
            Self::Reword,
            Self::Edit,
            Self::Squash,
            Self::Fixup,
            Self::Drop,
        ]
    }
}

/// One line of the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub action: Action,
    pub id: ObjectId,
    /// First line of the message, for display and for the todo comment.
    pub summary: String,
}

/// A rebase plan: the commits to replay, oldest first.
///
/// Oldest first because that is the order `git rebase -i` writes and applies
/// them, and presenting them newest-first — the order history is read in —
/// would mean "squash into the previous" pointed the opposite way on screen
/// from the way it behaves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
}

impl Plan {
    /// Build a plan for the commits between `onto` and HEAD.
    pub fn from_range(repo: &Repo, onto: &str) -> Result<Self, GitError> {
        let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
        let out = Command::new("git")
            .arg("-C")
            .arg(workdir)
            // Reverse so the plan reads oldest first, as git applies it.
            .args([
                "log",
                "--reverse",
                "--format=%H%x00%s",
                &format!("{onto}..HEAD"),
            ])
            .output()?;

        if !out.status.success() {
            return Err(GitError::Walk(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }

        let text = String::from_utf8_lossy(&out.stdout);
        let steps = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| {
                let (hex, summary) = line.split_once('\0')?;
                Some(Step {
                    action: Action::Pick,
                    id: parse_hex(hex)?,
                    summary: summary.to_string(),
                })
            })
            .collect();

        Ok(Self { steps })
    }

    /// Render the plan as a `git-rebase-todo` file.
    pub fn to_todo(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            // A dropped commit is written as a `drop` line rather than omitted.
            // Omitting it works, but the todo file is also the record of what
            // was decided, and a silently missing line is impossible to review.
            out.push_str(&format!(
                "{} {} {}\n",
                step.action.keyword(),
                step.id.to_hex(),
                step.summary
            ));
        }
        out
    }

    /// Whether git would reject this plan outright.
    ///
    /// Only one rule matters in practice: the first step cannot fold, because
    /// there is nothing above it to fold into. git's own error for this
    /// arrives after it has already started and left a rebase in progress.
    pub fn problem(&self) -> Option<&'static str> {
        match self.steps.first() {
            None => Some("nothing to rebase"),
            Some(first) if first.action.is_fold() => {
                Some("the first commit has nothing above it to squash into")
            }
            _ => {
                if self.steps.iter().all(|s| s.action == Action::Drop) {
                    Some("every commit is dropped, which would empty the branch")
                } else {
                    None
                }
            }
        }
    }

    /// Move a step, keeping the plan's order meaningful.
    pub fn move_step(&mut self, from: usize, to: usize) {
        if from >= self.steps.len() || to >= self.steps.len() || from == to {
            return;
        }
        let step = self.steps.remove(from);
        self.steps.insert(to, step);
    }
}

/// Outcome of starting a rebase.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Finished cleanly.
    Complete,
    /// Stopped — for a conflict, or an `edit`/`reword` step. The working tree
    /// is mid-rebase until continued or aborted.
    Stopped { reason: String },
}

/// Run an interactive rebase against `onto` with this plan.
///
/// `GIT_SEQUENCE_EDITOR` is set to a `cp` that overwrites git's generated todo
/// with ours, which is how a GUI drives `rebase -i` without a terminal editor.
/// `GIT_EDITOR=true` covers the message editor for `reword`, so the process can
/// never block waiting for an editor that will not appear.
pub fn run(repo: &Repo, onto: &str, plan: &Plan) -> Result<Outcome, GitError> {
    if let Some(problem) = plan.problem() {
        return Err(GitError::Walk(problem.to_string()));
    }

    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;

    // Written next to the git dir rather than /tmp: a sequence editor is
    // invoked inside the repository, and a path there is one fewer thing that
    // can be missing under a sandbox.
    let todo_path = repo.git_dir().join("forqen-rebase-todo");
    let mut file = std::fs::File::create(&todo_path)?;
    file.write_all(plan.to_todo().as_bytes())?;
    file.sync_all()?;

    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rebase", "-i", onto])
        .env(
            "GIT_SEQUENCE_EDITOR",
            format!("cp {}", shell_quote(&todo_path.to_string_lossy())),
        )
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;

    let _ = std::fs::remove_file(&todo_path);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if out.status.success() {
        // `edit` and `reword` stop successfully, so an exit code of zero does
        // not mean the rebase is over — the state directory is what settles it.
        if in_progress(repo) {
            return Ok(Outcome::Stopped {
                reason: first_useful_line(&stdout, &stderr),
            });
        }
        return Ok(Outcome::Complete);
    }

    if in_progress(repo) {
        return Ok(Outcome::Stopped {
            reason: first_useful_line(&stdout, &stderr),
        });
    }

    Err(GitError::Walk(format!("rebase failed: {}", stderr.trim())))
}

/// Continue after resolving a conflict or finishing an `edit`.
pub fn cont(repo: &Repo) -> Result<Outcome, GitError> {
    run_step(repo, "--continue")
}

pub fn skip(repo: &Repo) -> Result<Outcome, GitError> {
    run_step(repo, "--skip")
}

/// Abandon the rebase and restore the original branch.
pub fn abort(repo: &Repo) -> Result<(), GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rebase", "--abort"])
        .env("GIT_EDITOR", "true")
        .output()?;
    if !out.status.success() {
        return Err(GitError::Walk(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

fn run_step(repo: &Repo, flag: &str) -> Result<Outcome, GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rebase", flag])
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    if in_progress(repo) {
        Ok(Outcome::Stopped {
            reason: first_useful_line(&stdout, &stderr),
        })
    } else if out.status.success() {
        Ok(Outcome::Complete)
    } else {
        Err(GitError::Walk(stderr.trim().to_string()))
    }
}

/// Whether a rebase is currently stopped in this repository.
pub fn in_progress(repo: &Repo) -> bool {
    let g = repo.git_dir();
    g.join("rebase-merge").exists() || g.join("rebase-apply").exists()
}

fn first_useful_line(stdout: &str, stderr: &str) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("rebase stopped")
        .to_string()
}

/// Quote a path for the shell git will run the sequence editor through.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn parse_hex(hex: &str) -> Option<ObjectId> {
    let hex = hex.trim();
    if hex.len() < 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(ObjectId(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;

    fn plan_of(actions: &[Action]) -> Plan {
        Plan {
            steps: actions
                .iter()
                .enumerate()
                .map(|(i, a)| Step {
                    action: *a,
                    id: ObjectId([i as u8; 20]),
                    summary: format!("commit {i}"),
                })
                .collect(),
        }
    }

    // --- plan shape, no git process involved --------------------------------

    #[test]
    fn a_plan_reads_oldest_first() {
        let dir = fixture(4);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let plan = Plan::from_range(&repo, "HEAD~3").unwrap();

        assert_eq!(plan.steps.len(), 3);
        assert_eq!(
            plan.steps[0].summary, "commit 1",
            "the plan must apply oldest first, the way git writes and runs it"
        );
        assert_eq!(plan.steps[2].summary, "commit 3");
        assert!(plan.steps.iter().all(|s| s.action == Action::Pick));
    }

    #[test]
    fn the_todo_renders_the_lines_git_expects() {
        let plan = plan_of(&[Action::Pick, Action::Squash, Action::Drop]);
        let todo = plan.to_todo();
        let lines: Vec<&str> = todo.lines().collect();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("pick 0000"), "{}", lines[0]);
        assert!(lines[1].starts_with("squash 0101"), "{}", lines[1]);
        assert!(
            lines[2].starts_with("drop 0202"),
            "a dropped commit is written as `drop`, not omitted — the todo is \
             also the record of what was decided: {}",
            lines[2]
        );
        assert!(lines[0].ends_with("commit 0"));
    }

    #[test]
    fn a_first_step_that_folds_is_refused_before_git_starts() {
        // git's own error for this arrives after it has begun and left a
        // rebase in progress to clean up.
        for fold in [Action::Squash, Action::Fixup] {
            let plan = plan_of(&[fold, Action::Pick]);
            assert!(
                plan.problem().is_some(),
                "{fold:?} as the first step has nothing to fold into"
            );
        }
        assert!(plan_of(&[Action::Pick, Action::Squash]).problem().is_none());
    }

    #[test]
    fn dropping_everything_is_refused() {
        assert!(plan_of(&[Action::Drop, Action::Drop]).problem().is_some());
        assert!(Plan::default().problem().is_some());
    }

    #[test]
    fn moving_a_step_reorders_without_losing_any() {
        let mut plan = plan_of(&[Action::Pick; 4]);
        plan.move_step(3, 0);

        let order: Vec<&str> = plan.steps.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(order, ["commit 3", "commit 0", "commit 1", "commit 2"]);
        assert_eq!(plan.steps.len(), 4);
    }

    #[test]
    fn an_out_of_range_move_is_ignored_rather_than_panicking() {
        let mut plan = plan_of(&[Action::Pick; 2]);
        plan.move_step(5, 0);
        plan.move_step(0, 9);
        plan.move_step(1, 1);
        assert_eq!(plan.steps.len(), 2);
    }

    #[test]
    fn a_path_with_a_quote_is_escaped_for_the_sequence_editor() {
        // The sequence editor runs through a shell, so an apostrophe in a
        // repository path would otherwise end the quoting and break the command.
        assert_eq!(shell_quote("/home/me/repo"), "'/home/me/repo'");
        assert_eq!(
            shell_quote("/home/o'brien/repo"),
            r"'/home/o'\''brien/repo'"
        );
    }

    #[test]
    fn hex_parsing_rejects_anything_that_is_not_a_full_sha() {
        assert!(parse_hex("abc").is_none());
        assert!(parse_hex("zz".repeat(20).as_str()).is_none());
        let id = parse_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(id.to_hex(), "0123456789abcdef0123456789abcdef01234567");
    }

    // --- against real repositories ------------------------------------------

    /// A repo whose commits touch *different* files.
    ///
    /// The shared fixture rewrites one file every commit, so dropping or
    /// reordering any of them conflicts — correct git behaviour, but it means
    /// a test asserting "drop removed the commit" would really be asserting
    /// "the patches happened to still apply". Independent files separate the
    /// two questions.
    fn independent(n: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(p)
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        for i in 0..n {
            let name = format!("f{i}.txt");
            std::fs::write(p.join(&name), format!("{i}\n")).unwrap();
            run(&["add", &name]);
            run(&["commit", "-q", "-m", &format!("commit {i}")]);
        }
        dir
    }

    #[test]
    fn a_plain_pick_plan_replays_history_unchanged() {
        let dir = fixture(4);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let plan = Plan::from_range(&repo, "HEAD~3").unwrap();

        assert_eq!(run(&repo, "HEAD~3", &plan).unwrap(), Outcome::Complete);

        let repo = crate::Repo::open(dir.path()).unwrap();
        let after = Plan::from_range(&repo, "HEAD~3").unwrap();
        let summaries: Vec<&str> = after.steps.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(summaries, ["commit 1", "commit 2", "commit 3"]);
        assert!(!in_progress(&repo));
    }

    #[test]
    fn dropping_a_commit_removes_it_from_history() {
        let dir = independent(4);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let mut plan = Plan::from_range(&repo, "HEAD~3").unwrap();
        plan.steps[1].action = Action::Drop;

        assert_eq!(run(&repo, "HEAD~3", &plan).unwrap(), Outcome::Complete);

        let repo = crate::Repo::open(dir.path()).unwrap();
        let after = Plan::from_range(&repo, "HEAD~2").unwrap();
        let summaries: Vec<&str> = after.steps.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(
            summaries,
            ["commit 1", "commit 3"],
            "commit 2 should be gone and the rest kept"
        );
        assert!(
            !dir.path().join("f2.txt").exists(),
            "the dropped commit's file should be gone with it"
        );
    }

    #[test]
    fn a_drop_that_conflicts_stops_rather_than_corrupting() {
        // Every commit rewrites the same file here, so removing one from the
        // middle leaves the next patch unappliable. Stopping is correct — the
        // point is that it reports rather than producing a wrong tree.
        let dir = fixture(4);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let mut plan = Plan::from_range(&repo, "HEAD~3").unwrap();
        plan.steps[1].action = Action::Drop;

        match run(&repo, "HEAD~3", &plan).unwrap() {
            Outcome::Stopped { reason } => {
                assert!(!reason.is_empty(), "a stop must say why");
                assert!(in_progress(&repo));
                abort(&repo).unwrap();
                assert!(!in_progress(&repo));
            }
            Outcome::Complete => {} // a future git may merge this cleanly
        }
    }

    #[test]
    fn a_fixup_folds_two_commits_into_one() {
        let dir = fixture(4);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let mut plan = Plan::from_range(&repo, "HEAD~3").unwrap();
        plan.steps[2].action = Action::Fixup;

        assert_eq!(run(&repo, "HEAD~3", &plan).unwrap(), Outcome::Complete);

        let repo = crate::Repo::open(dir.path()).unwrap();
        let after = Plan::from_range(&repo, "HEAD~2").unwrap();
        assert_eq!(after.steps.len(), 2, "three commits should have become two");
        assert_eq!(
            after.steps[1].summary, "commit 2",
            "fixup keeps the first message"
        );
    }

    #[test]
    fn reordering_is_applied_in_the_new_order() {
        let dir = independent(4);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let mut plan = Plan::from_range(&repo, "HEAD~3").unwrap();
        plan.move_step(0, 2);

        assert_eq!(run(&repo, "HEAD~3", &plan).unwrap(), Outcome::Complete);

        let repo = crate::Repo::open(dir.path()).unwrap();
        let after = Plan::from_range(&repo, "HEAD~3").unwrap();
        let summaries: Vec<&str> = after.steps.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(
            summaries,
            ["commit 2", "commit 3", "commit 1"],
            "the first commit was moved to the end"
        );
    }

    #[test]
    fn an_invalid_plan_never_starts_a_rebase() {
        let dir = fixture(3);
        let repo = crate::Repo::open(dir.path()).unwrap();
        let mut plan = Plan::from_range(&repo, "HEAD~2").unwrap();
        plan.steps[0].action = Action::Squash;

        assert!(run(&repo, "HEAD~2", &plan).is_err());
        assert!(
            !in_progress(&repo),
            "refusing early is the point — git would have left state to clean up"
        );
    }
}
