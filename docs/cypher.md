# Company Brain Cypher

These are the graph operations that make FalkorDB a required part of the product. Runtime queries are parameterized; user text is not inserted into query structure. The executable definitions live in `apps/aqua-desktop/companyBrain.ts`, `apps/aqua-desktop/falkorPlugin.ts`, `core/src/company_brain.rs`, `core/src/company_ingest.rs`, and `core/src/company_memory.rs`.

## Connected context traversal

The main answer requires a ticket-to-project ownership path and a decision-to-source evidence path in one tenant-scoped traversal:

```cypher
MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)
MATCH (owner:Person)-[:OWNS]->(project)
MATCH (owner)-[:MEMBER_OF]->(team:Team)
MATCH (decision:Decision)-[:AFFECTS]->(project)
MATCH (decision)-[:SUPPORTED_BY]->(source:Source)
WHERE ticket.tenant_id = $tenantId
  AND coalesce(ticket.active, true)
  AND coalesce(project.active, true)
  AND coalesce(owner.active, true)
  AND coalesce(decision.active, true)
  AND coalesce(source.active, true)
  AND (ticket.stale_at IS NULL OR ticket.stale_at > timestamp())
  AND (project.stale_at IS NULL OR project.stale_at > timestamp())
  AND (owner.stale_at IS NULL OR owner.stale_at > timestamp())
  AND (decision.stale_at IS NULL OR decision.stale_at > timestamp())
  AND ($needle = '' OR toLower(ticket.title) CONTAINS $needle
    OR toLower(project.name) CONTAINS $needle
    OR toLower(decision.title) CONTAINS $needle)
RETURN ticket.id AS ticket_id, ticket.title AS ticket_title,
  project.id AS project_id, project.name AS project_name,
  owner.id AS owner_id, owner.name AS owner_name, owner.role AS owner_role,
  team.name AS team_name,
  decision.id AS decision_id, decision.title AS decision_title,
  decision.summary AS decision_summary,
  source.id AS source_id, source.title AS source_title,
  source.kind AS source_kind, source.uri AS source_uri,
  source.updated_at AS source_updated_at
ORDER BY source.updated_at DESC
LIMIT 10
```

The answer composer accepts only rows returned by this traversal. Zero rows cause a refusal; database failure returns an error instead of a cached or model-only answer.

## Intent-specific core traversals

The normal Aqua agent selects one of six closed templates; the model never supplies Cypher. Every template also applies the configured tenant, active state, freshness deadline, source provenance, and `LIMIT 100`.

```cypher
// Ownership
MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)<-[:OWNS]-(owner:Person)
MATCH (ticket)-[:SUPPORTED_BY]->(source:Source)

// Decision rationale
MATCH (decision:Decision)-[:AFFECTS]->(project:Project)
MATCH (decision)-[:SUPPORTED_BY]->(source:Source)

// Expertise
MATCH (person:Person)-[:HAS_EXPERTISE]->(topic:Topic)
MATCH (person)-[:SUPPORTED_BY]->(source:Source)
OPTIONAL MATCH (person)-[:WORKED_ON]->(project:Project)

// Dependency impact
MATCH (project:Project)-[dependency:DEPENDS_ON|USES]->(target)
MATCH (project)-[:SUPPORTED_BY]->(source:Source)

// Incident context uses project ownership before assignment
MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)<-[:OWNS]-(owner:Person)
MATCH (ticket)-[:SUPPORTED_BY]->(source:Source)

// General connected context
MATCH (subject)-[connection]->(connected)
MATCH (subject)-[:SUPPORTED_BY]->(source:Source)
```

The complete return projections and freshness predicates are fixed in `core/src/company_brain.rs`.

## Graph-validated action

Assignment succeeds only when the proposed person owns the project connected to the requested ticket. The same transaction records why the agent selected that person:

```cypher
MATCH (ticket:Ticket {id: $ticketId, tenant_id: $tenantId})
      -[:BELONGS_TO]->(project:Project)
MATCH (person:Person {id: $assigneeId, tenant_id: $tenantId})
      -[:OWNS]->(project)
MERGE (ticket)-[assignment:ASSIGNED_TO]->(person)
  SET assignment.assigned_at = timestamp(),
      assignment.assigned_by = 'company_brain_agent'
CREATE (action:Action {
  id: $actionId, kind: 'assign_ticket', tenant_id: $tenantId,
  status: 'completed', created_at: timestamp()
})
CREATE (action)-[:ACTED_ON]->(ticket)
CREATE (action)-[:SELECTED_FROM_GRAPH]->(person)
RETURN action.id AS action_id, ticket.id AS ticket_id,
       person.name AS assignee
```

No matching ownership path means no action row and an HTTP `409` response.

## Answer provenance

Each desktop question is connected to every source used in its answer:

```cypher
CREATE (question:Question {
  id: $interactionId, tenant_id: $tenantId, text: $question,
  intent: $intent, answered_at: timestamp()
})
WITH question
MATCH (source:Source)
WHERE source.tenant_id = $tenantId AND source.id IN $sourceIds
MERGE (question)-[:ANSWERED_WITH]->(source)
RETURN question.id AS interaction_id
```

## Ingestion and freshness

The core ingestion layer normalizes records, calculates a deterministic content hash, and selects an upsert, touch, reactivation, explicit deletion, or snapshot deactivation. Labels and relationship types come from Rust enums; values are passed separately as parameters. A representative upsert is:

```cypher
MERGE (node:Ticket {
  tenant_id: $tenant_id,
  source_system: $source_system,
  external_id: $external_id
})
SET node.id = $external_id,
    node.source_id = $source_id,
    node.source_uri = $source_uri,
    node.title = $title,
    node.content_hash = $content_hash,
    node.observed_at = $observed_at_ms,
    node.source_updated_at = $source_updated_at_ms,
    node.stale_at = $stale_at_ms,
    node.active = true
RETURN node.external_id AS external_id
```

The ingestion planner also creates `SUPPORTED_BY` provenance edges and enum-selected connected-data edges such as `MEMBER_OF`, `OWNS`, `BELONGS_TO`, `AFFECTS`, `AUTHORED_BY`, and `REFERENCES`. Complete snapshots deactivate missing records rather than silently treating them as current.

## Persistent memory and agent coordination

Session memory is tenant-scoped and stored in FalkorDB:

```cypher
MERGE (agent:Agent {tenant_id: $tenant_id, id: $agent_id})
MERGE (session:Session {tenant_id: $tenant_id, id: $session_id})
  ON CREATE SET session.user_id = $user_id,
                session.created_at = timestamp()
MERGE (agent)-[:PARTICIPATED_IN]->(session)
RETURN session.id AS session_id
```

Shared state uses append-only `StateRevision` nodes, and handoffs connect the source agent, destination agent, and session. Recall starts at the tenant-scoped session and returns the event and related nodes. Exact fixed templates are in `core/src/company_memory.rs`.

## Deterministic demo and reset

The portable synthetic seed is [`demo/company-brain.seed.cypher`](../demo/company-brain.seed.cypher). The seed uses `MERGE` and is safe to repeat. The reset is intentionally limited to the demo tenant:

```cypher
MATCH (node)
WHERE node.tenant_id = 'demo'
DETACH DELETE node
```

Do not run an unscoped deletion on a shared graph.
