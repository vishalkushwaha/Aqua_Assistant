# Core Storage (`aqua_core.db`)

Relational settings and task history via embedded SQLite (WAL mode).

---

## Type

**`DatabaseManager`** — `storage/db.rs`

---

## Initialization

```rust
let db = DatabaseManager::new("data")?;
db.set_setting("system_status", "ACTIVE")?;
```

Creates `data/aqua_core.db` if missing.

---

## Schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

```

(An older version also created a `task_history` table that nothing ever wrote to. It is gone: agent runs are recorded step by step in
`data/agent_runs.db`, see `core/src/agent_runs`. A database that already has the empty table keeps it.)

---

## API

| Method | Description |
| :--- | :--- |
| `set_setting(key, value)` | Upsert setting with Unix timestamp |
| `get_setting(key)` | Optional string value |
| `solutions_db_path()` | Path to sibling `solutions.db` (managed by `memory/`) |

---

## The shared SQLite helper (`sqlite_store.rs`, card OA3-02)

Every Aqua database is opened and upgraded the same way (`open_migrated`, or `connect_checked` for a store that opens a connection for every call):

| What | How |
| :--- | :--- |
| Options | `Opts::standard()` (write-ahead log, 5 s wait for a lock, check on) or `Opts::private()` (no log, overwritten deletions, no copy before an upgrade: for what the person asks to forget); builders for the busy wait, `secure_delete`, `synchronous`, foreign keys, the check, a refusal to recover |
| Damage | `PRAGMA quick_check(1)` for a file up to 32 MB (`Opts::quick_check_up_to`), then the file is moved aside as `<name>.corrupt-<unix>` with its `-wal` and `-shm`, and `Opened::recovered_from` says where; `recover(false)` makes damage an error instead (`aqua_core.db`). A locked file is an error, never a quarantine |
| Steps | `Migration { id, sql }`, counting up from 1; each runs in `BEGIN IMMEDIATE` with `PRAGMA user_version = id`; never edit a shipped step, add one |
| A newer file | `StoreError::TooNew { found, known }`, before the journal mode or anything is written |
| A copy | `VACUUM INTO <name>.bak-v<old>` before the first step that upgrades a file that holds a version; the two newest are kept |
| Old files | A file made before versions existed holds the first step's tables already (`CREATE TABLE IF NOT EXISTS`), so the version is only stamped; `open_migrated_adopting` takes a function that looks at the columns for the store whose old builds also altered tables |

Where each file stands:

| File | Store | On the helper |
| :--- | :--- | :--- |
| `agent_runs.db` | `agent_runs/store.rs` | yes (five steps, one connection kept) |
| `self_repair.db` | `self_repair/store.rs` | yes (`MIGRATIONS`) |
| `experience.db`, `solutions.db` | `memory/experience/store.rs`, `memory/solutions_db.rs` | yes (one step; a connection per call) |
| `user_memory.db` | `memory/user_facts/store.rs` | yes, private |
| `vault/vault.db` | `vault/mod.rs` | yes, private |
| `aqua_core.db` | `storage/db.rs` | yes, damage stays an error (voice lock) |
| `flows.db` | the five flow stores | yes: the version, the check and the guard; the tables are still made by the stores' own start-up (see `flows/store.rs::DB_STEPS`) |
| `workflows.db` | `workflows/store.rs` | yes |
| `files.db` | `adapters/mcp_adapters/src/file_index/store.rs` | **no**: its own crate (adapters do not depend on core), and it has its own check and `.bad` move |
| `aqua_core.db` (`scheduled_tasks`) | `modules/scheduler/scheduler_engine.rs` | **no**: shares the file with `db.rs`, and nothing calls it |
| user databases | `adapters/native_tools/src/db` | not Aqua's own |

A connection kept open for the stores that open one per call (the 10 ms a write costs on a short connection, measured once in AUDIT_5) was not done: it needs a before and after on this machine.

## Related

- Solution memory: [`../memory/README.md`](../memory/README.md)
- Data directory: [`../../../data/README.md`](../../../data/README.md)
