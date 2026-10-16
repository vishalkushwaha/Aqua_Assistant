# Aqua CLI

The command-line tool for the things that are better done from a terminal than from the app: checking an install, keeping secrets, reading the signed record of a run, signing a promoted configuration, filling the offline vault, and cleaning stored recipes. It is also a **client of the running core's HTTP API**: it can start a run, follow it, end it and say something to it, and get an exit code (`run`, `runs`, `stop`, `steer`, `api`, see below). It does not run the assistant itself. The assistant is the `aqua-core` program (or the desktop app).

The code is `apps/aqua-cli/src/main.rs` (the dispatch). What the newer commands do is in `core/src/headless/` (the client) and `core/src/cli_tools/` (`config`, `env`, `stats`, `support-bundle`, `door-report`, `slots`), because the binary has no JSON crate and adding one would change `Cargo.lock`. Where this page and the code disagree, the code is right.

---

## Run

From the repository root:

```bash
cargo run -p aqua-cli -- <command> [options]
```

An installed copy has `aqua-cli.exe` beside the app. Without a command it prints the version, the list of commands below, and the report of `doctor` for the data folder of this install.

Every command that reads Aqua's data folder takes `--data DIR`. The default is `data/` in the folder that holds this checkout, or `%LOCALAPPDATA%\Aqua\data` for an installed copy; `AQUA_WORKSPACE_ROOT` overrides the folder (the rule is `aqua_workspace_root` in `adapters/mcp_adapters/src/browser_mcp/health.rs`).

---

## Commands

| Command | What it does |
| :--- | :--- |
| `doctor [--fix] [--live] [--data DIR]` | Checks the tool side of an install and, on a first run, writes the default configuration files that are missing (with nothing dangerous switched on). See below. |
| `secret-set NAME` | Reads the value from standard input (so it is not in a shell history or a process list) and stores it in Windows Credential Manager. |
| `secret-check NAME` | Says whether a secret is stored, and how many characters it has. It never prints it. |
| `secret-delete NAME` | Removes the secret. |
| `audit-key [--data DIR]` | Prints the key id and the public key that sign the tool host's audit ledger, and how many keys have signed. Write them down somewhere Aqua cannot reach. |
| `verify-run <run id> [--data DIR]` | Proves the ledger lines of one run in this install's own ledger: the hash chain and the signed tree heads. Exit code 0 when everything checks out, 1 when not. |
| `verify-bundle <file> --key <hex>` | Checks a run's proof bundle (from the proof route, or the proof itself) against a public key in hex. Needs no running Aqua and no data folder. Exit code 0 or 1. |
| `sign-promoted <pending file> [--data DIR]` | Signs the promotion that `scripts/eval/quality.py promote` wrote, with the audit key, so that it becomes `data/promoted_config.json`. It shows what the file holds and waits for `yes` typed at a terminal; a script or an agent that pipes an answer in is refused. The core uses a promoted configuration at start only when it is signed. |
| `vault add <folder>`, `vault index`, `vault status`, `vault forget <folder>` or `vault forget --all` | Chooses, reads and empties the folders the offline vault searches (`data/vault/vault.db`). Nothing is read until `vault index` is run. |
| `cmd list`, `cmd show <name>`, `cmd run <name> [words] [--folder DIR] [--dry-run] [--background] [--core URL]` | The commands you wrote in `data/commands` (card OA11-02; see [`TOOLS_CONFIG.md`](../../documentation/TOOLS_CONFIG.md#commands-you-write-datacommands)). `list` and `show` read the files. `run --dry-run` puts the words in and prints the goal, the steps and the tools allowed, and runs nothing. `run` asks the running core on this computer (default `http://127.0.0.1:8767`; the API token is read from the data folder and sent to `127.0.0.1` only) to start it: the core runs the steps through its tool host, where each asks in the app, and then the agent; it needs `AQUA_COMMANDS=on`. Words that look like an option go after `--`. |
| `purge-recipes` | Drops stored recipes that use retired tools and redacts the rest (`SolutionsDb::purge_unsafe_recipes`). |
| `logs [--run ID] [--level L] [--tail N] [--json] [--file PATH]` | Reads the core's log back: the lines of one run (`--run`, by its id), the lines at a level and above (`--level error\|warn\|info\|debug\|trace`), the last N lines (`--tail`, 200 by default), each as a JSON object with `--json`. It reads `AQUA_LOG_FILE` (the file a core writes when it is told to), else `logs/core.log` of the workspace (what the desktop app keeps of the core's output), and the older files beside it (`.1` to `.4`). It does not talk to the running core. |
| `run "<goal>" [--dir PATH] [--format text\|json] [--detach] [--wait-approval SECS] [--max-steps N] [--profile P] [--preapproval ID] [--session NAME] [--core URL] [--data DIR] [--token-file F]` | Starts a run on the running core (`POST /api/v1/runs`) and follows it to its end. See "Running a run from a terminal". |
| `runs list`, `runs show ID`, `runs events ID [--after N] [--follow]`, `runs export ID` | The runs of this token that are not over, the state of one, its events (rendered, or as JSON lines with `--format json`; `--follow` follows it to its end with the exit code of `run`), and its event log as JSON lines (the form the API token may read: what a call was given and what it returned are the app's). Finished runs of the person are in the app. |
| `stop [ID]` | Ends a run (the only one of this token that is going when no id is given). |
| `steer ID "<text>" [--queue]` | Says something to a run that is going: added to its task at its next step, or (`--queue`) kept for after it. |
| `api <operationId> [--param k=v]... [--data JSON]`, `api --list` | Calls any operation of the API that the API token may call, by the name `documentation/openapi.json` gives it (`--data-dir` names the data folder for this command, since `--data` is the body). An operation that needs the UI token or is a stream is refused with a pointer. |
| `preapprove <folder> [--kinds tests,build,lint,typecheck] [--budget N] [--minutes N]`, `preapprovals [list]`, `preapprovals revoke ID` | **For a person at a terminal**, with the app's own token (read from the data folder, for this request only): allows ONE run to run the project's checks in ONE folder without a question each time (needs `AQUA_PREAPPROVALS=on` on the core). It shows what it will do and waits for `yes` typed; a script that pipes an answer in is refused. It cannot approve anything that is asked. |
| `tui ["<goal>"] [--dir PATH] [--session NAME] [--profile P] [--max-steps N] [--wait-approval SECS] [--preapproval ID] [--attach ID] [--ui] [--width N] [--no-color]` | The terminal view of a run (card OA10-07): starts a run like `run` (profile `coding`, a question kept open 120 seconds) and shows it as lines, or with no goal asks for goals one after another in one session, or with `--attach ID` follows a run that exists. See "The terminal view". |
| `acp [--core URL] [--data-dir DIR] [--token-file F] [--wait-approval SECS] [--profile P]` | The Agent Client Protocol on standard input and output, for an editor that starts it (Zed, JetBrains, Neovim); approvals stay in the Aqua app. Started by the editor, not by hand. See [`ACP.md`](../../documentation/ACP.md). |
| `config list`, `config validate`, `config get <file> [key]`, `config set <file> <key> <value>` | The configuration files of `data/tools`: where they are and whether they are valid (the same report as `doctor`; `validate` exits 1 for a broken one), one file or key (secrets hidden), and `set`, which changes only `live_output` and `auto_background` of `term.json` (at a terminal, with `yes`; comments and line endings are kept). |
| `env [--set]` | The `AQUA_*` variables the source mentions and which are set here (the value of one whose name says it is a secret is never shown). |
| `stats [--days N] [--json]` | Runs of the last days by how they ended, the model's time to the first token and speed, the tool error rate and the questions asked of the person by day, from the run database (opened for reading only). |
| `support-bundle [--out DIR]` | A folder of six files for a bug report (versions, the doctor, which config files are valid, the redacted tail of the log, the last 20 runs without content). Never a token, a database or a word you said; printed, never uploaded. |
| `door-report [--file PATH] [--min-runs N] [--max-looser N] [--data DIR]` | What the one-door comparison has found (card OA8-05): reads `data/door_shadow.json` (or the file named) and prints how many calls and runs were compared, per tool and per caller how often the door would decide the same as the old route, stricter or looser, every looser decision, every kind of stricter one, and a last line, `enough evidence to flip: yes` or `no`, against `--min-runs` (50 by default) and `--max-looser` (0 by default): numbers for the person to change, advice and not a gate. It only reads and prints words; a missing file is a report that says so, a file that is empty, damaged or not a comparison file is exit code 1. A file written before runs were counted (version 1) says that its runs are not recorded. |
| `slots list [--data DIR]`, `slots forget [--data DIR]` | The saved model slots (card OA9-06). With `AQUA_LLAMA_SLOT_SAVE=on` the model server's slot is saved as files `kv_<16 hex>_<16 hex>.bin` in `data/cache/kv`; each one holds a conversation's tokens and what the model made of them, so it is as private as the conversation. `list` prints how many there are, their size in all and, for each, its name, size and age (never a content, never a path); `forget` deletes every one and prints how many files and bytes it deleted. Neither needs a running core, and neither depends on the switch (you can clear the files after you turned it off). They touch only that folder, with links followed, and only plain files with exactly that name form: a folder, a link or any other file in it is left alone and counted (`left alone: N entries`), and a link at `cache` or `kv` that leaves the data folder is not followed (exit code 1). A file that cannot be deleted (in use, read-only) is named, the others are still deleted, and the exit code is 1. The folder itself stays; a missing folder is `no saved slots`, exit code 0. |

The help text that the CLI prints with no command lists `doctor`, `logs`, the three `secret-*` commands, `vault`, `pack`, `cmd`, `run`, `runs`, `stop`, `steer`, `api`, `preapprove`, `preapprovals`, `tui`, `acp`, `config`, `env`, `stats`, `support-bundle`, `door-report`, `slots` and `purge-recipes`. `audit-key`, `verify-run`, `verify-bundle` and `sign-promoted` work but are not in that list.

---

## Running a run from a terminal

```bash
aqua-cli run "add a doc comment to main.rs" --dir E:\work\project            # follow it as text
aqua-cli run "fix the failing test" --dir E:\work\project --format json | jq  # one JSON event per line, as the core sent it
git diff | aqua-cli run "review this change"                                 # piped text is added to the goal
aqua-cli run "..." --detach                                                  # start it and leave: runs events <id> --follow comes back to it
```

It talks to `http://127.0.0.1:8767` (`--core URL` or `AQUA_API_PORT` for a second core; **only** an address on this computer is accepted, so the token is never sent anywhere else) with the **API token**, read from `--token-file`, else `AQUA_API_TOKEN`, else `data/.aqua_api_token`. The token is never printed.

| Exit code | Means |
| :--- | :--- |
| 0 | the run completed |
| 1 | the run failed, or the request was wrong (a bad option, a folder Aqua may not use, a token the core refused) |
| 2 | **the run needs you**: the agent asked a question, or a question of the host (a command to run, a file to change) was not answered in time |
| 3 | the run was stopped by a limit (the round cap, or a cap of the autonomy setting) |
| 4 | the run was cancelled (by Ctrl+C here, or by you in the app) |
| 5 | the core is not running on that address (the hint is `aqua-cli doctor --live`), or it is too busy to take the run |

- **It never approves.** A question of the core is answered in the app, with the app's token. `--wait-approval SECS` keeps such a question open that long (10 seconds at least, 600 at most; 10 by default) for a person who is away and will come back; the run's `approval.closed` event says how it ended, and an unanswered question makes the exit code 2.
- **Ctrl+C**: the first press asks the core to end the run ("stopping...") and waits up to ten seconds for it; the second leaves at once.
- **A connection that breaks** is taken up again from the last event the client has (`?after=`): nothing is missed and nothing is printed twice. After eight failed tries in a row it gives up (exit 5).
- `--format json` prints exactly the `data:` of each server-sent frame, one per line, the heartbeats left out, and nothing else on the output; the client's own words are on the error stream. Colour is for a terminal only, never in JSON. Text from a run has every control character replaced before it is drawn.
- **Pre-approved checks** (`AQUA_PREAPPROVALS=on` on the core; decision D-07 of the audit): `aqua-cli preapprove E:\work\project --kinds tests,build --budget 60 --minutes 120` prints an id; `aqua-cli run "..." --dir E:\work\project --preapproval <id>` then runs that project's own checks (`cargo test`, `npm test`, ...) with no question, for that run and no other. Everything else still asks, **including every edit**: a run left alone still stops at its first edit that nobody answers. See [`SECURITY_AND_PRIVACY.md`](../../documentation/SECURITY_AND_PRIVACY.md).
- `--session NAME` puts the run in a coding session (card OA10-04: with `AQUA_CODING_SESSIONS=on` the core tells a coding run of the session what the earlier ones came to). Not built: `--continue` (it needs the list of sessions), a plan run (`agent: "plan"` is approved in the app) and a `logs --follow`. The terminal view is `tui` (below).

Names of secrets are letters, digits and `_ - / .`. What the register says about these commands (`obs.doctor`, `safety.doctor_token_files`, `obs.doctor_live_probe`, and the entries for the vault, the signed ledger and the promoted configuration) is in [`CAPABILITIES.md`](../../documentation/CAPABILITIES.md); the signed record is described in [`SECURITY_AND_PRIVACY.md`](../../documentation/SECURITY_AND_PRIVACY.md).

---

## The terminal view

```bash
aqua-cli tui "fix the failing test" --dir E:\work\project      # start a run and watch it
aqua-cli tui                                                  # at a terminal: asks for goals, one session
aqua-cli tui --attach run_9f2c                                # follow (or replay) a run that exists
```

Card OA10-07 (task UX-06 of the OpenCode audit). It is the same client as `run` (the same token, the same address rule, the same stream taken up again from the last event), shown for a person who wants to watch: `> run ...` and `> step` lines, `$ tool` rows, what a call printed or returned cut to a few lines with the rest counted, the diff of a change that waits for a yes, a question of the agent, a status line (`[running | 0:12 | ctx 60% (3.7k/6.1k) | 8.4 tok/s | ttft 1.2 s]`) after each reply of the model, the answer, `= completed (final) in 12.0 s, 1 files changed`, and one line on what the run did (calls, files, tokens, questions). The exit code is `run`'s (0 to 5).

- **It writes lines and nothing else.** No raw mode, no alternate screen, no cursor movement, no new crate (the audit asked for `ratatui`; decision D-06 keeps `Cargo.lock` as it is): the scrollback and the copy of your terminal work as always and there is nothing to restore after a panic. Colour only on a terminal and not with `NO_COLOR` (or `--no-color`); without colour the marks of a diff (`+`, `-`, `@@`) are the text, so nothing is lost, and a piped output is plain text. `--width N` (else `COLUMNS`, else 100) sets where a line is wrapped; under 60 columns there is no indent and the status line is short.
- **It never answers anything.** A question of the agent and an approval are answered in the Aqua app with the app's token. With the API token (the default) the run says only that it waits. **`--ui`** (decision D-09, recommended option b: for a person at a terminal, with a notice on the screen when it starts) reads the stream of the run with the app's own token **for reading only**: it then shows what is asked (the call, its reason, risk and rule, the diff of the change, the question and its choices) and what a call was given and returned. Its requests with that token are `GET`s of the run's stream; the run is started, ended and steered with the API token; there is no word of it that answers, approves or changes. A script (no terminal) is refused `--ui`.
- **While a run goes**, a line you type is said to it for its next step (`/steer TEXT` is the same), `/queue TEXT` keeps it for after, `/stop` ends the run, `/status` prints the status line, `/help` lists the words. The first Ctrl+C asks the core to end the run, the second leaves; at the prompt Ctrl+C leaves. At the prompt (no goal given) `/new` starts another session, `/sessions` and `/runs` list this token's, `/quit` leaves; every goal of one prompt is in one session (`session_id`).
- Everything a run says is text a model, a file or a program wrote: control characters are replaced, characters that cannot be seen or that turn the text around are shown by their number (`<U+202E>`), and code or a diff is clipped, not re-flowed.
- Not built: a full-screen layout with a sidebar and a live prompt box, `@` completion of paths, Esc-Esc to interrupt (a terminal needs raw mode for it), `/undo`, `/redo`, `/export` (they need the routes of undo and export that the API token may call), the GPU/CPU field of the status line (no event carries it). Tested against a stub of the core and through the real binary; **the 20-minute session against a second core that the card asks for was not done** (a run needs the model).

---

## `doctor`

`doctor` does not need Aqua to be running. It never prints a secret and never changes an existing configuration file. Its source is `core/src/doctor.rs`.

The first report is one line per check, `[ ok ]`, `[ info ]`, `[ WARN ]` or `[ FAIL ]`, with a `fix:` line under a problem, and a count of failures and warnings at the end. It covers:

- the data folder, and whether a remote of this clone carries a login in its URL (the name of the remote only, never the value);
- the two tokens (`.aqua_api_token`, `.aqua_ui_token`): present, and long enough (the content is never shown);
- `policy.json` (a broken file locks the whole tool host) and `AQUA_TOOL_MODE`;
- each configuration file of the tool families (`fs.json`, `term.json`, `term_rules.json`, `hooks.json`, `mcp.json`, `repos.json`, `ssh.json`, `db.json`, `http.json`, `docker.json`, `k8s.json`, `iac.json`, `cloud.json`, `forge.json`, `mon.json`) and the skills folder: valid or rejected, and what each enables;
- web search: which services are set up and whether the key of each is in the credential store;
- the folder roots of the file tools (`--fix` creates a missing one);
- the credential store, end to end, with a throw-away entry;
- the enrolled voice: whether the stored voiceprint can still be opened with the key in the credential store;
- the audit log: the number of records and whether the hash chain is intact;
- the programs the tools drive (`git`, `ssh`, `scp`, `uvx`, `docker`, `kubectl`, `terraform`, `aws`, `az`, `gcloud`, `wevtutil`): a missing one only hides its tools;
- the background jobs: a line `background jobs` when the running core's monitor found one stuck (no sign of life for five times what it promised) or ended by an error or a panic, read from `data/crash/jobs.json` (a failure while the record is under three minutes old, a note when it is older);
- whether something listens on `127.0.0.1:8767` (the one connect that is made without `--live`).

A second report follows, one line per fact in the form `key=value current` or `key=value drift`, for each of the two token files (`api_token.*` and `ui_token.*`): whether it exists, is a plain file and not a link, how many bytes it has, whether git tracks it, and who can open it (the file is never opened). With `--live` it adds one line, `core_health`, from one `GET /api/v1/health` on `127.0.0.1:8767` (over after 5 seconds at the latest). A finding is a report; nothing here changes a permission or a file.

The exit code is 1 when a check failed or any line of the second report is `drift`, and 0 otherwise.

---

## Not a demo any more

Earlier versions printed ten start-up checks of simulated modules and a loop of invented microphone and speaker-match figures. That is gone: `main.rs` no longer calls the vision engine, the dependency integrator, the social watchdog, the career engine, the TDD engine or the MCP suite, and it has no monitoring loop. Those modules are still in the tree and nothing outside their own tests calls them ([`REQUIREMENTS_TRACEABILITY.md`](../../documentation/REQUIREMENTS_TRACEABILITY.md), [`10_DEAD_CODE_CALLERS.md`](../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)).

---

## Dependencies

`aqua-core`, `aqua-native-tools` (the credential store), `aqua-mcp-adapters` (the data folder rule). `aqua-audio-adapter` and `aqua-vision-adapter` are listed in `Cargo.toml` and not used by `main.rs`.

The code behind the commands is tested where it lives (`core/src/doctor.rs`, `core/src/audit_key.rs`, `core/src/promoted.rs`, `core/src/vault/cli.rs`, `core/src/headless/tests.rs`, `core/src/cli_tools/`). The crate has three test files: `apps/aqua-cli/tests/run_cli.rs` (the real binary against a stub of the core, a thread with a `TcpListener`, for the exit codes, the JSON format, the taking up of a stream again and the refusal of an address that is not this computer), `apps/aqua-cli/tests/tui_view.rs` (the same stub, `tui` with its output piped) and `apps/aqua-cli/tests/acp_stdio.rs` (`acp` driven by JSON-RPC written to its standard input); the last two share `apps/aqua-cli/tests/common/mod.rs`. For the HTTP API use `aqua-core` (`core/src/main.rs`).
