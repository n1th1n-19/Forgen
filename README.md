# forqen

A native GitHub client for Linux. Local git and github.com in one application,
without a bundled web engine.

## Why

GitHub Desktop has no official Linux build; the community fork is Electron and
sits around 400MB resident. GitKraken is Electron and paid. Lazygit is terminal
only and has no pull request review. gitg is local-git only and unmaintained.

forqen is Rust and GTK4. The target is a flat memory curve on repositories that
make Electron clients swap.

### Measured, not aspirational

Release build, 50,000-commit repository, on Pop!_OS with the NVIDIA driver:

| | empty window | repo open | forqen's own cost |
|---|---|---|---|
| GTK GL renderer (default) | 154 MB | 192 MB | **38 MB** |
| `GSK_RENDERER=cairo` | 67 MB | 105 MB | **38 MB** |

The application's data costs 38MB and does not grow with time or scrolling.
The rest is GTK's renderer baseline — mostly GPU driver buffers, and the reason
the original 90MB idle target is not reachable with hardware rendering on this
hardware. That was an honest miss in planning: the budget was set without
measuring what an empty GTK4 window costs.

The part forqen controls is gated in CI. `crates/git/tests/memcheck.rs` walks
and scrolls a 20,000-commit history in both directions and fails the build if
RSS growth exceeds 64MB. Current measurement: **12MB**.

## Status

R0, R1 and R2 complete. 181 tests.

**Engine — all tested against real repositories:**
windowed history over large repositories · refs · working-tree status ·
unified diff parsing · staging by file, hunk and individual line · commit with
hooks, signing config and co-author trailers · branch
create/switch/rename/delete with a dirty-tree guard · stash
push/pop/apply/drop/preview · fetch/pull/push with live progress and
force-with-lease · merge with conflict detection · three-way conflict reading
from index stages · device-flow auth · keyring storage · `gh` token import ·
HTTP revalidation cache · rate limiting.

**UI:** History page with the windowed commit list · Changes page with
staged/unstaged lists, an interactive diff pane supporting **hunk- and
line-level staging, unstaging and discarding**, and a commit box · sync
buttons with live progress · branch switching · a three-way conflict
resolver that appears only while a merge is stopped · responsive layout that
collapses the sidebar and stacks the panes on a narrow window · stash
browser with per-stash diff preview · keyboard actions exported on the
session bus · session restore.

**R2:** a Pull Requests page lists open PRs, shows each one's changed files
and diffs, drafts inline review comments anchored to a line and side, submits
them as one review (comment, approve, or request changes), merges, and checks
a PR out into a `pr/<n>` branch via the `refs/pull/*` refspec — fork-aware,
and correct for forks that have since been deleted. Review threads come from
GraphQL, so resolved and outdated state is real rather than inferred.

**CLI:** `forqen login` adopts the `gh` CLI's token, `forqen accounts` lists
signed-in identities, `forqen logout` removes one. Sign-in without a display
is the point — a headless machine, or a build with no GitHub App id.

**Not built:** issues, Actions, notifications, interactive rebase, worktrees,
reflog, search, releases. See `PLAN.md`.

**Needs setup before browser sign-in works:** the binary ships a placeholder
GitHub App id. Register an App and rebuild with `FORQEN_CLIENT_ID=<id>`, or use
the `gh` CLI import path, which works today.

## Architecture

```
crates/
├── git/      history, refs, status, diff, staging, commit, branch, stash
├── github/   REST, ETag revalidation, rate limiting
├── auth/     device flow, keyring, gh import, multi-account
├── db/       SQLite: account roster + HTTP cache
├── ui/       GTK4 widgets and view models
└── app/      the `forqen` binary
```

`git`, `github`, `auth` and `db` never import `gtk`. They are testable without a
display server, so a behavioural bug reproduces in a unit test rather than under
a compositor.

### Two git backends

`gix` handles reads: revwalk, objects, refs, status, diff, blame. The `git`
binary handles rebase, signed commits, hook-running commits, push/fetch
negotiation, LFS and filters.

This is not a stopgap. `gix-rebase` is published at version `0.0.0` — an empty
placeholder — and reimplementing hook execution, gitattributes filters and
commit signing is how a client silently corrupts someone's repository. Shelling
out is both more correct and cheaper in memory, since the child process's heap
dies with the child.

### How the memory ceiling is held

Two structures with deliberately different costs:

- **The spine** — `Vec<ObjectId>`, 20 inline bytes per commit, never evicted.
  About 26MB for the Linux kernel's 1.3M commits. Gives O(1) random access so
  the scrollbar can jump anywhere.
- **Realized rows** — `HashMap<usize, CommitRow>`, capped at 512 entries. These
  hold the heap strings, so these are what eviction targets.

`GtkColumnView` only asks for rows it is about to draw, so the model reports a
million rows while holding a few hundred. Eviction ranks by distance from the
viewport rather than by age: pure FIFO thrashes on scroll-up, because the rows
just passed are the oldest and get discarded first.

Also: bounded gix object cache, mmapped blobs, lazy per-file diffs, and
`MALLOC_ARENA_MAX=2` set in the `.desktop` entry and Flatpak manifest — glibc
reads it at the first `malloc`, long before `main`, so it cannot be set from
inside the process.

## Authentication

OAuth **device flow** against a GitHub App. Only a public `client_id` ships in
the binary; no client secret, because a secret in a distributed binary is
readable with `strings` and is therefore not a secret.

- Tokens go to the Secret Service via the `keyring` crate. There is no
  file-backed fallback — if no keyring is available forqen says so and stops,
  rather than quietly writing credentials somewhere every process can read.
- An existing `gh` CLI login can be adopted on first run.
- PAT paste is supported for GitHub Enterprise Server.
- Multiple accounts across multiple hosts, from day one.
- Git transport auth is separate: ssh-agent for SSH remotes, the OAuth token
  for HTTPS.

## Building

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev meson flatpak-builder

cargo build --workspace
cargo run --bin forqen -- /path/to/repo
```

GtkSourceView is deliberately not a dependency. The diff pane is a
`GtkColumnView` where one row is one diff line, so a selection maps to diff
lines unambiguously — which is what makes line-level staging possible. A
`GtkTextView` selection is a character range, and recovering "which lines" from
it is guesswork the moment a line wraps.

The engine crates carry no GTK dependency and need none of that:

```bash
cargo test -p git -p auth -p db -p github
```

### Flatpak

```bash
flatpak-builder --user --install --force-clean \
    build build-aux/io.github.forqen.Forqen.yml
flatpak run io.github.forqen.Forqen
```

The manifest punches specific holes that are worth knowing about:
`--socket=ssh-auth` for pushing over SSH, `--talk-name=org.freedesktop.secrets`
for the keyring, and git itself is bundled as a module because
`org.gnome.Platform` does not ship it.

## Testing

```bash
cargo test --workspace                       # 181 tests
cargo test -p git -p auth -p db -p github    # 149 of them, no display server needed
cargo test -p git --test memcheck --release  # the memory gate
cargo test -p auth -- --ignored              # keyring round trip, needs a session bus
```

Git tests build fixture repositories with the real `git` binary and assert
against the real index, so the fixtures are unarguably valid rather than
whatever gix is assumed to write. Staging tests in particular check the *index
contents* after a partial stage, not just that the command exited zero — a
synthesized patch that applies cleanly but stages the wrong lines is the
failure mode that matters.

Not covered by tests: GTK widget interaction. The window has been run and
visually verified against a real repository — history, the Changes page, the
interactive diff and the responsive layout all render correctly — but button
clicks have not been driven programmatically.

## Licence

GPL-3.0-or-later.
