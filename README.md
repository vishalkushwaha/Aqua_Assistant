<div align="center">
  <img src="assets/aqua-logo.png" alt="Aqua Logo" width="100" height="100" />
  <h1>Aqua Company Brain</h1>
  <p><strong>Graph-Grounded Private AI Agent powered by FalkorDB</strong></p>
  <p>
    <a href="https://youtu.be/8DfhnxvLxXA"><img src="https://img.shields.io/badge/YouTube-Watch%20Demo-red?style=flat-square&logo=youtube" alt="YouTube Demo" /></a>
    <a href="https://github.com/Vaibhav-tester/Aqua_Assistant/releases/tag/v1.0.0"><img src="https://img.shields.io/badge/Release-v1.0.0-blue?style=flat-square" alt="Release" /></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-green?style=flat-square" alt="License" /></a>
    <a href="https://www.wemakedevs.org/hackathons/falkordb"><img src="https://img.shields.io/badge/WeMakeDevs-Graph%20Hacks%20Track%2003-purple?style=flat-square" alt="Hackathon" /></a>
  </p>
</div>

Aqua Company Brain is a graph-grounded agent for **Graph Hacks Track 03 - Company Brain**. It connects people, teams, projects, tickets, decisions, documents, chats, actions, and agent memories in FalkorDB. The graph is not a visualization layer: it determines the answer, validates the proposed assignment, supplies the cited paths, and stores later-session and multi-agent state.

The focused demo question is:

> Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?

If FalkorDB is unavailable or no connected evidence exists, Company Brain refuses to synthesize an answer.

## What the agent does

1. Follows a multi-hop incident, project, owner, team, decision, and source path.
2. Filters inactive or stale evidence and ranks current connected records.
3. Returns the exact relationship paths and source records behind the answer.
4. Proposes and executes an assignment only when FalkorDB confirms the ownership path.
5. Stores questions, answers, actions, outcomes, state revisions, and handoffs for later sessions and agents.

## Demo Video & Walkthrough

A narrated, captioned walkthrough demonstrating live FalkorDB retrieval, explainability, graph-controlled action, persistent memory, and disconnection refusal:

[![Aqua Company Brain Walkthrough](demo/video/demo-preview.gif)](https://youtu.be/8DfhnxvLxXA)

> 🎬 **[▶️ Watch Demo on YouTube](https://youtu.be/8DfhnxvLxXA)**  
> 📹 **GitHub Player**: [Play `aqua-company-brain-demo.mp4` on GitHub](https://github.com/Vaibhav-tester/Aqua_Assistant/blob/main/demo/video/aqua-company-brain-demo.mp4) | [💾 Direct Download](https://github.com/Vaibhav-tester/Aqua_Assistant/releases/download/v1.0.0/aqua-company-brain-demo.mp4)  
> 📝 **Captions/Subtitles**: [📄 `demo/video/captions.srt`](demo/video/captions.srt) | 📖 **Step-by-step Runbook**: [`docs/DEMO_RUNBOOK.md`](docs/DEMO_RUNBOOK.md)

### Walkthrough Timeline:
- **0:00 - 0:16**: Problem statement & tenant-scoped company knowledge model in FalkorDB.
- **0:16 - 0:32**: Live multi-hop graph retrieval (`Ticket -> Project <- Person -> Team`) and citing grounded sources.
- **0:32 - 0:40**: Graph-authorized action: revalidates ownership via Cypher before creating `ASSIGNED_TO`.
- **0:40 - 0:48**: Persistent memory recall across sessions from FalkorDB.
- **0:48 - 0:56**: Multi-agent coordination: handoff to second agent with shared graph context.

## 🖼️ User Interface & System Showcase

### 1. Neural Voice Orb & Desktop Assistant
Aqua features an always-available floating desktop **Neural Voice Orb** that handles hands-free speech, system control, and workspace context:

![Aqua Neural Voice Orb](assets/aqua-orb-assistant.png)

---

### 2. Company Brain Knowledge Graph Dashboard
Ask complex enterprise questions and receive answers composed exclusively from FalkorDB records, complete with verified multi-hop paths, citations, and authority scores:

![Aqua Company Brain Dashboard](assets/company-brain-dashboard.png)

---

### 3. Graph-Authorized Action Execution
Before modifying team tickets or triggering mutations, Aqua executes pre-flight Cypher traversals (`Ticket -> Project <- Person`) to revalidate ownership:

![Graph-Authorized Action Execution](assets/action-validation-dashboard.png)

---

### 4. Persistent Memory Graph & Multi-Agent Coordination
Sessions, questions, answers, source citations, state revisions, and agent handoffs are saved as persistent graph entities in FalkorDB:

![FalkorDB Persistent Memory Graph](assets/memory-graph-dashboard.png)

## Architecture

```mermaid
flowchart LR
    UI[Aqua Company Brain UI] -->|question or approved action| Bridge[Vite FalkorDB bridge]
    Agent[Aqua agent] -->|company.* tool| Host[Policy-aware ToolHost]
    Host --> Provider[Company Brain provider]
    Bridge --> DB[(FalkorDB)]
    Provider --> DB
    Ingest[Markdown / JSON ingestion] --> Provider
    DB -->|connected facts, paths, sources| Bridge
    DB -->|tenant-scoped memory and handoffs| Provider
```

All normal Aqua tools execute through `ToolHost::call`, which validates input, applies policy and approvals, masks secrets, and audits the call. The Vite bridge is a loopback development/demo route, not a production database gateway.

## Graph model

```mermaid
graph LR
    Person -->|MEMBER_OF| Team
    Team -->|PART_OF| Company
    Person -->|OWNS| Project
    Person -->|WORKED_ON| Project
    Person -->|HAS_EXPERTISE| Topic
    Ticket -->|BELONGS_TO| Project
    Project -->|DEPENDS_ON| System
    Decision -->|AFFECTS| Project
    Decision -->|DECIDED_BY| Person
    Decision -->|SUPPORTED_BY| Source
    Ticket -->|ASSIGNED_TO| Person
    Action -->|ACTED_ON| Ticket
    Action -->|SELECTED_FROM_GRAPH| Person
    Agent -->|PARTICIPATED_IN| Session
    Session -->|CONTAINS| Question
    Question -->|ANSWERED_BY| Answer
    Agent -->|PRODUCED| Answer
    Answer -.->|source_ids| Source
    Agent -->|PUBLISHED_STATE| StateRevision
    Agent -->|HANDED_OFF| Handoff
```

Every company and memory record is tenant-scoped. Ingested records also carry a source identity, content hash, observed time, source update time, freshness deadline, and active state.

## Quick start

Requirements:

- An accessible FalkorDB instance (a self-hosted server or FalkorDB Cloud; a TLS-enabled endpoint for remote hosts)
- Node.js 18 or newer
- npm

From the repository root:

```powershell
Set-Location apps/aqua-desktop
Copy-Item .env.example .env
# Fill in the FALKORDB_HOST, FALKORDB_PORT, FALKORDB_GRAPH, FALKORDB_USER and
# FALKORDB_PASSWORD values for the instance you will use.
npm.cmd install
npm.cmd run dev
```

Open `http://127.0.0.1:1420`, choose **Company Brain**, and select **Prepare demo company**. Ask the prefilled question, inspect its paths and sources, then approve the graph-backed assignment.

Connection values are read from `apps/aqua-desktop/.env`; `vite.config.ts` passes them to the bridge. Remote hosts require TLS (`FALKORDB_TLS=true`). The password belongs in the environment file or the operating system Credential Manager, never in a committed file.

The `FALKORDB_ALLOW_INSECURE_REMOTE=true` override exists only for synthetic-data demo endpoints that do not offer TLS. Use it with synthetic data only, and replace such an endpoint with a TLS-enabled one before handling confidential information.

`docker-compose.company-brain.yml` is kept in the repository for anyone who prefers to run FalkorDB locally in a container. It is an optional convenience, not a requirement for the quick start above.

See the [demo runbook](docs/DEMO_RUNBOOK.md) for reset, failure-mode, memory, and recording steps.

## Aqua core configuration

The core agent exposes eleven policy-controlled tools:

- `company.search_context`, `company.explain_path`, and `company.propose_assignment`
- `company.assign_ticket` and `company.record_outcome`
- `company.ingest`
- `company.open_session`, `company.remember_exchange`, and `company.recall_memory`
- `company.share_state` and `company.handoff`

For a normal Aqua agent run, configure and enable `data/tools/company_brain.json`. Keep passwords in the operating-system Credential Manager and reference the secret from configuration; do not put a password in the JSON file. Remote database connections require TLS. The desktop quick start above is the primary independently reproducible Track 03 path.

The full Aqua core currently has a path dependency on the `laya-candle` source under `vendor/laya_rust/laya-rs`. In this working tree, `vendor` is a machine-local Windows junction and is intentionally excluded from publication. Before advertising the full-core build as reproducible, replace that junction with a portable, license-compatible dependency source (for example a documented Git dependency, submodule, or vendored source that the project is allowed to redistribute), then verify the Rust commands below from a fresh clone. Do not publish the current junction target or assume another machine has it.

## Why FalkorDB is essential

The central retrieval joins five connected patterns:

```cypher
MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)
MATCH (owner:Person)-[:OWNS]->(project)
MATCH (owner)-[:MEMBER_OF]->(team:Team)
MATCH (decision:Decision)-[:AFFECTS]->(project)
MATCH (decision)-[:SUPPORTED_BY]->(source:Source)
WHERE ticket.tenant_id = $tenantId
RETURN ticket, project, owner, team, decision, source
```

The assignment mutation independently rechecks `Ticket -> Project <- Person`. Removing FalkorDB therefore removes the facts, explanation, action authorization, and persistent memory; the product returns a dependency error rather than continuing from ordinary storage.

See [docs/cypher.md](docs/cypher.md) for the complete bounded traversal, freshness conditions, provenance write, graph-validated action, ingestion strategy, memory queries, and tenant-safe reset.

## Synthetic demo data

The included Acme Cloud dataset is synthetic and contains no real company or personal data. The desktop creates it idempotently. Portable seed and reset queries are in `demo/company-brain.seed.cypher` and `demo/company-brain.reset.cypher`.

Read [docs/DATA_POLICY.md](docs/DATA_POLICY.md) before adding any source. Do not ingest private, login-protected, paywalled, personal, confidential, or otherwise restricted material.

## Tests and verification

Frontend checks:

```powershell
Set-Location apps/aqua-desktop
npm.cmd test
npm.cmd run lint
npm.cmd run build
```

Focused Rust check, on a machine with the required native toolchain and sufficient disk space:

```powershell
cargo check -p aqua-core --lib
```

This passes on the development machine. The `core_` test command is deliberately not listed here: `cargo test -p aqua-core --lib` does not currently compile, because several `include_str!` fixtures it needs are not present in this working tree (the `documentation/*.example.json` files, `documentation/RUN_EVENTS.md`, and `core/tests/fixtures/one_door/table.json`). Those files are generated artifacts and were never committed, so they cannot be restored from history. Until they are regenerated, the frontend checks above are the reproducible verification path for a reviewer.

Live acceptance passed on October 16, 2026 against the configured `AquaCompanyBrain` graph: seed, sourced multi-hop answer, graph-authorized action, persisted recall, invalid-owner rejection, disconnected-database refusal, and configuration recovery all passed. Run `npm.cmd run test:integration` again against the exact deployment submitted to judges.

The final desktop workflow also persists a tenant-scoped `Agent -> Session -> Question -> Answer -> Source` trail, action outcomes, and a handoff to a second agent. The checked-in narrated and captioned demo video is [`demo/video/aqua-company-brain-demo.mp4`](demo/video/aqua-company-brain-demo.mp4). It demonstrates Company Brain, the live memory graph, Aqua's wider assistant capabilities, voice controls, and the validated Windows platform target. To recapture it from the configured live graph and render it with Aqua's offline neural Kokoro voice:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/render-company-brain-demo.ps1
```

The `Company Brain` GitHub Actions workflow runs the frontend checks, scoped Rust formatting, and the opt-in workflow against a disposable FalkorDB 6.0.0 service for every push and pull request.

## Submission checklist

- Track: **Track 03 - Company Brain**
- Public repository URL is reachable without authentication.
- A fresh public clone contains no junctions or machine-local absolute dependencies.
- The full-core `laya-candle` dependency is made portable, or the submission clearly scopes the reproducible path to the desktop plus FalkorDB demo.
- This README and the graph/Cypher documentation match the submitted revision.
- Tests and a live FalkorDB workflow have been run on the submitted revision.
- Demo video shows retrieval, paths, sources, action validation, persistent session memory, and agent handoff.
- Video URL is reachable without authentication.
- Team has no more than four members; every member is registered and on only this team.
- Main submitted design/coding work was completed during the hackathon window.
- Dataset rights and synthetic-data declaration are confirmed.
- AI-assistance disclosure below is accurate.
- Every participant can explain the graph model, queries, architecture, and tradeoffs.

Publishing the repository, registering the team, recording/uploading the video, and confirming participant/data attestations are owner actions; they cannot be completed safely by source-code automation.

## AI-assistance disclosure

AI coding assistants and multiple AI implementation agents were used for planning, TypeScript and Rust implementation, Cypher authoring, tests, integration, review, and documentation. The participant is responsible for reviewing every submitted change, running the documented verification, understanding the architecture and queries, managing credentials, confirming dataset rights, and accurately presenting what was personally verified.

## Limitations

- PDF extraction is not included; deterministic ingestion supports Markdown/frontmatter and JSON exports.
- The Vite FalkorDB bridge is for loopback development and demos, not public deployment.
- The full Aqua core is not reproducible from this working tree alone while its `laya-candle` dependency points through a machine-local `vendor` junction; the frontend plus FalkorDB demo does not use that dependency.
- A live synthetic-data database and local demo video artifact have been verified. A public repository URL, TLS-enabled production endpoint, and externally reachable video URL remain external submission requirements.
- Track 03 is the supported submission position. Claims for Tracks 01 or 02 require their additional MCP/algorithm or concurrency evidence.

## Key implementation files

- `core/src/company_brain.rs` - FalkorDB provider and policy-controlled tools
- `core/src/company_ingest.rs` - normalization, synchronization, provenance, and freshness
- `core/src/company_retrieval.rs` - retrieval planning, ranking, ambiguity, refusal, and claim checks
- `core/src/company_memory.rs` - persistent memory, shared state, handoffs, retention, and deletion
- `apps/aqua-desktop/companyBrain.ts` - graph traversal and grounded answer composition
- `apps/aqua-desktop/falkorPlugin.ts` - loopback FalkorDB demo bridge
- `apps/aqua-desktop/src/components/CompanyBrainPage.tsx` - answer, paths, sources, and action UI
- `apps/aqua-desktop/tests/companyBrain.test.ts` - grounding and refusal behavior tests

## License and ownership

This project is licensed under the [MIT License](LICENSE). Intellectual property developed during the hackathon belongs to the participant/team under the Graph Hacks rules.
