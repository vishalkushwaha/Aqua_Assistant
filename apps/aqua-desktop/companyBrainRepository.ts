import {
  COMPANY_CONTEXT_QUERY,
  composeCompanyAnswer,
  searchNeedle,
  type CompanyBrainAnswer,
  type CompanyBrainRow,
} from './companyBrain.ts';

export const DEFAULT_COMPANY_TENANT_ID = 'demo';
export const DEFAULT_COMPANY_SESSION_ID = 'session_demo_primary';

export interface GraphQueryResult {
  data?: unknown[];
  statistics?: Record<string, unknown>;
}

export type CompanyGraph = unknown;
export type CompanyGraphProvider = () => Promise<CompanyGraph>;
export type CompanyGraphQuery = (
  graph: CompanyGraph,
  cypher: string,
  options?: { params?: Record<string, unknown> },
) => Promise<GraphQueryResult>;

export interface CompanyAnswer extends CompanyBrainAnswer {
  interactionId: string;
  answerId: string;
  sessionId: string;
  graphRequired: true;
}

export interface CompanyMemory {
  sessionId: string;
  questions: Array<{
    questionId: unknown;
    question: unknown;
    answerId: unknown;
    answer: unknown;
    sourceIds: unknown[];
  }>;
  actions: Array<{
    actionId: unknown;
    kind: unknown;
    status: unknown;
    targetId: unknown;
  }>;
  handoffs: Array<{
    handoffId: unknown;
    fromAgent: unknown;
    toAgent: unknown;
    summary: unknown;
  }>;
}

const COMPANY_SEED_QUERY = `
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
    SET ticket.title = 'OAuth production incident', ticket.status = 'open', ticket.priority = 'P1', ticket.active = true
  MERGE (decision:Decision {id: 'DEC-014', tenant_id: 'demo'})
    SET decision.title = 'Use staged PKCE migration',
        decision.summary = 'Roll out PKCE by client cohort and preserve the legacy callback during migration.',
        decision.decided_at = 1760486400000, decision.active = true
  MERGE (adr:Source:Document {id: 'SRC-ADR-014', tenant_id: 'demo'})
    SET adr.title = 'ADR-014: OAuth PKCE migration', adr.kind = 'document',
        adr.uri = 'company://docs/ADR-014', adr.updated_at = 1760486400000, adr.active = true
  MERGE (chat:Source:Chat {id: 'SRC-CHAT-221', tenant_id: 'demo'})
    SET chat.title = 'Identity incident handoff', chat.kind = 'chat',
        chat.uri = 'company://chat/identity/221', chat.updated_at = 1760572800000, chat.active = true
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
    ticket.id AS ticket_id, decision.id AS decision_id
`;

function queryRows(result: GraphQueryResult): CompanyBrainRow[] {
  if (!Array.isArray(result.data)) return [];
  return result.data.filter((row: unknown): row is CompanyBrainRow => Boolean(row) && typeof row === 'object' && !Array.isArray(row));
}

function rows(result: GraphQueryResult): any[] {
  return Array.isArray(result.data) ? result.data : [];
}

/**
 * Reusable Company Brain database boundary. HTTP, MCP and desktop callers can
 * share these operations without depending on Vite request/response objects.
 */
export class CompanyBrainRepository {
  private readonly graph: CompanyGraphProvider;
  private readonly query: CompanyGraphQuery;
  private readonly now: () => number;

  constructor(
    graph: CompanyGraphProvider,
    query: CompanyGraphQuery,
    now: () => number = Date.now,
  ) {
    this.graph = graph;
    this.query = query;
    this.now = now;
  }

  async seed(): Promise<{ data: unknown[]; statistics: Record<string, unknown> }> {
    const result = await this.query(await this.graph(), COMPANY_SEED_QUERY);
    return {
      data: Array.isArray(result.data) ? result.data : [],
      statistics: result.statistics || {},
    };
  }

  async ask(question: string, tenantId: string, sessionId: string): Promise<CompanyAnswer | null> {
    const graph = await this.graph();
    let result = await this.query(graph, COMPANY_CONTEXT_QUERY, {
      params: { tenantId, needle: searchNeedle(question) },
    });
    let evidence = queryRows(result);
    if (evidence.length === 0) {
      result = await this.query(graph, COMPANY_CONTEXT_QUERY, { params: { tenantId, needle: '' } });
      evidence = queryRows(result);
    }
    const grounded = composeCompanyAnswer(question, evidence);
    if (!grounded) return null;

    const interactionId = `question_${this.now()}`;
    const answerId = `answer_${this.now()}`;
    await this.query(graph, `
      MERGE (agent:Agent {id: 'agent_aqua', tenant_id: $tenantId})
        SET agent.name = 'Aqua Company Brain', agent.role = 'company_brain'
      MERGE (user:User {id: 'user_demo', tenant_id: $tenantId})
        SET user.name = 'Demo Operator'
      MERGE (session:Session {id: $sessionId, tenant_id: $tenantId})
        ON CREATE SET session.created_at = timestamp()
      CREATE (q:Question {
        id: $interactionId, tenant_id: $tenantId, text: $question,
        intent: $intent, answered_at: timestamp()
      })
      CREATE (a:Answer {
        id: $answerId, tenant_id: $tenantId, text: $answer,
        created_at: timestamp()
      })
      MERGE (agent)-[:PARTICIPATED_IN]->(session)
      MERGE (user)-[:OWNS_SESSION]->(session)
      CREATE (session)-[:CONTAINS]->(q)
      CREATE (q)-[:ANSWERED_BY]->(a)
      CREATE (agent)-[:PRODUCED]->(a)
      WITH q, a
      MATCH (s:Source) WHERE s.tenant_id = $tenantId AND s.id IN $sourceIds
      MERGE (q)-[:ANSWERED_WITH]->(s)
      MERGE (a)-[:GROUNDED_IN]->(s)
      RETURN q.id AS interaction_id, a.id AS answer_id
    `, { params: {
      interactionId,
      answerId,
      sessionId,
      tenantId,
      question,
      answer: grounded.answer,
      intent: grounded.intent,
      sourceIds: grounded.sources.map((source) => source.id),
    } });

    return {
      ...grounded,
      interactionId,
      answerId,
      sessionId,
      graphRequired: true,
    };
  }

  async assignTicket(ticketId: string, assigneeId: string, tenantId: string, sessionId: string): Promise<{ actionId: string; data: any[] } | null> {
    const graph = await this.graph();
    const actionId = `action_${this.now()}`;
    const result = await this.query(graph, `
      MATCH (ticket:Ticket {id: $ticketId, tenant_id: $tenantId})-[:BELONGS_TO]->(project:Project)
      MATCH (person:Person {id: $assigneeId, tenant_id: $tenantId})-[:OWNS]->(project)
      MERGE (ticket)-[assignment:ASSIGNED_TO]->(person)
        SET assignment.assigned_at = timestamp(), assignment.assigned_by = 'company_brain_agent'
      CREATE (action:Action {
        id: $actionId, kind: 'assign_ticket', tenant_id: $tenantId,
        status: 'completed', created_at: timestamp()
      })
      CREATE (action)-[:ACTED_ON]->(ticket)
      CREATE (action)-[:SELECTED_FROM_GRAPH]->(person)
      WITH action, ticket, person
      MERGE (agent:Agent {id: 'agent_aqua', tenant_id: $tenantId})
        SET agent.name = 'Aqua Company Brain', agent.role = 'company_brain'
      MERGE (session:Session {id: $sessionId, tenant_id: $tenantId})
      MERGE (session)-[:CONTAINS]->(action)
      MERGE (agent)-[:PERFORMED]->(action)
      CREATE (outcome:Outcome {
        id: $outcomeId, tenant_id: $tenantId, status: 'success',
        summary: 'Ticket assignment validated and completed', recorded_at: timestamp()
      })
      CREATE (action)-[:RESULTED_IN]->(outcome)
      RETURN action.id AS action_id, ticket.id AS ticket_id, person.name AS assignee
    `, { params: {
      ticketId,
      assigneeId,
      tenantId,
      actionId,
      outcomeId: `outcome_${this.now()}`,
      sessionId,
    } });
    const data = rows(result);
    return data.length === 0 ? null : { actionId, data };
  }

  async recallMemory(tenantId: string, sessionId: string): Promise<CompanyMemory> {
    const graph = await this.graph();
    const questions = await this.query(graph, `
      MATCH (session:Session {id: $sessionId, tenant_id: $tenantId})-[:CONTAINS]->(q:Question)
      MATCH (q)-[:ANSWERED_BY]->(a:Answer)
      OPTIONAL MATCH (a)-[:GROUNDED_IN]->(source:Source)
      RETURN q.id AS question_id, q.text AS question, a.id AS answer_id,
        a.text AS answer, collect(DISTINCT source.id) AS source_ids,
        q.answered_at AS answered_at
      ORDER BY answered_at DESC LIMIT 10
    `, { params: { tenantId, sessionId } });
    const actions = await this.query(graph, `
      MATCH (session:Session {id: $sessionId, tenant_id: $tenantId})-[:CONTAINS]->(action:Action)
      OPTIONAL MATCH (action)-[:ACTED_ON]->(target)
      RETURN action.id AS action_id, action.kind AS kind, action.status AS status,
        target.id AS target_id, action.created_at AS created_at
      ORDER BY created_at DESC LIMIT 10
    `, { params: { tenantId, sessionId } });
    const handoffs = await this.query(graph, `
      MATCH (session:Session {id: $sessionId, tenant_id: $tenantId})-[:CONTAINS]->(handoff:Handoff)
      MATCH (from:Agent)-[:HANDED_OFF]->(handoff)-[:TO_AGENT]->(to:Agent)
      RETURN handoff.id AS handoff_id, from.name AS from_agent,
        to.name AS to_agent, handoff.summary AS summary,
        handoff.created_at AS created_at
      ORDER BY created_at DESC LIMIT 10
    `, { params: { tenantId, sessionId } });
    return {
      sessionId,
      questions: rows(questions).map((row) => ({ questionId: row.question_id, question: row.question, answerId: row.answer_id, answer: row.answer, sourceIds: row.source_ids || [] })),
      actions: rows(actions).map((row) => ({ actionId: row.action_id, kind: row.kind, status: row.status, targetId: row.target_id })),
      handoffs: rows(handoffs).map((row) => ({ handoffId: row.handoff_id, fromAgent: row.from_agent, toAgent: row.to_agent, summary: row.summary })),
    };
  }

  async createHandoff(tenantId: string, sessionId: string, summary: string): Promise<string> {
    const handoffId = `handoff_${this.now()}`;
    await this.query(await this.graph(), `
      MERGE (session:Session {id: $sessionId, tenant_id: $tenantId})
      MERGE (from:Agent {id: 'agent_aqua', tenant_id: $tenantId})
        SET from.name = 'Aqua Company Brain', from.role = 'company_brain'
      MERGE (to:Agent {id: 'agent_incident', tenant_id: $tenantId})
        SET to.name = 'Incident Response Agent', to.role = 'incident_response'
      CREATE (handoff:Handoff {
        id: $handoffId, tenant_id: $tenantId, summary: $summary,
        created_at: timestamp()
      })
      MERGE (session)-[:CONTAINS]->(handoff)
      CREATE (from)-[:HANDED_OFF]->(handoff)
      CREATE (handoff)-[:TO_AGENT]->(to)
      RETURN handoff.id AS handoff_id
    `, { params: { tenantId, sessionId, handoffId, summary: summary.slice(0, 32000) } });
    return handoffId;
  }
}
