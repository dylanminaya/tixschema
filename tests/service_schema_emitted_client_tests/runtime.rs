//! What the runtime groups share: a workspace per run, the run itself, and the stand-down a
//! machine with no such runtime takes.

use core::sync::atomic::{AtomicU32, Ordering};
use std::env;
use std::env::temp_dir;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::io::stderr;
use std::path::PathBuf;
use std::process::{Command, id};
use std::sync::Mutex;

/// The process id alone does not separate two tests running beside each other.
static RUNS: AtomicU32 = AtomicU32::new(0);

/// The keys already reported absent: once per key, so two silent groups are not one.
static STOOD_DOWN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// A directory whose `node_modules` holds the packages a run imports; passed as `NODE_PATH`.
pub const NODE_MODULES_VAR: &str = "TIXSCHEMA_NODE_MODULES";

/// `None` unless every package in `required` has a directory under `TIXSCHEMA_NODE_MODULES`'s own
/// `node_modules`.
pub fn node_modules(required: &[&str]) -> Option<PathBuf> {
    let at = PathBuf::from(env::var(NODE_MODULES_VAR).ok()?).join("node_modules");
    required
        .iter()
        .all(|package| at.join(package).is_dir())
        .then_some(at)
}

/// A directory of its own per run.
fn workspace(named: &str) -> PathBuf {
    let nth = RUNS.fetch_add(1, Ordering::Relaxed);
    let at = temp_dir().join(format!("tixschema-emitted-{named}-{run}-{nth}", run = id()));
    if at.exists() {
        fs::remove_dir_all(&at).unwrap();
    }
    fs::create_dir_all(&at).unwrap();
    at
}

/// Writes `entry` into a workspace of its own, runs it under the named runtime with `env` applied
/// to the child process, and answers what the process wrote to stdout.
///
/// `None` says no runtime was reachable and nothing ran — never that a run passed. A runtime named
/// explicitly in `var` that cannot be started is a failure instead.
pub fn ran(
    named: &str,
    var: &str,
    fallback: &'static str,
    entry: &str,
    source: &str,
    env: &[(&str, &OsStr)],
) -> Option<String> {
    let chosen = env::var(var).ok();
    let runtime = chosen.clone().unwrap_or_else(|| fallback.to_owned());
    let at = workspace(named);
    fs::write(at.join(entry), source).unwrap();
    let run = Command::new(&runtime)
        .arg(entry)
        .current_dir(&at)
        .envs(env.iter().copied())
        .output();
    let Ok(reported) = run else {
        assert!(
            chosen.is_none(),
            "{var} names `{runtime}`, and no runtime could be started there: {}",
            run.unwrap_err()
        );
        stand_down(var, fallback);
        fs::remove_dir_all(&at).unwrap();
        return None;
    };
    fs::remove_dir_all(&at).unwrap();
    assert!(
        reported.status.success(),
        "`{runtime} {entry}` failed.\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- source ---\n{source}",
        String::from_utf8_lossy(&reported.stdout),
        String::from_utf8_lossy(&reported.stderr)
    );
    Some(String::from_utf8_lossy(&reported.stdout).into_owned())
}

/// Says `notice` on the process's own stderr, which `cargo test` does not capture — but only the
/// first time for a given `key`, so two silent groups sharing one reason are not reported twice.
fn once(key: &'static str, notice: &str) {
    let already = {
        let mut said = STOOD_DOWN.lock().unwrap();
        let seen = said.contains(&key);
        if !seen {
            said.push(key);
        }
        seen
    };
    if !already {
        drop(stderr().write_all(notice.as_bytes()));
    }
}

/// Said when no runtime named by `var`, nor `fallback` on `PATH`, could be started.
fn stand_down(var: &str, fallback: &'static str) {
    once(
        fallback,
        &format!(
            "\ntixschema: no `{fallback}` is reachable, so the emitted client was NOT run.\n  \
             That group stood down. Put `{fallback}` on PATH, or name one in {var}, and run \
             `just test-emitted`, which refuses to stand down.\n\n"
        ),
    );
}

/// Said when [`node_modules`] found no directory holding every package in `required`, naming the
/// `surface` that stood down (e.g. "the emitted WebSocket server").
pub fn stand_down_modules(required: &[&str], surface: &str) {
    let packages = required.join(", ");
    once(
        NODE_MODULES_VAR,
        &format!(
            "\ntixschema: no `{packages}` package is reachable through {NODE_MODULES_VAR}, so \
             {surface} was NOT run.\n  That group stood down. Set {NODE_MODULES_VAR} to a \
             directory whose `node_modules` holds {packages}, and run `just test-emitted`, which \
             refuses to stand down.\n\n"
        ),
    );
}
