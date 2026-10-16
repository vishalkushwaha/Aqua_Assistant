# Building Aqua Company Brain: How We Turned FalkorDB into the Decision & Memory Engine for an AI Desktop Agent

> *Submitted for **Graph Hacks: Context for AI Agents** (Track 03: Company Brain) by WeMakeDevs & FalkorDB.*

---

## The Core Problem: Why Most AI Agents Fail in Real Companies

When you deploy an AI agent inside a modern organization, you quickly hit a wall: **company knowledge does not live in isolated documents**. It is deeply interconnected across:
- **People & Teams** (Who is on the team? Who owns what service?)
- **Projects & Repositories** (What dependencies exist? Which systems are impacted?)
- **Tickets & Incidents** (Who investigated the last outage? Who has the context?)
- **Architectural Decisions & Documents** (Why did we choose OAuth2 PKCE instead of Basic Auth?)
- **Agent Memory & Actions** (What did an agent decide yesterday, and can another agent pick up where it left off?)

Standard vector retrieval (RAG) fails at this because semantic similarity cannot traverse relationship chains. If an agent is asked:
> *"Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?"*

A vector database searches for keywords or embeddings and guesses. It cannot reliably follow:
$$\text{Incident Ticket} \longrightarrow \text{Project} \longleftarrow \text{Owner (Person)} \longrightarrow \text{Team}$$
$$\text{Decision} \longrightarrow \text{Project} \longrightarrow \text{Documentation Source}$$

---

## The Solution: FalkorDB as the Context & Grounding Layer

For our project, **Aqua Company Brain**, we made a fundamental architectural decision:
**FalkorDB is not a visualization tool or optional cache—it is the single source of truth.**

If FalkorDB cannot find connected, tenant-scoped, and active evidence, **Aqua refuses to answer rather than hallucinating.**

```
+-------------------------------------------------------------+
|                     Aqua Desktop Agent                      |
| (Tauri + React UI / Local Voice Interaction / Native Tools) |
+-------------------------------------------------------------+
                              |
                     [Question or Action]
                              v
+-------------------------------------------------------------+
|             Aqua Core Policy & ToolHost Engine              |
|        (Cypher Intent Planning, Proof Verification)         |
+-------------------------------------------------------------+
                              |
               [Parameterized Cypher Traversals]
                              v
+-------------------------------------------------------------+
|                  FalkorDB Context Engine                    |
|  - Multi-hop Entity Graph (People, Teams, Projects, Docs)   |
|  - Graph-Grounded Authorization for Mutations               |
|  - Persistent Sessions, Shared State & Agent Handoffs       |
+-------------------------------------------------------------+
```

---

## 1. Multi-Hop Graph Traversal in Action

When a user asks about an incident, Aqua decomposes the query into a deterministic graph traversal rather than prompting an LLM to guess.

Here is the central retrieval query running in FalkorDB:

```cypher
MATCH (ticket:Ticket {id: $ticketId, tenant_id: $tenantId})-[:BELONGS_TO]->(project:Project)
MATCH (owner:Person)-[:OWNS]->(project)
MATCH (owner)-[:MEMBER_OF]->(team:Team)
MATCH (decision:Decision)-[:AFFECTS]->(project)
MATCH (decision)-[:SUPPORTED_BY]->(source:Source)
WHERE ticket.active = true AND source.active = true
RETURN ticket, project, owner, team, decision, source
```

Because Cypher executes directly on FalkorDB's low-latency engine, the entire multi-hop relationship resolution happens in sub-milliseconds.

---

## 2. Graph-Authorized Actions (No Hallucinated Assignments)

Most AI agents trigger API tools blindly. Aqua enforces **Graph-Controlled Action Authorization**:

Before creating an `ASSIGNED_TO` relationship or dispatching a notification, Aqua executes a pre-flight Cypher validation:

```cypher
MATCH (t:Ticket {id: $ticketId, tenant_id: $tenantId})-[:BELONGS_TO]->(p:Project)
MATCH (target:Person {name: $assigneeName})-[:OWNS]->(p)
RETURN target.name AS valid_owner
```

If the proposed assignee is not proven in the graph as an authorized owner of the project, the action is rejected immediately. Once approved, the mutation and its outcome are written back into FalkorDB with full provenance.

---

## 3. Persistent Memory & Multi-Agent Coordination (Track 02 Crossover)

Company knowledge isn't static—it includes what the agents themselves learned. In FalkorDB, we model:
- `(:Session)-[:ASKED]->(:Question)-[:PRODUCED]->(:Answer)`
- `(:Answer)-[:GROUNDED_IN]->(:Source)`
- `(:Agent)-[:PUBLISHED_STATE]->(:StateRevision)`
- `(:Agent)-[:HANDED_OFF_TO]->(:Agent)`

When a user opens a session tomorrow or another agent takes over, they don't start with a blank context window. They query the persistent graph to recall previous findings, approved actions, and agent handoffs.

---

## 4. The "Anti-Decoration" Proof: Disconnection Refusal

The rules of Graph Hacks state:
> *"The graph has to do real work. If the product still works once FalkorDB is taken out, the graph is decoration."*

We built and verified this fail-closed behavior into the core:
When FalkorDB is stopped or unreachable, Aqua does not fall back to local mock data or hallucinated answers. It halts immediately with a clear dependency error:

```
[Error] FalkorDB connection failed (127.0.0.1:6379).
Refusing to synthesize an ungrounded answer without graph proof.
```

---

## What We Learned & Key Takeaways

1. **Sub-millisecond Graph Traversal Enables True Real-Time Agents**: FalkorDB’s C-based engine means we can run multi-hop queries on every turn of an agent loop without introducing user-visible latency.
2. **Determinism Beats Semantic Guessing**: In enterprise contexts, hallucinations aren't just inconvenient—they cause real damage. Graph validation guarantees that every cited person, ticket, and decision actually exists and connects.
3. **Multi-Agent Coordination Belongs in the Graph**: Modeling agent sessions and state revisions as nodes and edges makes handoffs audit-proof and multi-tenant safe.

---

## Try It Out

- **GitHub Repository**: [https://github.com/Vaibhav-tester/Aqua_Assistant](https://github.com/Vaibhav-tester/Aqua_Assistant)
- **Demo Video (YouTube)**: [https://youtu.be/8DfhnxvLxXA](https://youtu.be/8DfhnxvLxXA)
- **Track**: Track 03 — Company Brain *(Graph Hacks: Context for AI Agents)*
