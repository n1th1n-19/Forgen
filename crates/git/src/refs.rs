//! Branch, remote-branch and tag listing for the sidebar.

use crate::{GitError, ObjectId, Repo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub kind: RefKind,
    /// Display name: `main`, `origin/main`, `v1.2.0`.
    pub short: String,
    /// Full name: `refs/heads/main`.
    pub full: String,
    /// Target commit. `None` for a symbolic or unresolvable ref.
    pub target: Option<ObjectId>,
}

/// All refs, grouped and sorted for the sidebar.
///
/// Reads the ref database directly rather than shelling out: this runs on every
/// repo open and on every fetch, and `git for-each-ref` would mean a process
/// spawn plus output parsing for data gix already has mapped.
pub fn list(repo: &Repo) -> Result<Vec<Ref>, GitError> {
    let platform = repo
        .inner()
        .references()
        .map_err(|e| GitError::Walk(e.to_string()))?;

    let iter = platform.all().map_err(|e| GitError::Walk(e.to_string()))?;

    let mut out = Vec::new();
    for r in iter {
        let mut r = match r {
            Ok(r) => r,
            // A single unreadable ref must not blank the whole sidebar.
            Err(e) => {
                tracing::warn!(error = %e, "skipping unreadable ref");
                continue;
            }
        };

        let full = r.name().as_bstr().to_string();
        let Some(kind) = classify(&full) else {
            continue;
        };
        // Tags peel through the tag object to the commit, so annotated and
        // lightweight tags both land on a commit id.
        let target = r.peel_to_id().ok().map(|id| ObjectId::from(id.detach()));

        out.push(Ref {
            kind,
            short: shorten(&full),
            full,
            target,
        });
    }

    // Local branches, then remotes, then tags; alphabetical within each group.
    out.sort_by(|a, b| {
        group_order(a.kind)
            .cmp(&group_order(b.kind))
            .then_with(|| a.short.cmp(&b.short))
    });
    Ok(out)
}

fn classify(full: &str) -> Option<RefKind> {
    if full.starts_with("refs/heads/") {
        Some(RefKind::LocalBranch)
    } else if full.starts_with("refs/remotes/") {
        // `refs/remotes/origin/HEAD` is a symbolic pointer, not a branch users
        // can check out; showing it duplicates the default branch in the list.
        (!full.ends_with("/HEAD")).then_some(RefKind::RemoteBranch)
    } else if full.starts_with("refs/tags/") {
        Some(RefKind::Tag)
    } else {
        // refs/stash, refs/notes/*, refs/pull/* — not sidebar material.
        None
    }
}

fn shorten(full: &str) -> String {
    for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
        if let Some(rest) = full.strip_prefix(prefix) {
            return rest.to_owned();
        }
    }
    full.to_owned()
}

fn group_order(k: RefKind) -> u8 {
    match k {
        RefKind::LocalBranch => 0,
        RefKind::RemoteBranch => 1,
        RefKind::Tag => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;
    use std::process::Command;

    #[test]
    fn lists_branches_and_tags_grouped() {
        let dir = fixture(2);
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        run(&["branch", "feature"]);
        run(&["tag", "v1.0"]);

        let repo = Repo::open(dir.path()).unwrap();
        let refs = list(&repo).unwrap();

        let names: Vec<_> = refs.iter().map(|r| r.short.as_str()).collect();
        assert_eq!(
            names,
            ["feature", "main", "v1.0"],
            "grouped then alphabetical"
        );

        assert_eq!(refs[0].kind, RefKind::LocalBranch);
        assert_eq!(refs[2].kind, RefKind::Tag);
        assert_eq!(refs[0].full, "refs/heads/feature");
        assert!(refs.iter().all(|r| r.target.is_some()));
    }

    #[test]
    fn classification_skips_noise_refs() {
        assert_eq!(classify("refs/heads/main"), Some(RefKind::LocalBranch));
        assert_eq!(
            classify("refs/remotes/origin/main"),
            Some(RefKind::RemoteBranch)
        );
        assert_eq!(classify("refs/tags/v1"), Some(RefKind::Tag));
        // Symbolic remote HEAD would duplicate the default branch.
        assert_eq!(classify("refs/remotes/origin/HEAD"), None);
        assert_eq!(classify("refs/stash"), None);
        assert_eq!(classify("refs/notes/commits"), None);
    }

    #[test]
    fn shorten_strips_only_known_prefixes() {
        assert_eq!(shorten("refs/heads/feat/x"), "feat/x");
        assert_eq!(shorten("refs/remotes/origin/main"), "origin/main");
        assert_eq!(shorten("HEAD"), "HEAD");
    }
}
