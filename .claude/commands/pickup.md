---
description: Get your bearings at the start of a task (latest handoff notes, the shared working tree, what is running, which docs to read) and plan the first steps
argument-hint: "[task or card, optional]"
---

# /pickup

Rehydrate the context before touching anything. Everything you read is a past observation: check it against the tree before you rely on it. Do not change code for this command, and do not start, stop or call the user's Aqua core on `127.0.0.1:8767`.

## Steps

1. **Rules and memory.** `CLAUDE.md` is already loaded: obey it. Open the memory entries whose title fits the task (`$ARGUMENTS` if given).
2. **Handoff notes.** `ls -t .aqua/handoff/*.md 2>/dev/null | head -3`, then read the newest one that fits the task. A note says what its author believed at the time. Re-check its claims in step 3.
3. **The working tree.** Run `git status -sb`, `git log --oneline -10`, `git log --oneline origin/main..HEAD` and `git diff --cached --name-only`.
   - First run `powershell -NoProfile -File scripts/tree_status.ps1`. It is read-only and prints, in one go: git lock files (a stale one blocks every write), ahead and behind against `origin/main` (as of the last fetch), the dirty paths in groups, the live `cargo`, `rustc`, `llama-server` and `aqua-core` processes, and which of the ports 8767, 8790 and 1420 are listening.
   - Other agents work in this tree. A changed file you did not touch is theirs: leave it, and plan to commit only your own files by explicit path.
   - Anything already staged was staged by someone else: do not commit over it. Say so to the user.
   - Note unpushed commits, and whether the tree compiles where you will work (the rustc diagnostics in the editor are a hint, a build per crate is the proof).
4. **What is running.** `Get-NetTCPConnection -State Listen -LocalPort 8767,8790,1420 -ErrorAction SilentlyContinue` (PowerShell), and `df -h /e /c`. If `E:` has less than about 6 GB free, say so before any build: test one crate at a time, and read `scripts/ci.ps1`'s disk rules.
5. **The right docs.** Every page in `documentation/` has a `summary` and a `read_when`. List them with `python scripts/check_docs.py --list` (every page with its summary and the situations in `read_when`), narrow the list with words from the task (`python scripts/check_docs.py --list voice flow`), and read the pages whose `read_when` fits before the code. `documentation/CAPABILITIES.md` is the truth about what works; `ARCHITECTURE.md` is a design target. The newest audit is under `documentation/competitive/refresh/`.
6. **The checks you will run first.** `python scripts/check_docs.py` takes a second. For code, name the crate and the filter (`cargo test -p <crate> --lib <filter>`) before you start, not after.
7. **Plan.** Write the next two or three actions as a short list, say what you will not touch, and start the first one. If something above is wrong or unclear (a note that contradicts the tree, a staged file that is not yours), stop and ask the user before acting.

## Report

A short summary of the state (branch, unpushed commits, whose changes are in the tree, what is running, free disk), the docs you will rely on, and the plan. No long recap.
