# Aqua Company Brain implementation plan

## Objective

Make FalkorDB the required context layer for an Aqua agent that connects company people, teams, projects, tickets, decisions, documents, and chats; remembers interactions; coordinates agents; acts on graph-derived ownership; and shows the exact paths and sources behind every answer.

## Current completion

| Phase | Status | Evidence |
| --- | --- | --- |
| 1. Truthful vertical slice | Implemented; locally verified | Company Brain UI/API, graph-only answers/actions, five passing frontend tests, production build |
| 2. Aqua core integration | Implemented; `cargo check -p aqua-core --lib` passes | Eleven narrow `company.*` tools, six closed retrieval templates, protected configuration, tool-pack routing, prompt and source-verifier integration |
| 3. Ingestion and freshness | Implemented for Markdown/frontmatter and JSON exports | Tenant-scoped envelopes, deterministic hashes, upsert/deactivate plans, provenance and freshness; PDF extraction remains optional follow-up |
| 4. Retrieval quality | Implemented; module tests passed | Six intent/ranking/refusal/claim-validation tests in the isolated module harness |
| 5. Memory and coordination | Implemented; module tests passed | Session memory, append-only outcomes, shared state, handoffs, retention and authorized deletion |
| 6. Submission hardening | Repository work complete | Secure defaults, explicit legacy-endpoint opt-in, timeouts, request/body/result limits, origin checks, secret-safe config, local tests/build, and live graph acceptance pass; publishing and demo recording remain owner actions |

All six implementation phases are complete. Final submission still requires the project owner to publish the repository, provision or confirm a TLS-enabled deployment credential, and record the demo.

## Verification status (measured)

Run in this working tree:

| Command | Result |
| --- | --- |
| `cargo check -p aqua-core --lib` | Pass |
| `npm.cmd run build` (in `apps/aqua-desktop`) | Pass |
| `npm.cmd test` (in `apps/aqua-desktop`) | Pass: 12 passed, 0 failed, 1 skipped (the live graph test, gated on `TEST_FALKORDB=1`) |
| `npm.cmd run lint` (in `apps/aqua-desktop`) | Pass |
| `cargo test -p aqua-core --lib` | **Does not compile**: 8 errors, missing `include_str!` fixtures |

The missing fixtures are generated artifacts that were never committed (`documentation/model_profiles.example.json`, `documentation/model_providers.example.json`, `documentation/RUN_EVENTS.md`, `core/tests/fixtures/one_door/table.json`, `core/tests/fixtures/laya_gate_decision_cases.json`), so they cannot be recovered from history. Until they are regenerated, the frontend gate is the reproducible verification path.

## Definition of done

The Company Brain is submission-ready when a reviewer can:

1. Ingest connected company records into FalkorDB.
2. Ask a question that requires at least two graph hops.
3. See an answer composed only from FalkorDB records.
4. Inspect the exact relationship paths and source records used.
5. Approve an action whose validity is rechecked through the graph.
6. Start a later session and inspect the earlier question, sources, action, and agent handoff.
7. Disconnect FalkorDB and see the agent refuse to fabricate an answer.

## Phase 1 — Truthful vertical slice

Status: implemented and locally verified.

- Environment-only connection configuration; no successful fake graph fallback.
- Tenant-scoped company graph and connected retrieval.
- Grounded answer, source, path, assignment proposal, and approved action UI.
- Persistent question/source and action relationships.

Acceptance evidence:

- `npm.cmd test`: four Company Brain tests pass.
- `npm.cmd run build`: TypeScript and Vite production build pass.

## Phase 2 — FalkorDB in the Aqua core

Status: implemented; `cargo check -p aqua-core --lib` passes with the restored authoritative documentation and test fixtures.

- Added a FalkorDB provider behind Aqua's `ToolHost::call` boundary.
- Added eleven narrow tools: context search, path explanation, assignment proposal/action, outcome recording, ingestion, session opening, shared state, handoff, memory recall, and exchange memory.
- Registered the Company tool pack and routing vocabulary.
- Added prompt rules requiring graph retrieval, citations, and memory persistence when the tools are available.
- Registered graph sources with Aqua's existing observation/source verifier.
- Executes distinct bounded ownership, rationale, expertise, dependency, incident, and general-context traversals selected from a closed enum.
- Moved connection settings to `data/tools/company_brain.json`; the password remains in Credential Manager.
- Classified writes with approval-gated side effects.

## Phase 3 — Real ingestion and freshness

Status: implemented for the submission formats needed by the demo.

- Normalized tenant/source/entity envelope with deterministic SHA-256 content hashes.
- Markdown/frontmatter and JSON export ingestion for people, teams, projects, tickets, decisions, documents, and chats.
- Idempotent upsert/touch/reactivate/deactivate planning by `(tenant_id, source_system, external_id)`.
- Parameterized graph mutations with provenance, observed/source-updated timestamps, freshness, and active state.
- Bounded ingestion (1,000 records and 10,000 mutations per call) and sync status reporting.
- Stale and inactive evidence is excluded from answer grounding.

PDF text extraction is not included; converted Markdown/JSON is the supported deterministic demo path.

## Phase 4 — Retrieval quality and agent behavior

Status: implemented.

- Plans ownership, rationale, expertise, dependency, incident, and general intents.
- Detects ambiguous entity references before traversal.
- Uses bounded, parameterized query templates.
- Ranks evidence by entity match, freshness, authority, and path length.
- Refuses low-confidence or disconnected answers.
- Validates claims against retrieved evidence and returns structured sources/paths.

## Phase 5 — Memory, feedback, and coordination

Status: implemented.

- Models Agent, Session, Question, Answer, Action, Outcome, Feedback, SharedState, and Handoff.
- Keeps outcomes and shared-state revisions append-only.
- Allows later sessions and other agents to recall the same tenant-scoped trail.
- Supports retention policy statements and explicitly authorized tenant erasure.
- Exposes session, shared-state, handoff, recall, and exchange-memory operations through policy-controlled tools.

## Phase 6 — Submission hardening

Status: repository hardening and live validation complete; submission-owner actions remain.

Completed:

- TLS is required by default for remote FalkorDB connections. A clearly named explicit override exists only for legacy synthetic-data demo endpoints that do not offer TLS.
- Requests have origin checks, a 64 KiB body cap, per-client rate limiting, 15-second query timeouts, bounded result sizes, and structured errors.
- The query console is protected by the same-origin boundary and is not given a fallback dataset.
- Example configuration contains placeholders only.
- Frontend tests and production build pass.
- GitHub Actions runs the frontend gate and an opt-in end-to-end workflow against disposable FalkorDB 6.0.0.

Still requires project-owner access:

1. Provision/rotate a TLS-enabled FalkorDB credential outside the repository.
2. Publish the public repository.
3. Record and submit the demo video.
4. Complete registration and participant/data/IP attestations.

## Final verification commands

```powershell
cd apps/aqua-desktop
npm.cmd test
npm.cmd run build
```

For the Rust workspace, install the required native Windows GNU tooling or Linux cross-compiler, then run:

```powershell
cargo test -p aqua-core --lib company_
```
