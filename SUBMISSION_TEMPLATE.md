# Hackathon Submission Template: Aqua Company Brain

> **Event**: Graph Hacks: Context for AI Agents (WeMakeDevs & FalkorDB)  
> **Track**: Track 03 — Company Brain *(with crossover into Track 01: Acting on Connected Data & Track 02: Memory & Coordination)*

---

### 1. Basic Information
* **Project Name**: Aqua Company Brain
* **Tagline**: Graph-grounded private assistant using FalkorDB for context, multi-hop reasoning, action authorization, and persistent multi-agent memory.
* **Track Selected**: Track 03 — Company Brain
* **GitHub Repository URL**: `https://github.com/Vaibhav-tester/Aqua_Assistant`
* **Demo Video URL**: `https://youtu.be/8DfhnxvLxXA`
* **Release Download**: `https://github.com/Vaibhav-tester/Aqua_Assistant/releases/tag/v1.0.0`
* **Blog Post URL**: *(Paste your published Dev.to/Hashnode link here for the AirPods 5 prize)*

---

### 2. Elevator Pitch
Aqua Company Brain turns FalkorDB into the mandatory context and decision engine for an AI desktop agent. Instead of treating graphs as a visualization layer or guessing with vector embeddings, Aqua traverses connected company entities—people, teams, projects, tickets, architectural decisions, and documents—to answer complex multi-hop questions, validates ownership before triggering mutations, and maintains persistent memory and agent handoffs across sessions. If FalkorDB is disconnected, Aqua refuses to fabricate an answer.

---

### 3. The Problem It Solves
Modern corporate knowledge is fragmented across scattered tools (Slack, Jira, Confluence, GitHub). Existing RAG systems rely on semantic similarity search, which fundamentally fails when questions require multi-hop relationship understanding:
- Who is responsible for investigating an outage?
- What historical architectural decision influenced this service?
- Which verified documentation source supports the recommended resolution?

Standard LLM agents either hallucinate answers or propose dangerous actions without checking authorization.

---

### 4. How We Built It & Why FalkorDB is Essential
Aqua integrates FalkorDB directly into its core agent architecture:
1. **Multi-Hop Traversal**: Resolves complex questions via parameterized Cypher traversals (`Ticket -> Project <- Person -> Team` and `Decision -> Project -> Source`) in sub-milliseconds.
2. **Anti-Decoration Guarantee**: FalkorDB is strictly required. Grounded answers are composed solely from retrieved graph rows. When FalkorDB is stopped, Aqua halts and refuses to hallucinate ungrounded answers.
3. **Graph-Authorized Actions**: Before creating an `ASSIGNED_TO` relationship or taking action, Aqua rechecks the ownership graph in Cypher to prevent unauthorized assignments.
4. **Persistent Memory & Multi-Agent Coordination**: Models `Session`, `Question`, `Answer`, `StateRevision`, and `Handoff` as persistent graph entities so subsequent sessions and separate agents share the same audit trail.
5. **Deterministic Ingestion**: Normalizes Markdown/frontmatter and JSON records with SHA-256 content hashes, freshness deadlines, and active state flags.

---

### 5. Tech Stack
- **Graph Database**: FalkorDB (Cypher query language, multi-tenant topology)
- **Desktop Engine**: Tauri, Rust (Aqua Core, Policy-Controlled ToolHost)
- **Frontend UI**: React 18, TypeScript, Vite, Lucide Icons, Tailwind CSS
- **Local Speech & Audio**: Offline neural Kokoro TTS, Whisper local ASR

---

### 6. Challenges We Ran Into
- **Strict Grounding without LLM Hallucination**: Ensuring that the agent composes responses strictly from graph facts and refuses when graph paths are broken or stale.
- **Action Validation**: Designing a pre-flight Cypher check that verifies ownership authority before allowing mutations.
- **Multi-Tenant State Modeling**: Partitioning memory and shared states by tenant so that multi-agent handoffs maintain complete tenant isolation.

---

### 7. Accomplishments We're Proud Of
- Passing all 12 frontend behavior and grounding tests out-of-the-box.
- Demonstrating verifiable graph dependency: stopping FalkorDB produces an explicit refusal rather than a hallucinated fallback.
- Sub-millisecond graph query performance allowing continuous graph traversals on every turn of the agent loop.
- Providing a fully synthetic, reproducible dataset that runs with zero cloud setup via Docker Compose.

---

### 8. What's Next for Aqua Company Brain
- Extending ingestion to live webhook listeners for GitHub, Jira, and Slack.
- Adding automated community detection algorithms to infer cross-functional project pods.
- Expanding multi-agent consensus voting on high-impact infrastructure changes.
