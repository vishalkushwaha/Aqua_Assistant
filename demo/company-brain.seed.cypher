// Synthetic Track 03 demo data. This is idempotent and confined to tenant_id = 'demo'.
MERGE (company:Company {id: 'company_acme', tenant_id: 'demo'})
  SET company.name = 'Acme Cloud', company.active = true
MERGE (team:Team {id: 'team_identity', tenant_id: 'demo'})
  SET team.name = 'Identity Engineering', team.active = true
MERGE (person:Person {id: 'person_priya', tenant_id: 'demo'})
  SET person.name = 'Priya Raman', person.role = 'Staff Engineer', person.active = true
MERGE (project:Project {id: 'project_identity', tenant_id: 'demo'})
  SET project.name = 'Identity Platform', project.status = 'active', project.active = true
MERGE (system:System {id: 'system_oauth_gateway', tenant_id: 'demo'})
  SET system.name = 'OAuth Gateway', system.status = 'active', system.active = true
MERGE (topic:Topic {id: 'topic_oauth_pkce', tenant_id: 'demo'})
  SET topic.name = 'OAuth and PKCE', topic.title = 'OAuth and PKCE', topic.active = true
MERGE (ticket:Ticket {id: 'INC-1042', tenant_id: 'demo'})
  SET ticket.title = 'OAuth production incident', ticket.status = 'open',
      ticket.priority = 'P1', ticket.active = true
MERGE (decision:Decision {id: 'DEC-014', tenant_id: 'demo'})
  SET decision.title = 'Use staged PKCE migration',
      decision.summary = 'Roll out PKCE by client cohort and preserve the legacy callback during migration.',
      decision.decided_at = 1760486400000, decision.active = true
MERGE (adr:Source:Document {id: 'SRC-ADR-014', tenant_id: 'demo'})
  SET adr.title = 'ADR-014: OAuth PKCE migration', adr.kind = 'document',
      adr.uri = 'company://docs/ADR-014', adr.updated_at = 1760486400000,
      adr.active = true
MERGE (chat:Source:Chat {id: 'SRC-CHAT-221', tenant_id: 'demo'})
  SET chat.title = 'Identity incident handoff', chat.kind = 'chat',
      chat.uri = 'company://chat/identity/221', chat.updated_at = 1760572800000,
      chat.active = true
MERGE (person)-[:MEMBER_OF]->(team)
MERGE (team)-[:PART_OF]->(company)
MERGE (person)-[:OWNS]->(project)
MERGE (person)-[:WORKED_ON]->(project)
MERGE (person)-[:HAS_EXPERTISE]->(topic)
MERGE (ticket)-[:BELONGS_TO]->(project)
MERGE (project)-[:DEPENDS_ON]->(system)
MERGE (decision)-[:AFFECTS]->(project)
MERGE (decision)-[:DECIDED_BY]->(person)
MERGE (decision)-[:SUPPORTED_BY]->(adr)
MERGE (decision)-[:SUPPORTED_BY]->(chat)
MERGE (ticket)-[:SUPPORTED_BY]->(chat)
MERGE (person)-[:SUPPORTED_BY]->(adr)
MERGE (project)-[:SUPPORTED_BY]->(adr)
RETURN company.id AS company_id, project.id AS project_id,
       ticket.id AS ticket_id, decision.id AS decision_id;
