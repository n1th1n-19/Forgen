//! Memory budget gate.
//!
//! forqen's differentiating claim is that it holds a flat RSS curve over
//! histories that make Electron clients swap. A claim nobody measures rots
//! within a sprint, so this runs in CI on every change.
//!
//! The fixture is built with `git fast-import` rather than by spawning `git
//! commit` in a loop: 20k commits is seconds one way and many minutes the
//! other, and a gate slow enough to skip is a gate nobody runs.
//!
//! Set `FORQEN_MEMCHECK_REPO=/path/to/linux` to run against a real large
//! repository instead of the synthetic one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use git::history::{HistoryWindow, Walker};
use git::Repo;

/// Synthetic history size. Large enough that a non-windowed implementation
/// would be obviously over budget, small enough to build in CI in seconds.
const COMMITS: usize = 20_000;

/// Ceiling on RSS *growth* from opening and scrolling the whole history.
///
/// Growth rather than absolute, because the baseline differs between a debug
/// and a release binary and between libc versions — what must stay flat is the
/// part that scales with history size.
const GROWTH_CEILING_MB: u64 = 64;

fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("procfs is required");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .expect("VmRSS present in /proc/self/status")
}

/// Build a repo with `n` commits via fast-import.
fn fast_import_fixture(dir: &Path, n: usize) {
    let ok = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir)
        .status()
        .expect("git init");
    assert!(ok.success());

    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import");

    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        let mut w = std::io::BufWriter::new(stdin);
        for i in 0..n {
            // A fixed timestamp keeps the fixture reproducible; the walk order
            // is what matters here, not the dates.
            writeln!(w, "commit refs/heads/main").unwrap();
            writeln!(w, "mark :{}", i + 1).unwrap();
            writeln!(
                w,
                "committer Fixture <fixture@example.invalid> {} +0000",
                1_600_000_000 + i
            )
            .unwrap();
            let msg = format!("commit {i}");
            writeln!(w, "data {}", msg.len()).unwrap();
            writeln!(w, "{msg}").unwrap();
            if i > 0 {
                writeln!(w, "from :{i}").unwrap();
            }
            let blob = format!("{i}\n");
            writeln!(w, "M 100644 inline f.txt").unwrap();
            writeln!(w, "data {}", blob.len()).unwrap();
            write!(w, "{blob}").unwrap();
            writeln!(w).unwrap();
        }
        writeln!(w, "done").unwrap();
        w.flush().unwrap();
    }

    let status = child.wait().expect("fast-import completes");
    assert!(status.success(), "fast-import failed");
}

/// One test, not several.
///
/// `VmRSS` is a property of the process, and cargo runs a test binary's tests
/// as threads in one process — so two tests that both sample RSS measure each
/// other's allocations. Splitting this would produce a gate that fails
/// depending on thread scheduling, which is worse than no gate.
#[test]
fn scrolling_a_large_history_stays_within_budget() {
    let (path, _guard): (PathBuf, Option<tempfile::TempDir>) =
        match std::env::var_os("FORQEN_MEMCHECK_REPO") {
            Some(p) => (PathBuf::from(p), None),
            None => {
                let dir = tempfile::tempdir().expect("tempdir");
                fast_import_fixture(dir.path(), COMMITS);
                (dir.path().to_path_buf(), Some(dir))
            }
        };

    let baseline = rss_kb();

    let repo = Repo::open(&path).expect("fixture opens");
    let mut window = HistoryWindow::new();
    window
        .fill_spine(Walker::from_head(&repo).expect("walk from HEAD"))
        .expect("spine");
    let total = window.len();
    assert!(
        total > 1_000,
        "fixture should be large; got {total} commits"
    );

    // Scroll the entire history in viewport-sized steps, the way a user
    // dragging the scrollbar to the bottom would.
    let viewport = 40;
    let mut step = 0;
    while step + viewport < total {
        window
            .ensure(&repo, step..step + viewport)
            .expect("hydrate viewport");
        assert!(
            window.realized() <= window.budget(),
            "realized {} exceeded budget {} at row {step}",
            window.realized(),
            window.budget()
        );
        step += viewport;
    }

    // And back up again — the direction that a FIFO eviction policy would
    // thrash on.
    while step > viewport {
        step -= viewport;
        window
            .ensure(&repo, step..step + viewport)
            .expect("hydrate");
    }

    let growth_mb = rss_kb().saturating_sub(baseline) / 1024;
    assert!(
        growth_mb <= GROWTH_CEILING_MB,
        "RSS grew {growth_mb}MB scrolling {total} commits, ceiling is \
         {GROWTH_CEILING_MB}MB. Either the windowed model stopped windowing, \
         or something started retaining rows."
    );

    eprintln!(
        "memcheck: {total} commits, RSS growth {growth_mb}MB (ceiling {GROWTH_CEILING_MB}MB)"
    );
}

/// The spine's cheapness is a structural property, so assert it structurally
/// rather than with a second RSS probe.
///
/// An RSS measurement here would be dominated by gix's own revwalk state — the
/// seen-set and the 16MB object cache — neither of which is what the windowed
/// design is claiming about. That `realized()` stays at zero across a full walk
/// is the actual invariant: it is what guarantees no `CommitRow`, and therefore
/// no heap strings, exist for unviewed commits.
#[test]
fn walking_the_spine_hydrates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    fast_import_fixture(dir.path(), 2_000);

    let repo = Repo::open(dir.path()).expect("opens");
    let mut window = HistoryWindow::new();
    window
        .fill_spine(Walker::from_head(&repo).expect("walk"))
        .expect("spine");

    assert_eq!(window.len(), 2_000);
    assert_eq!(
        window.realized(),
        0,
        "walking must not hydrate rows — the spine holds ids only"
    );
}
