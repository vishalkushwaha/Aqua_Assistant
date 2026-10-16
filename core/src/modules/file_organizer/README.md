# File Organizer & Smart Search

> **Code status (2026-10-08): written, not wired.** `FileOrganizerEngine` has tests, and nothing outside them calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. It moves files with `fs::rename`, outside the tool host. Finding and opening files by name is `adapters/mcp_adapters/src/file_index/`. There is no register entry.

Natural-language file discovery, duplicate awareness, and download-folder sorting hooks.

---

## Implementation

- **Module:** `organizer_engine.rs`
- **Entry:** re-exported from `core/src/modules/file_organizer/mod.rs`

Integrates with architecture targets for `Laya_Rust` file-type identification and MCP path canonicalization (see [`documentation/ARCHITECTURE.md`](../../../../documentation/ARCHITECTURE.md)).

---

## Tests

`core/tests/file_organizer_test.rs`

---

## Related

- Document reader (structured files): [`../document_reader/README.md`](../document_reader/README.md)
- MCP Windows automation (sticky notes, shell): [`adapters/mcp_adapters/README.md`](../../../../adapters/mcp_adapters/README.md)
