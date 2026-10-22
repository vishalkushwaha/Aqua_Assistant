import type { Plugin } from 'vite';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { FalkorDB } from 'falkordb';
import {
  CompanyBrainRepository,
  DEFAULT_COMPANY_SESSION_ID,
  DEFAULT_COMPANY_TENANT_ID,
} from './companyBrainRepository.ts';

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

const companyBrain = new CompanyBrainRepository(getGraph, queryGraph);

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
              const result = await companyBrain.seed();
              return sendJson(res, {
                success: true,
                message: 'Company Brain dataset is ready in FalkorDB.',
                data: result.data,
                statistics: result.statistics,
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
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : DEFAULT_COMPANY_TENANT_ID;
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : DEFAULT_COMPANY_SESSION_ID;
            if (!question) return sendJson(res, { success: false, error: 'A question is required.' }, 400);

            try {
              const grounded = await companyBrain.ask(question, tenantId, sessionId);
              if (!grounded) {
                return sendJson(res, {
                  success: false,
                  error: 'No connected company context was found for this question.',
                }, 404);
              }

              return sendJson(res, {
                success: true,
                ...grounded,
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
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : DEFAULT_COMPANY_TENANT_ID;
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : DEFAULT_COMPANY_SESSION_ID;
            try {
              const result = await companyBrain.assignTicket(String(body.ticketId), String(body.assigneeId), tenantId, sessionId);
              if (!result) {
                return sendJson(res, { success: false, error: 'The graph did not validate this assignment.' }, 409);
              }
              return sendJson(res, {
                success: true,
                message: `${body.ticketId} was assigned using the ownership path in FalkorDB.`,
                data: result.data,
                actionId: result.actionId,
              });
            } catch (err: any) {
              return sendJson(res, { success: false, error: err?.message || 'Company Brain action failed' }, 503);
            }
          }

          if (url === '/api/falkordb/company/memory' && req.method === 'POST') {
            const body = await readBody(req);
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : DEFAULT_COMPANY_TENANT_ID;
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : DEFAULT_COMPANY_SESSION_ID;
            try {
              const memory = await companyBrain.recallMemory(tenantId, sessionId);
              return sendJson(res, {
                success: true,
                ...memory,
              });
            } catch (err: any) {
              return sendJson(res, { success: false, error: err?.message || 'Company memory recall failed' }, 503);
            }
          }

          if (url === '/api/falkordb/company/handoff' && req.method === 'POST') {
            const body = await readBody(req);
            const tenantId = typeof body.tenantId === 'string' && body.tenantId.trim() ? body.tenantId.trim() : DEFAULT_COMPANY_TENANT_ID;
            const sessionId = typeof body.sessionId === 'string' && body.sessionId.trim() ? body.sessionId.trim() : DEFAULT_COMPANY_SESSION_ID;
            const summary = typeof body.summary === 'string' ? body.summary.trim() : '';
            if (!summary) return sendJson(res, { success: false, error: 'A handoff summary is required.' }, 400);
            try {
              const handoffId = await companyBrain.createHandoff(tenantId, sessionId, summary);
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
