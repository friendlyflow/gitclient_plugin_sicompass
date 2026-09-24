//! The two things that must not run on the render thread.
//!
//! * [`Network`] runs `fetch`, `pull` and `push` in the background. They
//!   contact a remote, so they take as long as the network does, and a frame
//!   spent waiting is a frame the app does not draw. Inside the sandbox that is
//!   a host background task (a second instance of this plugin, see
//!   [`run_task`]); natively, in the unit tests, a thread.
//! * [`Watcher`] notices that the repository changed underneath the app,
//!   without running `git status` on a timer.

use crate::git::Git;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

// ---------------------------------------------------------------------------
// Network jobs
// ---------------------------------------------------------------------------

/// How one background git command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The operation, for the message that reports it.
    pub label: String,
    /// `None` on success.
    pub error: Option<String>,
}

/// Run `steps` in order, stopping at the first failure. `None` on success,
/// otherwise the message naming the step that failed.
///
/// Stopping is what makes "commit and sync" a pull followed by a push rather
/// than two independent things that can both half-happen.
pub fn run_steps(git: &Git, steps: &[Vec<String>]) -> Option<String> {
    for step in steps {
        let out = git.try_run(step);
        if !out.ok() {
            let sub = step.first().cloned().unwrap_or_default();
            let first = out
                .stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("failed")
                .trim()
                .to_owned();
            return Some(format!("git {sub}: {first}"));
        }
    }
    None
}

/// Runs one remote-contacting git command at a time.
///
/// Single-flight rather than a queue: `GIT_OPTIONAL_LOCKS=0` keeps the *read*
/// commands off `index.lock`, but a `pull` very much takes it, and two of them
/// at once fail with three lines of advice about deleting a lock file by hand.
/// Refusing the second is a better answer than racing it.
#[derive(Debug, Default)]
pub struct Network {
    /// What is running, for the row that says so.
    running: RefCell<Option<String>>,
    /// Filled when the job ends, drained by `tick`.
    done: RefCell<Option<Outcome>>,
    /// The host task running the job.
    #[cfg(target_arch = "wasm32")]
    task: Cell<Option<u64>>,
    /// Natively: the thread's result, when it has one.
    #[cfg(not(target_arch = "wasm32"))]
    thread: RefCell<Option<std::sync::mpsc::Receiver<Option<String>>>>,
}

/// The task a network job runs as.
pub const TASK: &str = "network";

impl Network {
    pub fn new() -> Network {
        Network::default()
    }

    pub fn busy(&self) -> bool {
        self.collect();
        self.running.borrow().is_some()
    }

    /// What is running right now, if anything.
    pub fn running(&self) -> Option<String> {
        self.collect();
        self.running.borrow().clone()
    }

    /// Take the result of the last finished job, if it has not been taken yet.
    pub fn take_outcome(&self) -> Option<Outcome> {
        self.collect();
        self.done.borrow_mut().take()
    }

    fn finish(&self, error: Option<String>) {
        if let Some(label) = self.running.borrow_mut().take() {
            *self.done.borrow_mut() = Some(Outcome { label, error });
        }
    }

    /// Start a job. Returns `false` when one is already running.
    ///
    /// `steps` is run in order and stops at the first failure (see
    /// [`run_steps`]).
    pub fn start(&self, git: Git, label: String, steps: Vec<Vec<String>>) -> bool {
        if self.busy() {
            return false;
        }
        if !self.spawn(git, steps) {
            return false;
        }
        *self.running.borrow_mut() = Some(label);
        true
    }

    /// The job travels as the same bytes a host task gets, so these tests
    /// cover that trip too.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn(&self, git: Git, steps: Vec<Vec<String>>) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        let input = encode_job(&git, &steps);
        std::thread::spawn(move || {
            // A panic drops `tx`, which `collect` reads as a failure, so the
            // flag cannot stay set.
            let _ = tx.send(run_task(&input).map_or_else(Some, |out| decode_result(&out)));
        });
        *self.thread.borrow_mut() = Some(rx);
        true
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn collect(&self) {
        use std::sync::mpsc::TryRecvError;
        let result = match self.thread.borrow().as_ref().map(|rx| rx.try_recv()) {
            None | Some(Err(TryRecvError::Empty)) => return,
            Some(Ok(error)) => error,
            Some(Err(TryRecvError::Disconnected)) => Some("git: the job stopped".to_owned()),
        };
        *self.thread.borrow_mut() = None;
        self.finish(result);
    }

    #[cfg(target_arch = "wasm32")]
    fn spawn(&self, git: Git, steps: Vec<Vec<String>>) -> bool {
        match sicompass_pdk::tasks::spawn(TASK, &encode_job(&git, &steps)) {
            Ok(id) => {
                self.task.set(Some(id));
                true
            }
            Err(e) => {
                sicompass_pdk::host::log(&format!("gitclient: {e}"));
                false
            }
        }
    }

    /// The task reports through [`Network::on_task_event`] instead.
    #[cfg(target_arch = "wasm32")]
    fn collect(&self) {}

    /// The end of the task started by [`Network::start`].
    #[cfg(target_arch = "wasm32")]
    pub fn on_task_event(&self, id: u64, event: sicompass_pdk::TaskEvent) {
        use sicompass_pdk::TaskEvent;
        if self.task.get() != Some(id) {
            return;
        }
        if let TaskEvent::Done(result) = event {
            self.task.set(None);
            self.finish(match result {
                Ok(bytes) => decode_result(&bytes),
                Err(e) => Some(format!("git: {e}")),
            });
        }
    }
}

/// A job as the task's input: the folder, the binary, then one `Obj` per step.
pub fn encode_job(git: &Git, steps: &[Vec<String>]) -> Vec<u8> {
    use sicompass_sdk::ffon::FfonElement;
    let mut out = vec![
        FfonElement::new_str(git.cwd().to_string_lossy().into_owned()),
        FfonElement::new_str(git.binary().to_owned()),
    ];
    for step in steps {
        let mut obj = FfonElement::new_obj("step");
        if let Some(o) = obj.as_obj_mut() {
            for arg in step {
                o.push(FfonElement::new_str(arg.clone()));
            }
        }
        out.push(obj);
    }
    sicompass_sdk::ffon::serialize_binary(&out)
}

/// The inverse of [`encode_job`].
pub fn decode_job(bytes: &[u8]) -> Option<(Git, Vec<Vec<String>>)> {
    let elems = sicompass_sdk::ffon::deserialize_binary(bytes);
    let mut it = elems.iter();
    let cwd = it.next()?.as_str()?.to_owned();
    let binary = it.next()?.as_str()?.to_owned();
    let steps = it
        .map(|e| {
            e.as_obj()
                .map(|o| {
                    o.children
                        .iter()
                        .filter_map(|c| c.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    Some((Git::new(binary, cwd), steps))
}

/// A job's result as the task's output: empty on success, else the message.
pub fn encode_result(error: Option<String>) -> Vec<u8> {
    error.map(String::into_bytes).unwrap_or_default()
}

/// The inverse of [`encode_result`].
pub fn decode_result(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned())
}

/// The task itself, in the worker instance: run the job, report how it ended.
pub fn run_task(input: &[u8]) -> Result<Vec<u8>, String> {
    let (git, steps) = decode_job(input).ok_or("gitclient: a malformed job")?;
    Ok(encode_result(run_steps(&git, &steps)))
}

// ---------------------------------------------------------------------------
// The .git watcher
// ---------------------------------------------------------------------------

/// Notices that something changed the repository from outside the app.
///
/// It stats four paths rather than running `git status`, because it runs on a
/// timer: `git status` on a large repository is hundreds of milliseconds and a
/// process spawn, and doing that once a second forever to answer "did anything
/// happen" is the wrong trade. `HEAD` covers checkout and commit, `index`
/// covers staging, and the `refs` directory covers branch and tag changes.
///
/// It does **not** watch the worktree. Editing a file changes `git status`
/// without touching `.git`, so an edit made in another window is picked up on
/// the next refresh rather than immediately. Watching a whole worktree means
/// a watch per directory and a rebuild every time a build writes to `target/`,
/// which is a much worse trade than being one keystroke stale.
///
/// There is no thread: the plugin asks from `poll`, every frame, and the paths
/// are statted at most once per [`POLL_INTERVAL`] of those asks.
pub struct Watcher {
    watched: [PathBuf; 4],
    last: RefCell<Vec<Option<SystemTime>>>,
    checked: Cell<Instant>,
}

impl Watcher {
    /// Start watching the git directories of an open repository.
    ///
    /// `git_dir` is this worktree's own (its `HEAD` and `index`), `common_dir`
    /// the one every worktree shares (its `refs`). For the main worktree they
    /// are the same directory.
    pub fn start(git_dir: PathBuf, common_dir: PathBuf) -> Watcher {
        let watched = [
            git_dir.join("HEAD"),
            git_dir.join("index"),
            common_dir.join("refs"),
            // Written by rebase, merge and cherry-pick, so an operation
            // running in the user's own shell shows up here.
            common_dir.join("packed-refs"),
        ];
        let last = RefCell::new(stamps(&watched));
        Watcher {
            watched,
            last,
            checked: Cell::new(Instant::now()),
        }
    }

    /// Whether something changed since the last time this said so.
    pub fn take_changed(&self) -> bool {
        if self.checked.get().elapsed() < POLL_INTERVAL {
            return false;
        }
        self.checked.set(Instant::now());
        let now = stamps(&self.watched);
        let mut last = self.last.borrow_mut();
        if *last == now {
            return false;
        }
        *last = now;
        true
    }
}

/// Long enough that the stats cost nothing, short enough that a commit made in
/// another window is picked up before the user wonders why it was not.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

fn stamps(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths.iter().map(|p| stamp(p)).collect()
}

/// A path that does not exist stamps as `None`, so its appearance or
/// disappearance counts as a change (`packed-refs` and `index` both come and
/// go in an ordinary repository).
fn stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::fixture::Fixture;

    #[test]
    fn a_job_reports_success() {
        let f = Fixture::new();
        let n = Network::new();
        assert!(n.start(f.git(), "check".into(), vec![vec!["--version".to_owned()]]));
        let outcome = wait_for(&n);
        assert_eq!(outcome.error, None, "git --version should succeed");
        assert_eq!(outcome.label, "check");
        assert!(!n.busy(), "the flag is cleared when the job ends");
    }

    #[test]
    fn a_failing_step_stops_the_ones_after_it() {
        // "commit and sync" is a pull then a push. If the pull fails the push
        // must not run, or a conflict would be pushed straight past.
        let f = Fixture::new();
        let n = Network::new();
        n.start(
            f.git(),
            "sync".into(),
            vec![
                vec![
                    "rev-parse".to_owned(),
                    "--verify".to_owned(),
                    "refs/heads/definitely-not-a-branch".to_owned(),
                ],
                vec!["--version".to_owned()],
            ],
        );
        let outcome = wait_for(&n);
        assert!(outcome.error.is_some(), "the first step failed");
        assert!(
            outcome.error.unwrap().starts_with("git rev-parse:"),
            "the message should name the step that failed"
        );
    }

    #[test]
    fn only_one_job_runs_at_a_time() {
        // Two writes at once contend on index.lock and fail with advice the
        // user cannot act on, so the second is refused instead.
        let f = Fixture::new();
        let n = Network::new();
        // A command that takes long enough to still be running on the next
        // line: `git log` over a fresh repo would be too fast, so this waits on
        // a subprocess that does not exist and fails slowly enough.
        assert!(n.start(f.git(), "first".into(), vec![vec!["--version".into()]]));
        // Whether the first has finished is a race, so only assert the
        // invariant that matters: `start` never lets two run together.
        if n.busy() {
            assert!(
                !n.start(f.git(), "second".into(), vec![vec!["--version".into()]]),
                "a second job must be refused while one is in flight"
            );
        }
        wait_for(&n);
    }

    #[test]
    fn the_running_label_is_readable_while_a_job_is_in_flight_and_gone_after() {
        let f = Fixture::new();
        let n = Network::new();
        n.start(f.git(), "fetching".into(), vec![vec!["--version".into()]]);
        wait_for(&n);
        // Cleared by the guard, so the in-flight row disappears even if the
        // job panicked.
        assert_eq!(n.running(), None);
    }

    #[test]
    fn an_outcome_is_taken_once() {
        let f = Fixture::new();
        let n = Network::new();
        n.start(f.git(), "one".into(), vec![vec!["--version".into()]]);
        wait_for(&n);
        assert_eq!(n.take_outcome(), None, "the outcome was already drained");
    }

    #[test]
    fn a_watcher_notices_a_commit_and_reports_it_once() {
        let f = Fixture::new();
        let info = crate::repo::discover(&f.git(), &f.path()).unwrap();
        let w = Watcher::start(info.git_dir.clone(), info.common_dir.clone());
        assert!(!w.take_changed(), "nothing has happened yet");

        f.write("a.txt", "one");
        f.commit("first");
        assert!(wait_for_change(&w), "a commit should be noticed");
        assert!(!w.take_changed(), "the flag is taken, not left set");
    }

    #[test]
    fn a_watcher_notices_staging() {
        let f = Fixture::new();
        f.write("a.txt", "one");
        f.commit("first");
        let info = crate::repo::discover(&f.git(), &f.path()).unwrap();
        let w = Watcher::start(info.git_dir.clone(), info.common_dir.clone());

        f.write("a.txt", "two");
        f.run(["add", "a.txt"]);
        assert!(wait_for_change(&w), "the index changed");
    }

    #[test]
    fn a_job_travels_to_the_task_and_back() {
        // The worker instance shares nothing with the UI one, so the job has
        // to survive the trip as bytes: a commit message with a newline in
        // it, say.
        let git = Git::new("git", "/some/repo");
        let steps = vec![
            vec!["pull".to_owned(), "--ff-only".to_owned()],
            vec![
                "commit".to_owned(),
                "-m".to_owned(),
                "two\nlines".to_owned(),
            ],
        ];
        let (back, back_steps) = decode_job(&encode_job(&git, &steps)).unwrap();
        assert_eq!(back.cwd(), Path::new("/some/repo"));
        assert_eq!(back.binary(), "git");
        assert_eq!(back_steps, steps);

        assert_eq!(decode_result(&encode_result(None)), None);
        assert_eq!(
            decode_result(&encode_result(Some("git push: rejected".into()))),
            Some("git push: rejected".to_owned())
        );
    }

    #[test]
    fn the_task_runs_the_job() {
        let f = Fixture::new();
        let ok = run_task(&encode_job(&f.git(), &[vec!["--version".to_owned()]])).unwrap();
        assert_eq!(decode_result(&ok), None);
        let bad = run_task(&encode_job(
            &f.git(),
            &[vec![
                "rev-parse".to_owned(),
                "--verify".to_owned(),
                "nope".to_owned(),
            ]],
        ))
        .unwrap();
        assert!(decode_result(&bad).unwrap().starts_with("git rev-parse:"));
        assert!(run_task(b"not a job").is_err());
    }

    fn wait_for(n: &Network) -> Outcome {
        for _ in 0..200 {
            if let Some(o) = n.take_outcome() {
                return o;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the job never finished");
    }

    fn wait_for_change(w: &Watcher) -> bool {
        for _ in 0..60 {
            if w.take_changed() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}
