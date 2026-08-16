# forqen — native Linux GitHub client

## Context

No Linux git GUI covers both local git *and* github.com. GitHub Desktop has no
official Linux build (the community fork is Electron, ~400MB RSS). GitKraken is
Electron and paid. Lazygit is terminal-only and has no GitHub PR review. Existing
GTK options (gitg) are local-git-only and unmaintained.

Goal: one native application that replaces GitHub Desktop **and** the parts of
github.com people use daily (PR review, issues, Actions, notifications), while
holding RSS under a budget that Electron cannot reach.

Target user is the repo owner: Pop!_OS / COSMIC desktop, `gh` already
authenticated as `n1th1n-19`.

**Decisions locked with the user:** Rust + GTK4/libadwaita · gitoxide backend ·
full scope phased across releases · Flatpak distribution.

**Verified constraint that shapes the design:** `gix-rebase` is published at
version `0.0.0` — an empty placeholder. gitoxide cannot be the whole git
backend. See "Git backend split" below.

---

## Architecture

### Workspace layout

```
forqen/
├── Cargo.toml                  # workspace
├── crates/
│   ├── git/                    # git engine. gix + git-binary fallback. NO UI types.
│   ├── github/                 # GitHub REST+GraphQL, ETag cache, rate limiter
│   ├── auth/                   # device flow, keyring, multi-account
│   ├── db/                     # SQLite cache + GSettings prefs
│   ├── ui/                     # GTK4 widgets, view models, .blp templates
│   ├── cli/                    # companion CLI + git credential helper
│   └── app/                    # binary `forqen`; wires the above
├── data/                       # .desktop, appstream metainfo, gschema, icons
├── build-aux/
│   └── io.github.forqen.Forqen.yml   # flatpak manifest
└── meson.build                 # GNOME-conventional build, wraps cargo
```

Unprefixed workspace-member names, matching Zed's layout (`crates/git`,
`crates/ui`, …). These are path dependencies, never published, so there is no
crates.io namespace to defend — a path dep always wins resolution over a
same-named registry crate. Only the binary carries the product name.

The hard rule: `git` and `github` never import `gtk`. They are sync/async
libraries testable without a display server. Every UI bug then reproduces in a
unit test instead of under a compositor.

### Threading model

Three execution contexts, and crossing between them is explicit:

| Context | Runs | Rule |
|---|---|---|
| GTK main loop (`glib::MainContext`) | all widget mutation | never blocks; no git call ever runs here |
| `rayon` pool | blocking git work (status, diff, revwalk, blame) | returns owned plain data, no GObjects |
| single `tokio` runtime | all network (GitHub API, fetch/push) | one runtime for the whole process |

Results marshal back with `glib::spawn_future_local` + a `oneshot` channel.
Long operations return a progress stream so the UI can show determinate bars
without polling.

### Git backend split

gix where it is strong, `git` binary where gix is absent or where git's own
semantics must be preserved exactly.

**gix (in-process):** revwalk + commit-graph, object/tree/blob reads, `status`,
`diff`, `blame`, ref listing, config parsing, index reads, merge-base,
merge (via `gix-merge`).

**Shell out to `git` (`std::process::Command`, porcelain v2 / `-z` output):**
rebase (interactive and plain), `commit` when hooks or GPG/SSH signing are
configured, `push`/`fetch` with credential negotiation, LFS smudge/clean,
filters, `git gc`, submodule update.

Reason: gix does not implement rebase at all, and re-implementing hook
execution, gitattributes filters, and signing is how you produce a client that
silently corrupts someone's repository. Shelling out is *more* correct and
*less* memory, since the child process's heap dies with it.

The `git` crate exposes one trait so callers never know which path served them:

```rust
pub trait GitBackend {
    fn history(&self, spec: &RevSpec, window: Range<usize>) -> Result<Vec<CommitRow>>;
    fn status(&self) -> Result<StatusSnapshot>;
    fn diff(&self, target: DiffTarget, path: &Path) -> Result<FileDiff>;
    fn rebase(&self, plan: RebasePlan, progress: ProgressSink) -> Result<RebaseOutcome>;
    // ...
}
```

---

## Memory strategy

This is the differentiating feature; it needs enforcing, not hoping.

**Budget (RSS, measured on `torvalds/linux`, ~1.3M commits):**

| State | Ceiling |
|---|---|
| Idle, one repo open | 90 MB |
| Scrolling full history | 140 MB |
| Large diff open (10k lines) | 180 MB |
| Hard fail (CI gate) | 250 MB |

**How the ceilings are held:**

1. **Windowed commit list.** A custom `GListModel` backed by a cursor over the
   gix revwalk. Only realized rows plus ~50 overscan exist as objects. Never
   collect a `Vec<Commit>` of the whole history. This alone is the difference
   between 90MB and 900MB.
2. **commit-graph file.** Generate/read `.git/objects/info/commit-graph` for
   O(1) parent and generation-number lookup — makes windowed scroll seek cheap.
3. **Bounded pack cache.** gix's object cache set to an explicit LRU size
   (default is unbounded-ish); tuned per repo size, capped hard.
4. **mmap blobs, never `read_to_string`.** Diff over `&[u8]` slices.
5. **Path interning.** Repo paths repeat enormously across commits; one
   `Rc<str>` interner cuts allocation count by an order of magnitude.
6. **Lazy per-file diffs.** Compute on selection only. Files over a size
   threshold or detected binary show a stub, never load.
7. **`MALLOC_ARENA_MAX=2`** set in the wrapper, or link mimalloc. glibc's
   per-thread arenas otherwise inflate RSS on a threaded app for no data reason.
8. **Drop the repo handle** when a tab closes; do not keep a global repo map.

**CI gate:** a `memcheck` test clones a fixture repo, drives a scripted scroll
through history, samples `/proc/self/status:VmRSS`, and fails the build over the
ceiling. Without the gate the budget rots in one sprint.

---

## Authentication

### Primary: GitHub App + OAuth Device Flow

Register a **GitHub App** (not an OAuth App). Ship only the public `client_id`.
No client secret is embedded — device flow does not need one, which matters
because a secret in a distributed binary is readable with `strings`.

GitHub App over OAuth App because it yields 8-hour user tokens plus refresh
tokens and fine-grained per-resource permissions, instead of one non-expiring
token with coarse scopes.

Flow:

1. `POST https://github.com/login/device/code` with `client_id` + `scope`
2. Display `user_code`, copy it to clipboard, open `verification_uri` via
   `gtk::UriLauncher`
3. Poll `POST /login/oauth/access_token` every `interval` seconds
4. Handle `authorization_pending` (keep polling), `slow_down` (increase
   interval — mandatory, GitHub will hard-fail otherwise), `expired_token`
   (restart), `access_denied` (abort)
5. Store `access_token` + `refresh_token` + expiry
6. Refresh proactively at expiry minus 5 minutes; on 401 refresh once then
   re-authenticate

No localhost listener, no custom URI scheme registration, and it works over SSH
on a headless box.

### Token storage

`keyring` crate v4 → Secret Service (gnome-keyring / KWallet / COSMIC's
provider). Key: `forqen:{host}:{account_login}`.

Explicitly **not** a JSON file in `~/.config`. If no Secret Service is present,
fail loudly with instructions — do not silently downgrade to plaintext.

### Additional paths

- **Import from `gh`.** `gh auth token` already works on this machine
  (authenticated as `n1th1n-19`, keyring-backed). Offer this on first run for
  a zero-friction start.
- **PAT paste.** Required for GitHub Enterprise Server, which may not have the
  App installed. Validate against `/user` before saving.
- **Multi-account from day one.** An `accounts` table with
  `(host, login, token_ref, default_for_host)`. Retrofitting multi-account is a
  schema migration plus a rewrite of every API call site — cheap now, expensive
  later.

### Git transport auth ≠ API auth

Separate concern, separate resolution order:

1. **ssh-agent** if the remote is SSH (`SSH_AUTH_SOCK`) — never touch private
   key files directly
2. **HTTPS** → OAuth token as password, `x-access-token` as username
3. Optional: install forqen as a `git credential helper`
   (`git config credential.helper forqen`) so the terminal shares the same
   keyring entry

### Scopes requested

`repo`, `read:org`, `workflow`, `gist`, `notifications`, `user:email`.
Request incrementally where possible — do not ask for `workflow` until the user
opens the Actions view.

### Security notes

- Never log token values; redact in all error paths including panic hooks
- Flatpak sandbox needs explicit holes: `--socket=ssh-auth`,
  `--filesystem=home`, `--talk-name=org.freedesktop.secrets`
- Verify commit signatures (GPG and SSH) and surface a verification badge;
  GitHub Desktop shows nothing here
- Device-flow poll must respect `slow_down` — ignoring it gets the App
  rate-limited for every user, not just one

---

## GitHub API layer

- **REST via `octocrab` 0.54** for simple resources (repos, branches, releases,
  Actions runs).
- **GraphQL for PR review threads.** REST v3 returns review comments as a flat
  list with `in_reply_to_id` — reconstructing threads client-side is fragile.
  GraphQL `reviewThreads` returns them already threaded, and lets one query
  fetch PR + files + threads + checks instead of five round trips.
- **ETag conditional requests.** Store `ETag` per endpoint in SQLite; send
  `If-None-Match`. A 304 does not count against the rate limit — this is the
  whole reason a poll-based notifications inbox is affordable.
- **Rate limiter** reading `X-RateLimit-Remaining` / `Reset`, with a shared
  semaphore and exponential backoff on secondary limits.
- **Offline-first.** Every fetched entity lands in SQLite. On no network the UI
  renders cached data read-only with a staleness banner, rather than an error
  page.

---

## Release phases

### R0 — Skeleton, auth, history *(~4 weeks)*

Ship a window that opens a repo and browses history. Nothing else.

- Meson + cargo build; `.desktop`, AppStream metainfo, GSettings schema
- **Flatpak manifest working in R0**, not deferred — sandbox holes for
  ssh-agent and Secret Service are architectural and painful to retrofit
- `auth`: device flow, keyring, `gh` token import, multi-account schema
- `git`: repo open, windowed revwalk, commit-graph, ref list
- UI: `AdwApplicationWindow`, sidebar (repos/branches), `GtkColumnView` commit
  list with the windowed model, commit detail pane
- Clone dialog: paste URL, or pick from the authenticated user's repo list
- **Memory gate wired into CI in R0** — a budget added later is a budget missed

### R1 — Local git parity *(~5 weeks)*

Everything GitHub Desktop does locally, plus what it refuses to do.

- Working-tree status, hunk-level **and line-level** staging
- Commit box with signing (GPG + SSH), co-author trailers, amend
- Diff viewer on `GtkSourceView5`: syntax highlight, word-level intra-line diff,
  whitespace toggle, side-by-side and unified
- Branch create/switch/delete/rename, checkout with dirty-tree guard
- Fetch / pull / push with progress and force-with-lease
- Merge, and a proper **3-way conflict resolver** (ours / theirs / merged panes)
- Stash: create, apply, pop, drop, with diff preview
- `.gitignore` editing, file history, per-file log

### R2 — Pull requests *(~5 weeks)*

The reason to leave the browser.

- PR list per repo: filters, search, author/label/review-state facets
- PR detail: description, timeline, commits, changed files
- **Review flow**: inline comments on any line, multi-line comment ranges,
  suggested changes, batch review submit (approve / request changes / comment)
- Checkout a PR branch in one click, including from a fork
- Checks/CI status inline with per-check log links
- Create PR from the current branch with template auto-fill
- Merge / squash / rebase-merge with branch cleanup
- Draft PRs, reviewer and assignee management

### R3 — Issues, notifications, Actions *(~4 weeks)*

- Issues: list, filter, create, edit, comment, close; labels, milestones,
  assignees; markdown editor with live preview and slash commands
- **Notifications inbox**: unified, keyboard-driven triage (`e` archive,
  `u` unsubscribe), ETag-polled so it costs almost no rate limit
- Actions: workflow run list, job breakdown, **live log tail**, re-run failed
  jobs, cancel, download artifacts
- Projects (v2) read-only board view

### R4 — Advanced git *(~5 weeks)*

The features that make this better than GitHub Desktop rather than equal to it.

- **Interactive rebase editor** — drag to reorder, pick/squash/fixup/edit/drop,
  live preview of the resulting history. Drives the `git` binary, not gix.
- **Worktree manager** — review a PR without stashing your current work. Nothing
  in GitHub Desktop does this, and it is the single biggest workflow win.
- **Reflog browser / "undo anything"** — recovery UI over `git reflog`, so a bad
  reset is one click back rather than a stack-overflow search
- Blame view with commit hover, and **"which PR introduced this line"**
  (blame → commit SHA → GraphQL `associatedPullRequests`)
- Cherry-pick, revert, bisect assistant
- Submodule support; Git LFS awareness with lock status
- Patch import/export (`format-patch` / `am`)

### R5 — Breadth and polish *(~4 weeks)*

- **Command palette** (`Ctrl+Shift+P`) — every action reachable without a mouse
- GitHub code search + local repo-wide search (`gix` pathspec, ripgrep-style)
- Releases and tags: browse, create, edit notes, upload assets
- Gists: list, create, edit
- Repo settings, collaborators, branch protection view
- Repo health tools: large-file finder, stale-branch pruner, `gc` runner
- `cli` crate: credential helper, `forqen open`, `forqen pr checkout <n>`,
  sharing the keyring entry with the GUI
- Session restore, per-repo state, COSMIC/GNOME theme integration

### R6 — Hardening *(ongoing)*

Fuzz the diff and porcelain parsers, profile against monorepos, accessibility
pass (Orca + full keyboard nav), i18n via gettext, Flathub submission.

---

## Your calls to make during implementation

Two decisions where the trade-off is yours, not mine. I will scaffold the
signature and surrounding code, and leave the body:

1. **`crates/auth/src/device_flow.rs` → `handle_poll_response()`**
   The device-flow polling state machine. `slow_down` handling, retry ceiling,
   and what happens on `expired_token` mid-wait. Aggressive polling gets the
   *App* rate-limited across all users; conservative polling makes login feel
   broken. Roughly 8 lines, and it sets the login UX.

2. **`crates/git/src/history/window.rs` → `evict()`**
   Which commit rows the windowed model drops when scrolling. Pure LRU thrashes
   on scroll-up; a keep-anchor-plus-bidirectional-band costs more memory but
   makes reverse scrolling instant. This is the memory ceiling in one function.

---

## Verification

**Prerequisites to install first** (all currently missing on this machine):

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev \
                 meson flatpak-builder
```

Present already: `rustc` 1.96.0, `git` 2.43.0, `gh` (authed as `n1th1n-19`),
`flatpak`.

**Per-phase checks:**

| What | How |
|---|---|
| Git engine correctness | `cargo test -p git` against fixture repos built by script; **differential test** — every operation compared byte-for-byte against the `git` binary's output on the same fixture |
| Memory ceiling | `cargo test -p app --test memcheck` — clones `torvalds/linux`, scripted scroll, samples `VmRSS`, asserts under budget. Runs in CI on every PR. |
| Auth | Manual: revoke in GitHub settings → relaunch → device flow completes → token lands in keyring (verify with `secret-tool search service forqen`) → restart app → still logged in |
| Token never leaks | `grep -r` over logs after a full session; panic-hook redaction unit test |
| API layer | `wiremock` fixtures for REST + GraphQL; assert ETag revalidation issues `If-None-Match` and a 304 does not refetch |
| UI | `cargo test` on view models (no GTK); manual smoke per phase; `broadwayd` headless for CI screenshots |
| Flatpak | `flatpak-builder --user --install build build-aux/io.github.forqen.Forqen.yml` then verify ssh-agent push and keyring access **inside** the sandbox — this is where sandbox holes are proven, not on the host |
| Perf | `criterion` benches on revwalk, status, and diff against small / medium / `linux` repos; regression-gated |

**R0 acceptance:** `flatpak run io.github.forqen.Forqen` → device-flow login →
clone a repo → scroll 100k commits smoothly → RSS under 140 MB.

---

## Risks

| Risk | Mitigation |
|---|---|
| gix API churn (0.x, frequent breaking releases) | Confine gix behind the `GitBackend` trait; pin exact versions; upgrade deliberately |
| gix gaps beyond rebase (push negotiation, LFS) | Shell-out path already exists — widen it rather than reimplement |
| Scope is genuinely large | Each phase ships standalone. R0+R1 alone already beats gitg; R2 already beats the browser |
| Flatpak sandbox blocks ssh-agent or keyring | Proven in R0, not discovered in R6 |
| GraphQL schema changes | Pin the schema, generate types with `cynic`/`graphql_client` so breakage is a compile error |
| GitHub App review for public distribution | Start registration during R0; it is calendar time, not work time |
