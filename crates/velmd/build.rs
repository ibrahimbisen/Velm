//! Stamp the commit this binary was built from into the binary itself.
//!
//! `velmd` reports it on `/api/v1/health` and in its startup banner, so a deploy can ask the
//! **running process** what it is rather than printing `git rev-parse HEAD` — the checkout,
//! which is a different claim and was the one being made. A run where `cargo` correctly had
//! nothing to rebuild and a run where the new binary never got picked up printed the same
//! green banner; this is what tells them apart.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // ⚠ **Watching `.git/HEAD` alone does not work, and the way it fails is silent.**
    // On a `git pull` that stays on the same branch, `refs/heads/<branch>` moves while
    // `.git/HEAD` keeps its literal `ref: refs/heads/<branch>` text and its mtime never
    // changes. So on exactly the commit this stamp exists for — one that touches nothing in
    // velmd's own dependency tree, where cargo therefore rebuilds nothing — the build script
    // would not rerun, the binary would keep the *previous* commit's stamp, and the deploy's
    // verification gate would reject a perfectly good deploy for ever after. The ref itself
    // is the file that moves, so the ref itself is the file to watch.
    let git = git_dir();
    if let Some(git) = &git {
        println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
        // A ref `git gc` has packed away is not a file any more, so watch the pack as well.
        println!("cargo:rerun-if-changed={}", git.join("packed-refs").display());
        if let Some(reference) = head_ref(git) {
            println!("cargo:rerun-if-changed={}", git.join(reference).display());
        }
    }
    // An explicit override for a build with no `.git` beside it — a tarball, a container.
    println!("cargo:rerun-if-env-changed=VELM_GIT_SHA");

    // The full 40 characters, never abbreviated: it is compared for equality against
    // `git rev-parse HEAD` by the deploy, and shortened only where it is printed.
    let sha = std::env::var("VELM_GIT_SHA").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(
        || {
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                // Never a build failure: a source tree without git in it still has to compile.
                // "unknown" is honest, and the deploy gate refuses it on sight.
                .unwrap_or_else(|| "unknown".to_owned())
        },
    );
    println!("cargo:rustc-env=VELM_GIT_SHA={sha}");
}

/// The repository's `.git`, walking up from this crate — a worktree's `.git` is a *file*
/// naming the real directory, so that case is read rather than assumed to be a directory.
fn git_dir() -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    for dir in manifest.ancestors() {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let path = text.strip_prefix("gitdir:")?.trim();
            let path = Path::new(path);
            return Some(if path.is_absolute() { path.to_owned() } else { dir.join(path) });
        }
    }
    None
}

/// `refs/heads/<branch>` for an attached HEAD; `None` when it is detached, where there is no
/// ref to watch and `.git/HEAD` itself holds the sha and does move.
fn head_ref(git: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    Some(head.strip_prefix("ref:")?.trim().to_owned())
}
