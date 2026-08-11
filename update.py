#!/usr/bin/env python3
"""Update the Velm.app you are already using, without rebuilding the bundle.

    python3 update.py                    # build, swap the executable, relaunch
    python3 update.py --quick            # same, but skip LTO — 14s per edit instead of 80s
    python3 update.py --no-open          # don't relaunch it afterwards
    python3 update.py --dest /Applications
    python3 update.py --tests            # run the workspace tests first, stop if they fail

*"i keep having to run the build.py everytime i want to update my application … so the
updates that you do are immidietly applied to the app i am using so i dont have delte thre
package etc etc"*

`build.py` rebuilds the whole bundle — icon, Info.plist, plist lint, ad-hoc signature,
commit, push — and `scripts/make-app.sh` **deletes the existing bundle** before writing the
new one. That is right for shipping and is a great deal of work to repeat when the only
thing that changed is Rust code.

This swaps **just the executable** into the bundle that is already there. Nothing is
deleted and the bundle never moves, which matters for two reasons beyond speed: Finder
caches an icon per bundle *path*, so a bundle that stays put keeps its icon; and macOS
remembers per-app permissions by bundle identity, so replacing the whole thing can make the
system treat it as a new app.

⚠ **It cannot update the icon, the app name or the `.vellum` file association.** Those are
`Info.plist` and `Velm.icns`, which `make-app.sh` generates and this deliberately leaves
alone. Change one of those and you need `python3 build.py` once; after that, this again.

**There is no hot reload and there cannot be** — this is a compiled binary, so the floor is
one incremental `cargo build`: seconds when little changed, up to about two minutes cold
after `target/` has been cleared. What this removes is everything around the build.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent

# `make-app.sh` writes the binary here and names it in Info.plist's CFBundleExecutable.
# Both spellings have to agree with that file or the bundle will not launch.
BUNDLE_NAME = "Velm.app"
EXECUTABLE_NAME = "Velm"
# Where each profile leaves the binary. `--quick` trades link-time optimisation for build
# time: measured on this machine, rebuilding after one edited file in `vellum-app` is
# **1m20s** on `release` and **14.2s** on `quick`. The first `--quick` build is slower
# (**3m14s**) because the profile has its own `target/` subdirectory and every dependency
# compiles once; it pays for itself on the second edit. See the `[profile.quick]` comment in
# Cargo.toml for why this is not simply a debug build.
BINARY_FOR = {
    "release": REPO / "target" / "release" / "vellum-app",
    "quick": REPO / "target" / "quick" / "vellum-app",
}

# Swapping a busy binary fails, so a running app is asked to quit first. Politely: a hard
# kill skips `App::shut_down`, which flushes every open board — and RULE ZERO in CLAUDE.md
# is precisely about not being casual with the user's boards.
QUIT_TIMEOUT_SECONDS = 12
QUIT_POLL_SECONDS = 0.25

# An incremental release build is small, but a cold one after `rm -rf target` regrows the
# directory to several GB. Well under `build.py`'s 6GB, since no bundle is written here.
MIN_FREE_GB = 3


def step(message: str) -> None:
    print(f"▸ {message}")


def ok(message: str) -> None:
    print(f"  ✓ {message}")


def die(message: str, hint: str | None = None) -> None:
    print(f"  ✗ {message}", file=sys.stderr)
    if hint:
        print(f"    {hint}", file=sys.stderr)
    sys.exit(1)


def free_gb() -> float:
    return shutil.disk_usage(REPO).free / 1024**3


def velm_is_running() -> bool:
    """Whether a process named exactly `Velm` is up.

    `pgrep -x` matches the process name exactly, so a stray `vellum-app` run from
    `target/` — which is how the demos and screenshots are driven — is not mistaken for
    the installed bundle and does not get quit out from under whoever started it.
    """
    return subprocess.run(["pgrep", "-x", EXECUTABLE_NAME], capture_output=True).returncode == 0


def quit_velm() -> bool:
    """Asks a running Velm to quit, and waits for it to actually go.

    Returns False if it is still up after the timeout, which is a refusal to continue
    rather than something to force: overwriting the binary of a live process is how you
    get a half-written executable and a board that never flushed.
    """
    if not velm_is_running():
        return True
    step("quitting the running Velm (its boards flush on quit)")
    subprocess.run(
        ["osascript", "-e", f'quit app "{EXECUTABLE_NAME}"'],
        capture_output=True,
    )
    deadline = time.monotonic() + QUIT_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if not velm_is_running():
            ok("it quit cleanly")
            return True
        time.sleep(QUIT_POLL_SECONDS)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Swap a freshly built executable into an installed Velm.app."
    )
    parser.add_argument(
        "--dest",
        default=str(Path.home() / "Desktop"),
        help="folder holding Velm.app (default: ~/Desktop)",
    )
    parser.add_argument("--no-open", action="store_true", help="do not relaunch afterwards")
    parser.add_argument(
        "--quick",
        action="store_true",
        help="build without LTO — 14s instead of 80s per edit, no measured runtime cost",
    )
    parser.add_argument(
        "--tests",
        action="store_true",
        help="run `cargo test --workspace` first and stop if anything fails",
    )
    args = parser.parse_args()

    app = Path(args.dest).expanduser() / BUNDLE_NAME
    executable = app / "Contents" / "MacOS" / EXECUTABLE_NAME

    step("preflight")
    if not app.is_dir():
        die(
            f"no bundle at {app}",
            "run `python3 build.py` once to create it, then this keeps it up to date.",
        )
    if not executable.parent.is_dir():
        die(f"{app} has no Contents/MacOS — it is not a Velm bundle")
    free = free_gb()
    if free < MIN_FREE_GB:
        die(
            f"only {free:.1f}GB free on disk, need ~{MIN_FREE_GB}GB",
            "clear `target/` or free some space; CLAUDE.md has the standing rule.",
        )
    ok(f"{free:.0f}GB free, bundle at {app}")

    if args.tests:
        step("tests")
        if subprocess.run(["cargo", "test", "--workspace"], cwd=REPO).returncode != 0:
            die("tests failed — the app was not touched")
        ok("workspace tests passed")

    profile = "quick" if args.quick else "release"
    built = BINARY_FOR[profile]
    step(f"building ({profile})")
    build = ["cargo", "build", "--profile", profile, "-p", "vellum-app"]
    if subprocess.run(build, cwd=REPO).returncode:
        die("the build failed — the app was not touched")
    if not built.is_file():
        die(f"the build reported success but {built} is not there")
    ok(f"built ({built.stat().st_size / 1024**2:.0f}MB, {profile})")

    # Only now, once there is definitely something to install. Quitting the user's app and
    # then failing to build would be the worst of both.
    if not quit_velm():
        die(
            "Velm is still running and will not quit",
            "close it by hand and run this again — its executable cannot be replaced while "
            "it is live.",
        )

    step("swapping the executable")
    shutil.copy2(built, executable)
    ok(f"{executable}")

    # macOS refuses to launch a bundle whose signature no longer matches its contents, so
    # this is not optional and its failure is not swallowed. `make-app.sh` learned that
    # once already by ending its codesign with `2>/dev/null || true`, which hid the one
    # message that named a broken Info.plist.
    step("signing")
    signed = subprocess.run(
        ["codesign", "--force", "--deep", "--sign", "-", str(app)],
        capture_output=True,
        text=True,
    )
    if signed.returncode != 0:
        die(
            f"codesign failed: {signed.stderr.strip()}",
            "the executable is in place but macOS will refuse to launch it; "
            "`python3 build.py` rebuilds the bundle from scratch.",
        )
    ok("ad-hoc signature replaced")

    if not args.no_open:
        step("launching")
        subprocess.run(["open", str(app)], check=False)

    print(f"\ndone.\n  {app}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
