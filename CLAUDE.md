# Aqua: rules for an AI coding agent

Hard rules only. What each part is, and how it works, is in `documentation/` (start at `documentation/README.md`). How to run, build and debug is in the skills in `.claude/skills/` (`aqua-run`, `aqua-build`, `aqua-mcp-debug`). Start a task with `/pickup` and end a session with `/handoff` (`.claude/commands/`): other agents work in this tree too. Where a skill, a document and the code disagree, the code wins.

## What is true
- What works today is `documentation/CAPABILITIES.md` (generated from `documentation/capabilities.toml`; a test checks it against the code). `ARCHITECTURE.md` and `API_INTERCONNECT_AND_MODELS.md` are design targets, not the shipped state. The newest audit is in `documentation/competitive/refresh/`; the other audit files are dated snapshots.
- Say how you know a thing: measured, read in the code, or only reported by someone.
- Speed and accuracy come first for Aqua. Saving CPU, GPU or RAM is not a goal. A claim about speed carries a before and after measurement; after any change to the model's window, layers or model, run `python scripts/eval/speed_gate.py` with Aqua closed.
- A bug fix gets a regression test. A new route, tool or setting updates its documentation in the same change.
- A check that fails is reported, not moved. Never change a threshold, a budget, a baseline file or a test fixture to make a check pass (`scripts/eval/baselines/`, the limits in `scripts/eval/speed_gate.py`, `core/tests/fixtures/`, the evidence in `documentation/capabilities.toml`): stop, report the numbers, and let the user decide. Say in your handoff which of those files you touched.

## Never touch
- **The running core.** The user's Aqua listens on `127.0.0.1:8767` and runs from `target\release`. Do not start, stop, rebuild over or test against it. For live checks start a second core with `AQUA_API_PORT=8790` and its own data folder (`scripts/bench/audit6/`, `scripts/eval/`).
- **Secrets.** The remote URL of this clone carries a login. Never print `git remote -v` or `.git/config` unmasked, the files `data/.aqua_api_token` and `data/.aqua_ui_token`, or any credential. `scripts/install_hooks.ps1` turns on the secret scan before each commit; `aqua-cli doctor` reports a login in the remote. A secret that was pushed is leaked: revoke it, do not just delete it.
- **What you did not create.** Do not delete dead code, files or folders you did not make (including the build folders of other sessions); ask. Never `rm -rf` or recursive-delete a folder that may hold junctions (the second-core sandbox links to the real model files): remove the junction first with `cmd /c rmdir <link>`.

## Other sessions work in this tree
- Several agents edit this repository at once. A change you did not make belongs to someone else: leave it, touch only your own files, and tell the user if it breaks the build.
- Stage by explicit path, never with `git add -A` or `git add .`. Never stage `data/`, `logs/`, `vendor/`, `scripts/bench/bakeoff_*.log`, or an untracked folder you did not create (for example `founder/`).
- Commit and push only when asked. No amend, force push, `reset --hard`, `clean` or `restore` unless asked. A commit subject says in a plain sentence what changed, and starts with the card id when there is one (`R6-20, third step (R3-07): ...`).
- Ask before downloading large files or installing software. Use the hash-checked download helper (`scripts/_verified_download.ps1`) and say what was fetched.

## One door
- Every tool call goes through `ToolHost::call` (`adapters/tool_host`): it validates, decides, asks, masks secrets and writes the audit log. A new tool goes through it; do not call a provider directly.
- Approvals can only be given from the app, never by the API token or by voice. The API listens on `127.0.0.1` only and needs a token.

## This machine (Windows 11; PowerShell and Git Bash)
- The disk is small and test executables are huge (the `aqua-core` lib test alone is about 1.2 GB). Avoid `cargo test --workspace`: test one crate at a time, clear `target/debug/deps/*.exe` and `*.pdb` when space is short, or point `CARGO_TARGET_DIR` at a roomier drive. `powershell -File scripts/ci.ps1` does all of this and is the gate (`-Steps tests,docs-check` picks parts; `-DryRun` shows the plan).
- Front end, in `apps/aqua-desktop`: `npm test` and `npm run build` (the UI is served on `127.0.0.1:1420`).
- In Git Bash a `cd` in one command moves the tool's working directory: start each command with `cd /e/Personal_AI/Aqua && ...`.
- Files keep their own line ending: most are LF, some are CRLF (for example `documentation/TESTING.md`). Edit with a tool that preserves it. A script that rewrites files must read and write bytes, and check the line ending with Python (`b"\r\n" in data`) rather than `grep`.

## Documentation
- Every page in `documentation/` starts with `summary` and `read_when` front matter (the format is in `documentation/CONVENTIONS.md`). `TOOLS_REFERENCE.md` and `CAPABILITIES.md`, and parts of `CONFIGURATION.md` and `TOOLS_CONFIG.md`, are generated: change the code or `capabilities.toml`, then `AQUA_UPDATE_DOCS=1 cargo test -p aqua-core --lib docs_gen`.
- `python scripts/check_docs.py` must pass: front matter, links, the paths a page names, the index, scripts named, skills, the hard rules of this file and of the agent commands and skills (`REQUIRED_RULES` in the script: a rule changed on purpose is changed there in the same commit), and no command that is always wrong in a code block (`git add -A`, `git remote -v`, `cargo test --workspace`...; a block that shows one as a warning ends the line with `# check_docs: ok`). A new page needs a row in `documentation/README.md`; a new script needs a line in `documentation/SOURCE_INVENTORY.md`. A page that names something not built yet needs a reasoned line in `scripts/check_docs.allow`.
- To choose what to read before a task: `python scripts/check_docs.py --list <word>` prints the pages with their `summary` and `read_when`.
- `powershell -File scripts/check_scripts.ps1` (the `scripts` step of `scripts/ci.ps1`) checks that every `.ps1`, `.py` and `.sh` still parses. Keep script files ASCII where you can: Windows PowerShell 5.1 reads a UTF-8 file without a byte order mark as ANSI, and a long dash in a string breaks the script.
