import type { Plugin } from 'vite';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { FalkorDB } from 'falkordb';
import { COMPANY_CONTEXT_QUERY, composeCompanyAnswer, searchNeedle, type CompanyBrainRow } from './companyBrain.ts';

export interface FalkorConfig {
  host: string;
  port: number;
  username: string;
  password: string;
  graphName: string;
  tls: boolean;
  allowInsecureRemote: boolean;
}

let activeConfig: FalkorConfig = {
  host: process.env.FALKORDB_HOST || '127.0.0.1',
  port: Number(process.env.FALKORDB_PORT) || 6379,
  username: process.env.FALKORDB_USER || '',
  password: process.env.FALKORDB_PASSWORD || '',
  graphName: process.env.FALKORDB_GRAPH || 'AquaCompanyBrain',
  tls: process.env.FALKORDB_TLS === 'true',
  allowInsecureRemote: process.env.FALKORDB_ALLOW_INSECURE_REMOTE === 'true',
};

let dbClient: any = null;
let graphInstance: any = null;
let lastError: string | null = null;

async function getGraph() {
  if (graphInstance && dbClient) return graphInstance;
  try {
    lastError = null;
    const local = ['127.0.0.1', 'localhost', '::1', '[::1]'].includes(activeConfig.host.toLowerCase());
    if (!local && !activeConfig.tls && !activeConfig.allowInsecureRemote) {
      throw new Error('Remote FalkorDB connections require TLS unless FALKORDB_ALLOW_INSECURE_REMOTE=true is explicitly set.');
    }
    dbClient = await FalkorDB.connect({
      username: activeConfig.username,
      password: activeConfig.password,
      socket: {
        host: activeConfig.host,
        port: activeConfig.port,
        tls: activeConfig.tls,
        connectTimeout: 4000,
      },
    });

    if (dbClient) {
      dbClient.on('error', (err: any) => {
        console.warn('[falkordb-bridge] Handled socket error:', err?.message || err);
        dbClient = null;
        graphInstance = null;
      });
    }

    graphInstance = dbClient.selectGraph(activeConfig.graphName);
    return graphInstance;
  } catch (err: any) {
    lastError = err?.message || String(err);
    dbClient = null;
    graphInstance = null;
    throw err;
  }
}

async function readBody(req: IncomingMessage): Promise<any> {
  return new Promise((resolve, reject) => {
    let body = '';
    let tooLarge = false;
    req.on('data', (chunk) => {
      if (tooLarge) return;
      body += chunk;
      if (Buffer.byteLength(body) > 64 * 1024) {
        tooLarge = true;
        body = '';
      }
    });
    req.on('end', () => {
      if (tooLarge) return reject(new Error('Request body exceeds 64 KiB.'));
      try {
        resolve(body ? JSON.parse(body) : {});
      } catch {
        reject(new Error('Request body must be valid JSON.'));
      }
    });
  });
}

const QUERY_TIMEOUT_MS = 15_000;
async function queryGraph(graph: any, cypher: string, options?: any): Promise<any> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      graph.query(cypher, options),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`FalkorDB query exceeded ${QUERY_TIMEOUT_MS} ms.`)), QUERY_TIMEOUT_MS); }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

function sendJson(res: ServerResponse, data: any, status = 200) {
  res.statusCode = status;
  res.setHeader('Content-Type', 'application/json');
  res.end(JSON.stringify(data));
}

const DESKTOP_ORIGINS = new Set(['http://127.0.0.1:1420', 'http://localhost:1420']);

/** Return the exact trusted browser origin, or null when there is no trusted Origin header. */
export function trustedDesktopOrigin(req: Pick<IncomingMessage, 'headers'>): string | null {
  const origin = req.headers.origin;
  return typeof origin === 'string' && DESKTOP_ORIGINS.has(origin) ? origin : null;
}

function allowedOrigin(req: IncomingMessage): boolean {
  const origin = req.headers.origin;
  if (typeof origin === 'string' && origin) return trustedDesktopOrigin(req) !== null;
  return req.headers['sec-fetch-site'] === 'same-origin' || req.headers['sec-fetch-site'] === 'none';
}

function applyCorsHeaders(req: IncomingMessage, res: ServerResponse): void {
  const origin = trustedDesktopOrigin(req);
  if (!origin) return;
  res.setHeader('Access-Control-Allow-Origin', origin);
  res.setHeader('Vary', 'Origin');
}

const requestWindows = new Map<string, { started: number; count: number }>();
function withinRateLimit(req: IncomingMessage): boolean {
  const key = req.socket.remoteAddress || 'local';
  const now = Date.now();
  const current = requestWindows.get(key);
  if (!current || now - current.started >= 60_000) {
    requestWindows.set(key, { started: now, count: 1 });
    return true;
  }
  current.count += 1;
  return current.count <= 120;
}

function queryRows(result: any): CompanyBrainRow[] {
  if (!Array.isArray(result?.data)) return [];
  return result.data.filter((row: unknown): row is CompanyBrainRow => Boolean(row) && typeof row === 'object' && !Array.isArray(row));
}

export function falkorPlugin(config: Partial<FalkorConfig> = {}): Plugin {
  activeConfig = {
    ...activeConfig,
    ...(config.host ? { host: config.host } : {}),
    ...(config.port ? { port: config.port } : {}),
    ...(config.username ? { username: config.username } : {}),
    ...(config.password ? { password: config.password } : {}),
    ...(config.graphName ? { graphName: config.graphName } : {}),
    ...(typeof config.tls === 'boolean' ? { tls: config.tls } : {}),
    ...(typeof config.allowInsecureRemote === 'boolean' ? { allowInsecureRemote: config.allowInsecureRemote } : {}),
  };
  return {
    name: 'aqua-falkordb-bridge',
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        const url = req.url?.split('?')[0] || '';
        if (!url.startsWith('/api/falkordb')) {
          return next();
        }

        if (!allowedOrigin(req)) {
          return sendJson(res, { error: 'This FalkorDB bridge only accepts requests from the Aqua desktop page.' }, 403);
        }
        applyCorsHeaders(req, res);
        if (!withinRateLimit(req)) {
          return sendJson(res, { error: 'Too many FalkorDB bridge requests; retry after one minute.' }, 429);
        }

        // Handle CORS preflight
        if (req.method === 'OPTIONS') {
          res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
          res.setHeader('Access-Control-Allow-Headers', 'Content-Type, Authorization');
          res.statusCode = 204;
          return res.end();
        }

        try {
          // Status endpoint
          if (url === '/api/falkordb/status' && req.method === 'GET') {
            const start = performance.now();
            try {
              const graph = await getGraph();
              const countResult = await queryGraph(graph, 'MATCH (n) RETURN count(n) AS node_count');
              const nodeCount = countResult.data?.[0]?.node_count ?? 0;

              const edgeResult = await queryGraph(graph, 'MATCH ()-[r]->() RETURN count(r) AS edge_count');
              const edgeCount = edgeResult.data?.[0]?.edge_count ?? 0;
              const latencyMs = Math.round(performance.now() - start);

              return sendJson(res, {
                connected: true,
                mode: 'falkordb_cloud',
                host: activeConfig.host,
                port: activeConfig.port,
                username: activeConfig.username,
                graphName: activeConfig.graphName,
                nodeCount: Number(nodeCount),
                edgeCount: Number(edgeCount),
                latencyMs,
              });
            } catch (err: any) {
              return sendJson(res, {
                connected: false,
                mode: 'offline',
                host: activeConfig.host,
                port: activeConfig.port,
                username: activeConfig.username,
                graphName: activeConfig.graphName,
                nodeCount: 0,
                edgeCount: 0,
                latencyMs: Math.round(performance.now() - start),
                error: err?.message || lastError || 'Could not connect to FalkorDB Cloud',
              });
            }
          }

          // Execute Cypher Query
          if (url === '/api/falkordb/query' && req.method === 'POST') {
            const body = await readBody(req);
            const { cypher, params } = body;
            if (!cypher || typeof cypher !== 'string') {
              return sendJson(res, { error: 'Missing cypher string in request' }, 400);
            }

            try {
              const graph = await getGraph();
              const result = await queryGraph(graph, cypher, { params });
              return sendJson(res, {
                success: true,
                data: result.data || [],
                metadata: result.metadata || [],
                statistics: result.statistics || {},
              });
            } catch (cloudErr: any) {
              return sendJson(res, {
                success: false,
                error: cloudErr?.message || 'FalkorDB query failed',
                notice: 'No fallback answer was used. Company Brain answers require FalkorDB.',
              }, 503);
            }
          }

          // Fetch graph snapshot for visual explorer
          if (url === '/api/falkordb/snapshot' && req.method === 'GET') {
            try {
              const graph = await getGraph();
              const result = await queryGraph(graph, `
                MATCH (n)
                OPTIONAL MATCH (n)-[r]->(m)
                RETURN n, type(r) AS rel_type, m
                LIMIT 150
              `);

              const nodeMap = new Map<string, any>();
              const edges: Array<{ source: string; target: string; label: string }> = [];

              for (const row of result.data || []) {
                if (row.n) {
                  const id = String(row.n.id ?? row.n.properties?.id ?? Math.random());
                  if (!nodeMap.has(id)) {
                    nodeMap.set(id, {
                      id,
                      labels: row.n.labels || [],
                      properties: row.n.properties || {},
                    });
                  }
                }
                if (row.m && row.rel_type) {
                  const targetId = String(row.m.id ?? row.m.properties?.id ?? Math.random());
                  if (!nodeMap.has(targetId)) {
                    nodeMap.set(targetId, {
                      id: targetId,
                      labels: row.m.labels || [],
                      properties: row.m.properties || {},
                    });
                  }
                  const sourceId = String(row.n.id ?? row.n.properties?.id);
                  edges.push({
                    source: sourceId,
                    target: targetId,
                    label: row.rel_type,
                  });
                }
              }

              return sendJson(res, {
                nodes: Array.from(nodeMap.values()),
                edges,
              });
            } catch (err: any) {
              return sendJson(res, {
                nodes: [],
                edges: [],
                error: err?.message || 'FalkorDB snapshot failed',
              }, 503);
            }
          }

          // Initialize Schema
          if (url === '/api/falkordb/init' && req.method === 'POST') {
            try {
              const graph = await getGraph();
              const statements = [
                'CREATE INDEX FOR (t:Task) ON (t.id)',
                'CREATE INDEX FOR (e:Experience) ON (e.id)',
                'CREATE INDEX FOR (tool:Tool) ON (tool.name)',
                'CREATE INDEX FOR (s:Skill) ON (s.name)',
              ];
              const logs: string[] = [];
              for (const stmt of statements) {
                try {
                  await queryGraph(graph, stmt);
                  logs.push(`Executed: ${stmt}`);
                } catch (e: any) {
                  logs.push(`Notice: ${stmt} (${e?.message || 'index already present'})`);
                }
              }
              return sendJson(res, { success: true, logs });
            } catch (err: any) {
              return sendJson(res, {
                success: false,
                logs: [],
                error: err?.message || 'FalkorDB schema initialization failed',
              }, 503);
            }
          }

          // Seed Rich Demo Data for Hackathon
          if (url === '/api/falkordb/seed' && req.method === 'POST') {
            try {
              const graph = await getGraph();
              const seedCypher = `
                CREATE (u:User {id: 'usr_owner', name: 'Primary Operator', role: 'Administrator'})

                CREATE (t1:Task {id: 'tsk_01', prompt: 'Audit system permissions and security', category: 'security', status: 'verified', created_at: timestamp() - 3600000})
                CREATE (t2:Task {id: 'tsk_02', prompt: 'Summarize weekly research notes and export', category: 'knowledge', status: 'verified', created_at: timestamp() - 1800000})
                CREATE (t3:Task {id: 'tsk_03', prompt: 'Execute automated desktop backup routine', category: 'automation', status: 'verified', created_at: timestamp() - 900000})

                CREATE (e1:Experience {id: 'exp_01', summary: 'Verify tool execution policy before run', solution: 'Check ToolsPanel policy; prompt if locked', confidence: 0.98, outcome: 'success'})
                CREATE (e2:Experience {id: 'exp_02', summary: 'Read document passages from vault index', solution: 'Perform semantic search on vault files', confidence: 0.95, outcome: 'success'})
                CREATE (e3:Experience {id: 'exp_03', summary: 'Orchestrate routine scheduling steps', solution: 'Queue workflow steps in sequence with retry', confidence: 0.92, outcome: 'success'})

                CREATE (tool_audit:Tool {name: 'tools.audit_log', permission_level: 'standard', domain: 'security'})
                CREATE (tool_vault:Tool {name: 'vault.search_files', permission_level: 'standard', domain: 'knowledge'})
                CREATE (tool_backup:Tool {name: 'flows.execute_routine', permission_level: 'always_ask', domain: 'automations'})

                CREATE (sk1:Skill {name: 'Daily Security Checkup', description: 'Performs comprehensive audit of system rules'})
                CREATE (sk2:Skill {name: 'Document Intelligence Summarizer', description: 'Summarizes vault documents into cohesive reports'})

                CREATE (u)-[:REQUESTS]->(t1)
                CREATE (u)-[:REQUESTS]->(t2)
                CREATE (u)-[:REQUESTS]->(t3)

                CREATE (t1)-[:RECALLS]->(e1)
                CREATE (t2)-[:RECALLS]->(e2)
                CREATE (t3)-[:RECALLS]->(e3)

                CREATE (e1)-[:USES]->(tool_audit)
                CREATE (e2)-[:USES]->(tool_vault)
                CREATE (e3)-[:USES]->(tool_backup)

                CREATE (e1)-[:FORGED_INTO]->(sk1)
                CREATE (e2)-[:FORGED_INTO]->(sk2)

                CREATE (sk1)-[:DEPENDS_ON]->(tool_audit)
                CREATE (sk2)-[:DEPENDS_ON]->(tool_vault)

                CREATE (e2)-[:DERIVED_FROM {similarity: 0.85}]->(e1)
              `;

              const seedRes = await queryGraph(graph, seedCypher);
              return sendJson(res, {
                success: true,
                message: 'FalkorDB agent memory graph seeded into FalkorDB Cloud!',
                statistics: seedRes.statistics,
              });
            } catch (err: any) {
              return sendJson(res, {
                success: false,
                message: 'FalkorDB is unavailable; no demo graph was substituted.',
                error: err?.message || 'FalkorDB seed failed',
              }, 503);
            }
          }

          // Company Brain demo dataset. MERGE makes this repeatable and does not erase other graphs.
          if (url === '/api/falkordb/company/seed' && req.method === 'POST') {
            try {
              const graph = await getGraph();
              const seed = `
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
              const result = await queryGraph(graph, seed);
              return sendJson(res, {
                success: true,
                message: 'Company Brain dataset is ready in FalkorDB.',
                data: result.data || [],
                statistics: result.statistics || {},
              });
            } catch (err: any) {
              return sendJson(res, {
                success: false,
                message: 'FalkorDB is unavailable; Company Brain withheld its answer instead of using ungrounded context.',
                error: err?.message || 'Company Brain seed failed',
              }, 503);
            }
          }

          // A graph-dependent agent answer. There is intentionally no local or model-only fallback.
          if (url === '/api/falkordb/company/ask' && req.method === 'POST') {
            const body = await readBody(req);
            const question = typeof body.question === 'string' ? body.question.trim() : '';
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : 'demo';
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : 'session_demo_primary';
            if (!question) return sendJson(res, { success: false, error: 'A question is required.' }, 400);

            try {
              const graph = await getGraph();
              let result = await queryGraph(graph, COMPANY_CONTEXT_QUERY, {
                params: { tenantId, needle: searchNeedle(question) },
              });
              let rows = queryRows(result);
              // A broad retry still uses FalkorDB; it helps natural questions whose wording is not an entity name.
              if (rows.length === 0) {
                result = await queryGraph(graph, COMPANY_CONTEXT_QUERY, { params: { tenantId, needle: '' } });
                rows = queryRows(result);
              }
              const grounded = composeCompanyAnswer(question, rows);
              if (!grounded) {
                return sendJson(res, {
                  success: false,
                  error: 'No connected company context was found for this question.',
                }, 404);
              }

              const interactionId = `question_${Date.now()}`;
              const answerId = `answer_${Date.now()}`;
              await queryGraph(graph, `
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

              return sendJson(res, {
                success: true,
                ...grounded,
                interactionId,
                answerId,
                sessionId,
                graphRequired: true,
              });
            } catch (err: any) {
              return sendJson(res, {
                success: false,
                error: err?.message || 'Company Brain query failed',
                graphRequired: true,
              }, 503);
            }
          }

          // Executes the action proposed from graph context and records an auditable graph event.
          if (url === '/api/falkordb/company/action' && req.method === 'POST') {
            const body = await readBody(req);
            if (body.kind !== 'assign_ticket' || !body.ticketId || !body.assigneeId) {
              return sendJson(res, { success: false, error: 'A valid assign_ticket action is required.' }, 400);
            }
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : 'demo';
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : 'session_demo_primary';
            try {
              const graph = await getGraph();
              const actionId = `action_${Date.now()}`;
              const result = await queryGraph(graph, `
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
                ticketId: String(body.ticketId),
                assigneeId: String(body.assigneeId),
                tenantId,
                actionId,
                outcomeId: `outcome_${Date.now()}`,
                sessionId,
              } });
              const rows = result.data || [];
              if (rows.length === 0) {
                return sendJson(res, { success: false, error: 'The graph did not validate this assignment.' }, 409);
              }
              return sendJson(res, {
                success: true,
                message: `${body.ticketId} was assigned using the ownership path in FalkorDB.`,
                data: rows,
                actionId,
              });
            } catch (err: any) {
              return sendJson(res, { success: false, error: err?.message || 'Company Brain action failed' }, 503);
            }
          }

          if (url === '/api/falkordb/company/memory' && req.method === 'POST') {
            const body = await readBody(req);
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : 'demo';
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : 'session_demo_primary';
            try {
              const graph = await getGraph();
              const questions = await queryGraph(graph, `
                MATCH (session:Session {id: $sessionId, tenant_id: $tenantId})-[:CONTAINS]->(q:Question)
                MATCH (q)-[:ANSWERED_BY]->(a:Answer)
                OPTIONAL MATCH (a)-[:GROUNDED_IN]->(source:Source)
                RETURN q.id AS question_id, q.text AS question, a.id AS answer_id,
                  a.text AS answer, collect(DISTINCT source.id) AS source_ids,
                  q.answered_at AS answered_at
                ORDER BY answered_at DESC LIMIT 10
              `, { params: { tenantId, sessionId } });
              const actions = await queryGraph(graph, `
                MATCH (session:Session {id: $sessionId, tenant_id: $tenantId})-[:CONTAINS]->(action:Action)
                OPTIONAL MATCH (action)-[:ACTED_ON]->(target)
                RETURN action.id AS action_id, action.kind AS kind, action.status AS status,
                  target.id AS target_id, action.created_at AS created_at
                ORDER BY created_at DESC LIMIT 10
              `, { params: { tenantId, sessionId } });
              const handoffs = await queryGraph(graph, `
                MATCH (session:Session {id: $sessionId, tenant_id: $tenantId})-[:CONTAINS]->(handoff:Handoff)
                MATCH (from:Agent)-[:HANDED_OFF]->(handoff)-[:TO_AGENT]->(to:Agent)
                RETURN handoff.id AS handoff_id, from.name AS from_agent,
                  to.name AS to_agent, handoff.summary AS summary,
                  handoff.created_at AS created_at
                ORDER BY created_at DESC LIMIT 10
              `, { params: { tenantId, sessionId } });
              return sendJson(res, {
                success: true,
                sessionId,
                questions: (questions.data || []).map((row: any) => ({ questionId: row.question_id, question: row.question, answerId: row.answer_id, answer: row.answer, sourceIds: row.source_ids || [] })),
                actions: (actions.data || []).map((row: any) => ({ actionId: row.action_id, kind: row.kind, status: row.status, targetId: row.target_id })),
                handoffs: (handoffs.data || []).map((row: any) => ({ handoffId: row.handoff_id, fromAgent: row.from_agent, toAgent: row.to_agent, summary: row.summary })),
              });
            } catch (err: any) {
              return sendJson(res, { success: false, error: err?.message || 'Company memory recall failed' }, 503);
            }
          }

          if (url === '/api/falkordb/company/handoff' && req.method === 'POST') {
            const body = await readBody(req);
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : 'demo';
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : 'session_demo_primary';
            const summary = typeof body.summary === 'string' ? body.summary.trim() : '';
            if (!summary) return sendJson(res, { success: false, error: 'A handoff summary is required.' }, 400);
            try {
              const graph = await getGraph();
              const handoffId = `handoff_${Date.now()}`;
              await queryGraph(graph, `
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
              return sendJson(res, { success: true, handoffId, message: 'Handoff persisted for the Incident Response Agent.' });
            } catch (err: any) {
              return sendJson(res, { success: false, error: err?.message || 'Company handoff failed' }, 503);
            }
          }

          // Update Configuration
          if (url === '/api/falkordb/config' && req.method === 'POST') {
            const body = await readBody(req);
            if (body.host) activeConfig.host = String(body.host).trim();
            if (body.port) activeConfig.port = Number(body.port);
            if (body.username) activeConfig.username = String(body.username).trim();
            if (body.password) activeConfig.password = String(body.password).trim();
            if (body.graphName) activeConfig.graphName = String(body.graphName).trim();

            // Reset client to reconnect
            if (dbClient) {
              try { await dbClient.close(); } catch { /* ignore */ }
              dbClient = null;
              graphInstance = null;
            }

            return sendJson(res, {
              success: true,
              config: {
                host: activeConfig.host,
                port: activeConfig.port,
                username: activeConfig.username,
                graphName: activeConfig.graphName,
              },
            });
          }

          return sendJson(res, { error: 'Unknown FalkorDB endpoint' }, 404);
        } catch (err: any) {
          return sendJson(res, {
            error: err?.message || String(err),
          }, 500);
        }
      });
    },
  };
}
