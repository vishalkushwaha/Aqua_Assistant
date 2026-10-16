export type CompanyBrainIntent = 'ownership' | 'decision' | 'project_context';

export interface CompanyBrainSource {
  id: string;
  title: string;
  kind: string;
  uri: string;
  updatedAt?: number;
}

export interface CompanyBrainAnswer {
  answer: string;
  intent: CompanyBrainIntent;
  action: {
    kind: 'assign_ticket';
    label: string;
    ticketId: string;
    assigneeId: string;
  } | null;
  paths: string[][];
  sources: CompanyBrainSource[];
  facts: Record<string, string>;
}

export interface CompanyBrainRow {
  ticket_id?: unknown;
  ticket_title?: unknown;
  project_id?: unknown;
  project_name?: unknown;
  owner_id?: unknown;
  owner_name?: unknown;
  owner_role?: unknown;
  team_name?: unknown;
  decision_id?: unknown;
  decision_title?: unknown;
  decision_summary?: unknown;
  source_id?: unknown;
  source_title?: unknown;
  source_kind?: unknown;
  source_uri?: unknown;
  source_updated_at?: unknown;
}

const text = (value: unknown): string => value == null ? '' : String(value);

/** Selects which graph traversal the agent needs. This decision is reported with every answer. */
export function classifyCompanyQuestion(question: string): CompanyBrainIntent {
  const q = question.toLocaleLowerCase();
  if (/\b(who|owner|owns|assign|handle|responsible)\b/.test(q)) return 'ownership';
  if (/\b(why|decision|decided|rationale|reason)\b/.test(q)) return 'decision';
  return 'project_context';
}

/**
 * One connected traversal deliberately returns the owner, decision, and evidence together.
 * The answer is therefore impossible to produce when the graph query fails.
 */
export const COMPANY_CONTEXT_QUERY = `
  MATCH (ticket:Ticket)-[:BELONGS_TO]->(project:Project)
  MATCH (owner:Person)-[:OWNS]->(project)
  MATCH (owner)-[:MEMBER_OF]->(team:Team)
  MATCH (decision:Decision)-[:AFFECTS]->(project)
  MATCH (decision)-[:SUPPORTED_BY]->(source:Source)
  WHERE ticket.tenant_id = $tenantId
    AND coalesce(ticket.active, true) AND coalesce(project.active, true)
    AND coalesce(owner.active, true) AND coalesce(decision.active, true)
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
`;

const STOP_WORDS = new Set([
  'about', 'affects', 'and', 'answer', 'earlier', 'for', 'from', 'handle', 'incident',
  'project', 'should', 'source', 'sources', 'support', 'that', 'the', 'this', 'what',
  'which', 'who', 'why', 'with',
]);

export function searchNeedle(question: string): string {
  const terms = question.toLocaleLowerCase().match(/[a-z0-9][a-z0-9-]{2,}/g) ?? [];
  return terms.find((term) => !STOP_WORDS.has(term)) ?? '';
}

/** Converts graph records into a grounded answer and the exact paths that support it. */
export function composeCompanyAnswer(question: string, rows: CompanyBrainRow[]): CompanyBrainAnswer | null {
  if (rows.length === 0) return null;
  const first = rows[0];
  const intent = classifyCompanyQuestion(question);
  const facts = {
    ticketId: text(first.ticket_id),
    ticketTitle: text(first.ticket_title),
    projectId: text(first.project_id),
    projectName: text(first.project_name),
    ownerId: text(first.owner_id),
    ownerName: text(first.owner_name),
    ownerRole: text(first.owner_role),
    teamName: text(first.team_name),
    decisionId: text(first.decision_id),
    decisionTitle: text(first.decision_title),
    decisionSummary: text(first.decision_summary),
  };

  const uniqueSources = new Map<string, CompanyBrainSource>();
  for (const row of rows) {
    const id = text(row.source_id);
    if (!id || uniqueSources.has(id)) continue;
    uniqueSources.set(id, {
      id,
      title: text(row.source_title),
      kind: text(row.source_kind),
      uri: text(row.source_uri),
      updatedAt: Number(row.source_updated_at) || undefined,
    });
  }

  const ownerSentence = `${facts.ownerName}, ${facts.ownerRole} on ${facts.teamName}, owns ${facts.projectName} and should handle ${facts.ticketId}.`;
  const decisionSentence = `${facts.decisionId} (${facts.decisionTitle}) affects that project: ${facts.decisionSummary}`;
  const answer = intent === 'ownership'
    ? `${ownerSentence} ${decisionSentence}`
    : intent === 'decision'
      ? `${decisionSentence} The accountable owner is ${facts.ownerName}.`
      : `${facts.ticketTitle} belongs to ${facts.projectName}. ${ownerSentence} ${decisionSentence}`;

  return {
    answer,
    intent,
    action: facts.ticketId && facts.ownerId ? {
      kind: 'assign_ticket',
      label: `Assign ${facts.ticketId} to ${facts.ownerName}`,
      ticketId: facts.ticketId,
      assigneeId: facts.ownerId,
    } : null,
    paths: [
      [facts.ticketId, 'BELONGS_TO', facts.projectName, 'OWNED_BY', facts.ownerName],
      [facts.decisionId, 'AFFECTS', facts.projectName],
      ...Array.from(uniqueSources.values()).map((source) => [facts.decisionId, 'SUPPORTED_BY', source.title]),
    ],
    sources: Array.from(uniqueSources.values()),
    facts,
  };
}
