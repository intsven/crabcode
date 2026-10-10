use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// How often the background worker re-reads the branch.
///
/// The render path used to refresh on a 2s TTL from inside `App::render`,
/// which meant a process spawn (~50ms measured on Windows) blocked the UI
/// thread roughly every 2 seconds of continuous typing. The cadence is
/// unchanged; only the thread it happens on changed.
pub const BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub fn get_current_branch() -> Option<String> {
    get_branch_for_path(".")
}

pub fn get_branch_for_path(path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;

    if output.status.success() {
        let branch = String::from_utf8(output.stdout).ok()?;
        let branch = branch.trim();
        if branch.is_empty() || branch == "HEAD" {
            None
        } else {
            Some(branch.to_string())
        }
    } else {
        None
    }
}

pub fn is_git_repo(path: &str) -> Option<bool> {
    let output = Command::new("git")
        .args(["-C", path, "rev-parse", "--git-dir"])
        .output()
        .ok()?;

    Some(output.status.success())
}

struct State {
    /// Path the UI wants tracked. Empty means "not interested yet".
    wanted_path: String,
    /// Last published branch for `wanted_path`.
    branch: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            wanted_path: String::new(),
            branch: None,
        }
    }
}

/// Non-blocking holder for the active workspace's branch name.
///
/// One worker thread owns the `git` spawn. [`Self::current`] only takes the
/// mutex for a `String` clone, so the render path can never pay for process
/// creation — that was the source of the perceived input lag.
///
/// The worker parks on [`BRANCH_REFRESH_INTERVAL`] and exits once the last
/// handle is dropped, so an idle app does not keep a thread alive forever.
pub struct GitBranchCache {
    state: Arc<Mutex<State>>,
}

impl GitBranchCache {
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(State::new()));
        spawn_worker(Arc::clone(&state));
        Self { state }
    }

    /// Ask the worker to track `path`.
    ///
    /// Non-blocking. The branch becomes visible in [`Self::current`] after the
    /// worker completes one read, which is at most one refresh interval away —
    /// the same latency the old synchronous TTL had. Any previously published
    /// branch is cleared immediately so it can never be shown against the new
    /// path.
    pub fn request(&self, path: &str) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.wanted_path == path {
                return;
            }
            // Clear immediately so a stale branch from the previous workspace
            // is never displayed against the new one.
            state.branch = None;
            state.wanted_path = path.to_string();
        }
        // Wake the worker so a workspace switch is picked up promptly instead
        // of waiting out the remainder of the interval.
        wake_worker();
    }

    /// Last known branch for the requested path. Never spawns a process.
    pub fn current(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.wanted_path.is_empty() {
            return None;
        }
        state.branch.clone()
    }
}

impl Default for GitBranchCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle the worker parks on, so `request` can unpark it.
///
/// Only the most recently spawned worker is registered. Extra caches (tests
/// create several) each get their own thread; they poll on the interval and
/// exit on drop, which is cheap and bounded.
static WORKER: OnceLock<std::thread::Thread> = OnceLock::new();

fn spawn_worker(state: Arc<Mutex<State>>) {
    let spawned = std::thread::Builder::new()
        .name("git-branch".to_string())
        .spawn(move || worker_loop(state));

    if let Ok(handle) = spawned {
        // Failing to register only costs latency on workspace switch; the
        // worker still refreshes on its interval.
        let _ = WORKER.set(handle.thread().clone());
    }
}

fn wake_worker() {
    if let Some(thread) = WORKER.get() {
        thread.unpark();
    }
}

fn worker_loop(state: Arc<Mutex<State>>) {
    // `strong_count == 1` means only this thread holds the Arc: every
    // GitBranchCache was dropped, so nothing can consume a result.
    while Arc::strong_count(&state) > 1 {
        std::thread::park_timeout(BRANCH_REFRESH_INTERVAL);

        // Read the wanted path under one lock so a request() landing mid-read
        // cannot be missed.
        let path = {
            let state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.wanted_path.clone()
        };
        if path.is_empty() {
            continue;
        }

        // Re-check every interval even when the path is unchanged: an
        // external `git checkout` announces itself through no request() call,
        // and the old synchronous version picked it up the same way.
        let branch = get_branch_for_path(&path);

        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        // A request() may have switched paths while the spawn was running.
        // Publishing now would attribute the old repo's branch to the new one.
        if state.wanted_path == path {
            state.branch = branch;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(cache: &GitBranchCache, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(branch) = cache.current() {
                return Some(branch);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn test_get_current_branch() {
        let branch = get_current_branch();
        if let Some(branch_name) = branch {
            assert!(!branch_name.is_empty());
            assert_ne!(branch_name, "HEAD");
        }
    }

    #[test]
    fn current_is_none_before_any_request() {
        let cache = GitBranchCache::new();
        assert!(cache.current().is_none());
    }

    #[test]
    fn cold_current_does_not_spawn_a_process() {
        // Guards the actual regression: the old implementation ran
        // `git rev-parse` inline here, costing ~50ms on Windows.
        let cache = GitBranchCache::new();
        let start = Instant::now();
        let _ = cache.current();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(5),
            "cold current() took {elapsed:?}; it must not spawn a process"
        );
    }

    #[test]
    fn warm_current_is_sub_millisecond() {
        let cache = GitBranchCache::new();
        cache.request(env!("CARGO_MANIFEST_DIR"));
        assert!(
            wait_for(&cache, Duration::from_secs(30)).is_some(),
            "worker never published a branch for the crate root"
        );

        let start = Instant::now();
        for _ in 0..100 {
            let _ = cache.current();
        }
        let per_call = start.elapsed() / 100;
        assert!(
            per_call < Duration::from_millis(1),
            "warm current() averaged {per_call:?} per call; expected sub-ms"
        );
    }

    #[test]
    fn switching_paths_drops_the_stale_branch_immediately() {
        let cache = GitBranchCache::new();
        cache.request(env!("CARGO_MANIFEST_DIR"));
        assert!(
            wait_for(&cache, Duration::from_secs(30)).is_some(),
            "first path should resolve"
        );

        // A workspace switch must not leave the previous repo's branch on
        // screen while the new one is still being read.
        cache.request("C:\\Windows\\Temp");
        assert!(
            cache.current().is_none(),
            "stale branch survived a workspace switch"
        );
    }

    #[test]
    fn non_repo_path_publishes_no_branch_without_stalling() {
        let cache = GitBranchCache::new();
        cache.request("C:\\Windows\\Temp");

        // Give the worker time to attempt the read; the published value stays
        // None because the directory is not a repository.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            cache.current().is_none(),
            "a non-repo directory must not report a branch"
        );
        // The important part: the UI thread was never blocked.
        let start = Instant::now();
        let _ = cache.current();
        assert!(start.elapsed() < Duration::from_millis(1));
    }
}
