#!/usr/bin/env python3
"""Bisects a crash of Apple's shader compiler down to the instructions behind it.

    tools/macos/reduce-crash.py <dir with one dumped container> <state file>

Needs a built victory.exe (run tools/macos/victory.sh once). Each step zeroes
some of the emitted instructions (VICTORY_REDUCE, see `Module::reduce`) and
keeps the change when the pipeline still fails with an interrupted compiler
connection. The state file lists the instructions zeroed so far, so the run can
be stopped and resumed; to read the result, emit with VICTORY_REDUCE=<state
file> and VICTORY_EMIT_DUMP=<dir>. The copy of victory.exe it takes at the
start keeps later builds out of the run.
"""
import os, shutil, subprocess, sys

root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
target, state = sys.argv[1], os.path.abspath(sys.argv[2])
wine = os.environ.get("WINE", os.path.expanduser("~/Library/Caches/victory-macos/target/wine-cx/bin/wine"))
exe = state + ".exe"
shutil.copy(os.path.join(root, "target/x86_64-pc-windows-gnullvm/release/victory.exe"), exe)
env = dict(
    os.environ,
    WINEPREFIX=os.environ.get("WINEPREFIX", os.path.join(root, "target/wineprefix-play")),
    WINEDEBUG="-all",
    WINEDLLOVERRIDES="mscoree,mshtml=;winedbg.exe=d;d3d12,d3d12core,dxgi=b",
    VICTORY_READING_PS="1",
    VICTORY_WAVE32_HOST="1",
    VICTORY_REDUCE=state,
)
tests = 0


def crashes(killed):
    global tests
    tests += 1
    with open(state, "w") as f:
        f.write(" ".join(map(str, sorted(killed))))
    out = subprocess.run(
        [wine, exe, "translate", "--pipelines", "--list", "--jobs", "1", target],
        env=env, cwd=root, capture_output=True, text=True, errors="replace",
    )
    return "XPC_ERROR_CONNECTION_INTERRUPTED" in out.stdout + out.stderr


killed = set(map(int, open(state).read().split())) if os.path.exists(state) else set()
if not crashes(killed):
    sys.exit("no crash to start from")
n = int(open(state + ".n").read())
best = set(killed)
chunk = max(1, (n - len(killed)) // 2)
while True:
    alive = [i for i in range(n) if i not in killed]
    progress = False
    for at in range(0, len(alive), chunk):
        trial = killed | set(alive[at : at + chunk])
        if crashes(trial):
            killed, progress = trial, True
            best = set(killed)
            print(f"{tests} tests: {n - len(killed)} of {n} left (chunk {chunk})", flush=True)
    if chunk == 1 and not progress:
        break
    chunk = max(1, chunk // 2)
with open(state, "w") as f:
    f.write(" ".join(map(str, sorted(best))))
print(f"done after {tests} tests: {n - len(best)} of {n} instructions left")
