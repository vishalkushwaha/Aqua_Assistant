---
description: Package this session's state for the next agent (scope, working tree, what is yours and what is not, running things, checks, next steps) and save it under .aqua/handoff/
argument-hint: "[short topic]"
---

# /handoff

Write the note the next session can start from without asking anyone. Several agents edit this one working tree, so be exact about which part is yours. Run the commands below instead of answering from memory. Do not change code for this command. Never put a secret, a token or the remote URL in the note.

## Collect

1. **Scope and status.** What the user asked for, what is done and verified, what is pending, what blocks it. Name the cards (`R6-xx`) and the files.
2. **The working tree.** Run `git status -sb`, `git log --oneline origin/main..HEAD` and `git diff --cached --name-only`.
   - **Yours:** every file you changed or created, and for each whether it is committed, staged or loose, and whether it is pushed.
   - **Not yours:** files that changed and that you did not touch. Say whose they are if you know (another session), and that you left them alone.
   - Anything staged that is not yours is a problem: say so first.
3. **Running things.** Which of these are up, with `Get-NetTCPConnection -State Listen -LocalPort 8767,8790,1420 -ErrorAction SilentlyContinue` (PowerShell): the user's Aqua core on `8767` (you never start, stop or call it), a second core on `8790`, the UI on `1420`. Also the background jobs you started, the `CARGO_TARGET_DIR` you used, and the free space (`df -h /e /c`).
4. **Checks.** Every command you ran and whether it passed, failed or was not run. What still has to run (for example `python scripts/check_docs.py`, `cargo test -p <crate> --lib <filter>`, `powershell -File scripts/ci.ps1 -Steps <steps>`). For each number say whether it was measured, read in the code, or only reported. Name every gate or baseline file you touched (`scripts/eval/baselines/`, a limit in `scripts/eval/speed_gate.py`, `core/tests/fixtures/`, evidence in `documentation/capabilities.toml`) and why, or write "none": a check that fails is reported, not moved.
5. **Waiting for the user.** The decisions only they can make, and what each one changes.
   - Write each open decision (D-n) as a decision brief, in the form of the `aqua-review` skill: the behaviour and who is affected, why now, the proof, the trade-offs, a recommendation, and the exact choices with what each one does.
6. **Next steps.** An ordered list for the next session, then the risks: half-finished files, flaky tests, a full disk, anything that must not be touched.

## Save and show

Write it to `.aqua/handoff/<YYYY-MM-DD_HHMM>_<topic>.md`, where the topic is `$ARGUMENTS` or a short slug of the task. Create the folder if needed. `.aqua/` is ignored by git, so the note stays on this computer. Never overwrite another note: the name carries the time. Then print the note, one screen at most, and give the path.
