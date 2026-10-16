# Solution Memory (`solutions.db`)

Long-term **solution recipes** and fast-path reuse for repeated user intents.

---

## Implementation

- **Type:** `SolutionsDb` in `solutions_db.rs`
- **Database path:** `{workspace}/data/solutions.db` (created on init)
- **Records:** `SolutionRecord` — intent pattern, JSON recipe payload, execution stats
- **Search:** `SearchResult` with similarity score and fast-path eligibility

Complements the broader learning design in [`learning/README.md`](../../../learning/README.md) (HNSW / vector extensions are architecture targets).

---

## Tests

`core/tests/solutions_db_test.rs`

---

## Related

- Core storage (settings/history): `core/src/storage/db.rs` → `aqua_core.db`
- API & storage overview: [`documentation/API_INTERCONNECT_AND_MODELS.md`](../../../documentation/API_INTERCONNECT_AND_MODELS.md) §7

---

## Agent experience memory

Beside the recipe store, `experience/` holds verified, reusable problem-solving experience with its own SQLite store (`data/experience.db`), retrieval, learning worker and management API. See [`documentation/AGENT_LEARNING.md`](../../../documentation/AGENT_LEARNING.md).
