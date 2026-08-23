#!/usr/bin/env python3
"""Build Velm.app onto the Desktop, then commit and push whatever changed.

    python3 build.py                 # build, replace the Desktop app, commit + push
    python3 build.py --no-git        # build only
    python3 build.py --no-push       # build and commit, but don't push
    python3 build.py -m "message"    # your own commit message
    python3 build.py --open          # launch the app when it's built
    python3 build.py --dest /Applications

The bundling itself is NOT reimplemented here — `scripts/make-app.sh` already owns
the icon, the Info.plist, the .vellum document association and the ad-hoc codesign,
and it already deletes any existing bundle before writing the new one. This script
is the safety layer around it: the preflight checks that keep an 8GB machine from
kernel-panicking mid-build, and the git half of the job.

Order matters: the build runs FIRST and the commit only happens if it succeeded.
Pushing code that doesn't compile is worse than not pushing.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

REPO = Path(__file__).resolve().parent
MAKE_APP = REPO / "scripts" / "make-app.sh"

# A release build of this workspace peaks around 743MB resident and target/ regrows
# to ~5GB. Below this there is not enough room, and on macOS a full disk also means
# swap cannot grow — which is how this machine gets wedged rather than just slow.
MIN_FREE_GB = 6

# --wait polling. SETTLE covers the gap where cargo has exited but rustc has not yet
# appeared (or vice versa), which otherwise reads as "the coast is clear".
POLL_SECONDS = 10
SETTLE_SECONDS = 5
WAIT_TIMEOUT = 30 * 60

# The GitHub remote is public. Never commit board content or account identifiers.
# .gitignore covers the known cases, but it has no *.vellum rule and an --import run
# writes boards "beside the other boards" — so a board file landing in the repo would
# sail straight into a public push. This is the backstop.
#
# Machine-specific patterns (an export whose filename has no extension, say) belong in
# `.git/info/exclude`, which is local to your clone. Add them there, not here.
SENSITIVE = [
    re.compile(r"\.vellum$"),
    re.compile(r"^captures/"),
    re.compile(r"^Screenshots/"),
    re.compile(r"\.rtb$"),
    re.compile(r"(^|/)\.env$"),
    re.compile(r"api[-_]?key", re.I),
    # A Miro REST API token is live account access, strictly worse than board
    # content: it reads every board the account can see. The working copy lives at
    # ~/.miro_token, outside the repo on purpose, so this only catches a stray
    # copy landing inside it. Deliberately NOT a bare `token` rule — that would
    # match crates/vellum-search/src/token.rs and crates/vellum-ui/tests/tokens.rs
    # and block every push that touched them.
    re.compile(r"miro[-_]?token", re.I),
]


# ── output ────────────────────────────────────────────────────────────────────

BOLD, DIM, RED, GREEN, YELLOW, RESET = (
    ("\033[1m", "\033[2m", "\033[31m", "\033[32m", "\033[33m", "\033[0m")
    if sys.stdout.isatty()
    else ("", "", "", "", "", "")
)


def step(msg: str) -> None:
    print(f"\n{BOLD}▸ {msg}{RESET}")


def ok(msg: str) -> None:
    print(f"  {GREEN}✓{RESET} {msg}")


def warn(msg: str) -> None:
    print(f"  {YELLOW}!{RESET} {msg}")


def die(msg: str, hint: str = "") -> None:
    print(f"\n{RED}✗ {msg}{RESET}", file=sys.stderr)
    if hint:
        print(f"  {DIM}{hint}{RESET}", file=sys.stderr)
    sys.exit(1)


def git(*args: str, check: bool = True, strip: bool = True) -> str:
    """Run a git command in the repo and return its stdout.

    `strip` must be False for `status --porcelain`. Its format is `XY PATH`, and an
    unstaged modification leaves X blank — so the first line begins with a space that
    a whole-output .strip() silently eats, shifting only that line's path left by one
    character. That is not merely cosmetic: it defeated the sensitive-path guard for
    whichever file happened to sort first.
    """
    r = subprocess.run(
        ["git", *args], cwd=REPO, capture_output=True, text=True, check=False
    )
    if check and r.returncode != 0:
        die(f"git {' '.join(args)} failed", (r.stderr or r.stdout).strip())
    return r.stdout.strip() if strip else r.stdout.rstrip("\n")


# ── preflight ─────────────────────────────────────────────────────────────────


def preflight(wait: bool = False) -> None:
    step("preflight")

    if not MAKE_APP.is_file():
        die(f"{MAKE_APP} is missing", "This script wraps it; it cannot build alone.")

    missing = [t for t in ("cargo", "git", "iconutil", "codesign") if not shutil.which(t)]
    if missing:
        die(f"not on PATH: {', '.join(missing)}")

    try:
        import PIL  # noqa: F401
    except ImportError:
        die(
            "Pillow is not installed",
            "make-app.sh draws the icon with it:  python3 -m pip install Pillow",
        )

    free_gb = shutil.disk_usage(REPO).free / 1024**3
    if free_gb < MIN_FREE_GB:
        die(
            f"only {free_gb:.1f}GB free on disk, need ~{MIN_FREE_GB}GB",
            "Free space first:  rm -rf target/debug",
        )
    ok(f"{free_gb:.0f}GB free on disk")

    # CLAUDE.md is explicit: one cargo invocation at a time, across this session and
    # every agent. Both kernel panics on this machine happened during compilation.
    # With several Claude Code sessions open this is a live race, not a formality —
    # a build can start in the seconds between checking and launching, so --wait
    # re-checks in a loop and only proceeds once the toolchain has actually gone
    # quiet for a moment.
    others = concurrent_builds()
    if others and wait:
        warn(f"another build is running (pid {', '.join(others)}) — waiting for it")
        waited = 0
        while others:
            time.sleep(POLL_SECONDS)
            waited += POLL_SECONDS
            if waited % 60 == 0:
                print(f"    {DIM}still waiting… {waited // 60}m{RESET}")
            if waited > WAIT_TIMEOUT:
                die(
                    f"still busy after {WAIT_TIMEOUT // 60} minutes",
                    "Another session is building in a loop. Close it, or run without --wait.",
                )
            if not concurrent_builds():
                # Settle: a multi-step run briefly shows no process between
                # cargo handing off to rustc, which would look like "finished".
                time.sleep(SETTLE_SECONDS)
                others = concurrent_builds()
        ok(f"toolchain free after {waited}s")
    elif others:
        die(
            f"another cargo/rustc build is already running ({len(others)} process(es))",
            "This machine has kernel-panicked twice compiling. Re-run with --wait to "
            f"queue behind it, or:  kill {' '.join(others[:4])}",
        )
    else:
        ok("no other cargo build running")


def concurrent_builds() -> list[str]:
    """PIDs of cargo/rustc processes that aren't this script or its children."""
    r = subprocess.run(
        ["ps", "-Ao", "pid=,command="], capture_output=True, text=True, check=False
    )
    mine = {os.getpid(), os.getppid()}
    found = []
    for line in r.stdout.splitlines():
        pid, _, cmd = line.strip().partition(" ")
        if not pid.isdigit() or int(pid) in mine:
            continue
        # Match the real toolchain binaries, not a grep or an editor that happens
        # to have the word "cargo" in an open filename.
        if re.search(r"(^|/)(cargo|rustc)\s", cmd) or re.search(r"/bin/(cargo|rustc)$", cmd):
            found.append(pid)
    return found


# ── build ─────────────────────────────────────────────────────────────────────


def build(dest: Path) -> Path:
    app = dest / "Velm.app"

    step(f"building Velm.app → {dest}")
    if app.exists():
        # make-app.sh does its own `rm -rf` first, so this is only reporting. Doing
        # the delete here too would leave a window where a failed build has removed
        # the working app the user already had.
        size = subprocess.run(
            ["du", "-sh", str(app)], capture_output=True, text=True, check=False
        ).stdout.split("\t")[0]
        # "will replace", not "replacing": make-app.sh builds first and only deletes
        # the old bundle once the compile has succeeded, so a failed build leaves the
        # app the user already had untouched.
        warn(f"will replace the existing bundle ({size.strip()}) once the build succeeds")
    print(f"{DIM}  a cold release build takes ~2 minutes; output follows{RESET}\n")

    r = subprocess.run(["bash", str(MAKE_APP), str(dest)], cwd=REPO, check=False)
    if r.returncode != 0:
        die("the build failed — nothing was committed", "Scroll up for the compiler error.")

    if not (app / "Contents" / "MacOS" / "Velm").is_file():
        die(f"make-app.sh reported success but {app} has no executable")

    # The agent shims, beside the main executable — where `vellum_agent::transport::Shim`
    # looks for them. This is the icon lesson applied to a second thing macOS reads and we
    # do not: an .app whose shims are missing launches perfectly and every agent on every
    # board silently loses messaging, notes, spawn and options, because the app is written
    # to degrade rather than fail. A degradation nothing checks is a feature that quietly
    # stops shipping.
    #
    # `os.access(X_OK)` rather than `is_file()`: the runtime requires the execute bit, so
    # that is the question worth asking.

    # An .app is a directory that macOS reads three things out of, and only one of
    # them is the executable. A bundle with a blank icon shipped once because this
    # check stopped at the line above: the icon was built and the plist naming it was
    # invalid XML, so Finder ignored the plist entirely. Check what the OS reads.
    icon = app / "Contents" / "Resources" / "Velm.icns"
    if not icon.is_file():
        die(f"make-app.sh reported success but {app} has no icon at {icon.name}")
    plist = app / "Contents" / "Info.plist"
    lint = subprocess.run(
        ["plutil", "-lint", str(plist)], capture_output=True, text=True, check=False
    )
    if lint.returncode != 0:
        die(
            "the bundle's Info.plist is not valid — macOS would ignore it and the app "
            "would have no icon",
            lint.stdout.strip() or lint.stderr.strip(),
        )

    ok(f"built {app}")
    return app


# ── git ───────────────────────────────────────────────────────────────────────


def sensitive(paths: list[str]) -> list[str]:
    return [p for p in paths if any(rx.search(p) for rx in SENSITIVE)]


def commit_and_push(message: str | None, do_push: bool) -> None:
    step("git")

    branch = git("rev-parse", "--abbrev-ref", "HEAD")
    if branch == "HEAD":
        die("detached HEAD — checkout a branch before committing")

    # --porcelain lists staged, unstaged and untracked-but-not-ignored in one go.
    changes = [
        ln for ln in git("status", "--porcelain", strip=False).splitlines() if ln.strip()
    ]
    if not changes:
        ok("working tree clean — nothing to commit")
        if do_push and ahead_count(branch):
            push(branch)
        return

    paths = [ln[3:].split(" -> ")[-1].strip().strip('"') for ln in changes]
    print(f"  {len(paths)} change(s) on {BOLD}{branch}{RESET}:")
    for p in paths[:12]:
        print(f"    {DIM}{p}{RESET}")
    if len(paths) > 12:
        print(f"    {DIM}… and {len(paths) - 12} more{RESET}")

    if bad := sensitive(paths):
        die(
            "refusing to commit — these look like board content or secrets:\n    "
            + "\n    ".join(bad),
            "The GitHub remote is PUBLIC. Add them to .gitignore, or commit by hand "
            "if you're certain.",
        )

    git("add", "-A")
    # Nothing to do if everything that changed was already ignored.
    if not git("diff", "--cached", "--name-only"):
        ok("only ignored files changed — nothing to commit")
        return

    msg = message or default_message()
    subprocess.run(["git", "commit", "-m", msg], cwd=REPO, check=True)
    ok(f"committed: {msg.splitlines()[0]}")

    if do_push:
        push(branch)
    else:
        warn("--no-push: the commit is local only")


def default_message() -> str:
    """A summary of what actually changed, not just a timestamp."""
    files = git("diff", "--cached", "--name-only").splitlines()
    stat = git("diff", "--cached", "--shortstat")

    # Name the crate(s) touched — more useful in a log than "update files".
    crates = sorted({p.split("/")[1] for p in files if p.startswith("crates/") and "/" in p[7:]})
    if len(crates) == 1:
        subject = f"Update {crates[0]}"
    elif 1 < len(crates) <= 3:
        subject = f"Update {', '.join(crates)}"
    elif crates:
        subject = f"Update {len(crates)} crates"
    elif len(files) == 1:
        subject = f"Update {files[0]}"
    else:
        subject = f"Update {len(files)} files"

    body = f"Built from build.py at {datetime.now():%Y-%m-%d %H:%M}."
    if stat:
        body += f"\n{stat.strip()}"
    return f"{subject}\n\n{body}"


def ahead_count(branch: str) -> int:
    """Commits on the local branch that the remote doesn't have."""
    upstream = git("rev-parse", "--abbrev-ref", f"{branch}@{{upstream}}", check=False)
    if not upstream:
        return 0
    out = git("rev-list", "--count", f"{upstream}..{branch}", check=False)
    return int(out) if out.isdigit() else 0


def push(branch: str) -> None:
    if not git("remote", check=False):
        warn("no git remote configured — skipping push")
        return

    has_upstream = bool(
        git("rev-parse", "--abbrev-ref", f"{branch}@{{upstream}}", check=False)
    )
    args = ["push"] if has_upstream else ["push", "--set-upstream", "origin", branch]
    if not has_upstream:
        warn(f"{branch} has no upstream — setting origin/{branch}")

    r = subprocess.run(["git", *args], cwd=REPO, check=False)
    if r.returncode != 0:
        die(
            "push failed — the commit is safe locally",
            "Usually auth or a diverged remote. Try:  git pull --rebase && git push",
        )
    ok(f"pushed {branch} → origin")


# ── main ──────────────────────────────────────────────────────────────────────


def main() -> None:
    p = argparse.ArgumentParser(
        description="Build Velm.app onto the Desktop, then commit and push.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="For a shipping build use the fat-LTO profile by hand, and run it with\n"
        "nothing else going:  cargo build --profile dist -p vellum-app",
    )
    p.add_argument("--dest", type=Path, default=Path.home() / "Desktop",
                   help="where Velm.app goes (default: ~/Desktop)")
    p.add_argument("-m", "--message", help="commit message (default: generated)")
    p.add_argument("--no-git", action="store_true", help="build only, don't touch git")
    p.add_argument("--no-push", action="store_true", help="commit but don't push")
    p.add_argument("--no-build", action="store_true", help="git only, don't build")
    p.add_argument("--open", action="store_true", help="launch the app when built")
    p.add_argument("--wait", action="store_true",
                   help="queue behind another running cargo build instead of failing")
    args = p.parse_args()

    if not (REPO / "Cargo.toml").is_file():
        die(f"{REPO} is not the Velm repo root")

    app = None
    if not args.no_build:
        preflight(wait=args.wait)
        args.dest.mkdir(parents=True, exist_ok=True)
        app = build(args.dest)

    if not args.no_git:
        commit_and_push(args.message, do_push=not args.no_push)

    if app and args.open:
        step("launching")
        subprocess.run(["open", str(app)], check=False)

    print(f"\n{GREEN}{BOLD}done.{RESET}")
    if app:
        print(f"  {app}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        die("interrupted")
