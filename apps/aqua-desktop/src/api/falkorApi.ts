import { requireJson } from './jsonResponse.ts';

export interface FalkorStatus {
  connected: boolean;
  host: string;
  port: number;
  username: string;
  graphName: string;
  nodeCount: number;
  edgeCount: number;
  latencyMs: number;
  error?: string;
}

export interface GraphNode {
  id: string;
  labels: string[];
  properties: Record<string, any>;
}

export interface GraphEdge {
  source: string;
  target: string;
  label: string;
}

export interface GraphSnapshot {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

export interface FalkorQueryResult {
  success: boolean;
  data: any[];
  metadata?: any[];
  statistics?: Record<string, any>;
  error?: string;
}

export interface CompanyBrainSource {
  id: string;
  title: string;
  kind: string;
  uri: string;
  updatedAt?: number;
}

export interface CompanyBrainAction {
  kind: 'assign_ticket';
  label: string;
  ticketId: string;
  assigneeId: string;
}

export interface CompanyBrainAnswer {
  success: boolean;
  answer: string;
  intent: 'ownership' | 'decision' | 'project_context';
  action: CompanyBrainAction | null;
  paths: string[][];
  sources: CompanyBrainSource[];
  facts: Record<string, string>;
  interactionId: string;
  answerId: string;
  sessionId: string;
  graphRequired: true;
  error?: string;
}

export interface CompanyMemorySnapshot {
  sessionId: string;
  questions: Array<{
    questionId: string;
    question: string;
    answerId: string;
    answer: string;
    sourceIds: string[];
  }>;
  actions: Array<{
    actionId: string;
    kind: string;
    status: string;
    targetId: string;
  }>;
  handoffs: Array<{
    handoffId: string;
    fromAgent: string;
    toAgent: string;
    summary: string;
  }>;
}

export const DEMO_SESSION_ID = 'session_demo_primary';

const getFalkorUrl = (endpoint: string): string => {
  // Development keeps the Vite bridge for fast iteration. Installed builds use the authenticated
  // Aqua Core route: an installed application has no Vite server listening on port 1420.
  if (typeof window !== 'undefined' && window.location.port !== '1420') {
    return endpoint.replace('/api/falkordb', '/api/v1/falkordb');
  }
  return endpoint;
};

export const falkorApi = {
  async status(): Promise<FalkorStatus> {
    const res = await fetch(getFalkorUrl('/api/falkordb/status'), {
      headers: { Accept: 'application/json' },
    });
    if (!res.ok) {
      throw new Error(`FalkorDB status failed with ${res.status}`);
    }
    return requireJson<FalkorStatus>(res, `FalkorDB status failed (${res.status}).`);
  },

  async query(cypher: string, params?: Record<string, any>): Promise<FalkorQueryResult> {
    const res = await fetch(getFalkorUrl('/api/falkordb/query'), {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Accept: 'application/json',
      },
      body: JSON.stringify({ cypher, params }),
    });
    return requireJson<FalkorQueryResult>(res, `FalkorDB query failed (${res.status}).`);
  },

  async getSnapshot(): Promise<GraphSnapshot> {
    const res = await fetch(getFalkorUrl('/api/falkordb/snapshot'), {
      headers: { Accept: 'application/json' },
    });
    if (!res.ok) {
      throw new Error(`Failed to fetch graph snapshot: ${res.status}`);
    }
    return requireJson<GraphSnapshot>(res, `Could not load the graph snapshot (${res.status}).`);
  },

  async initSchema(): Promise<{ success: boolean; logs: string[] }> {
    const res = await fetch(getFalkorUrl('/api/falkordb/init'), {
      method: 'POST',
      headers: { Accept: 'application/json' },
    });
    return requireJson<{ success: boolean; logs: string[] }>(res, `Could not initialize the graph schema (${res.status}).`);
  },

  async seedDemoData(): Promise<{ success: boolean; message: string; statistics: any }> {
    const res = await fetch(getFalkorUrl('/api/falkordb/seed'), {
      method: 'POST',
      headers: { Accept: 'application/json' },
    });
    return requireJson<{ success: boolean; message: string; statistics: any }>(res, `Could not prepare the demo graph (${res.status}).`);
  },

  async updateConfig(config: { host: string; port: number; username?: string; password?: string; graphName: string; tls?: boolean; allowInsecureRemote?: boolean }) {
    const res = await fetch(getFalkorUrl('/api/falkordb/config'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify(config),
    });
    return requireJson(res, `Could not save the FalkorDB configuration (${res.status}).`);
  },

  async seedCompanyBrain(): Promise<{ success: boolean; message?: string; error?: string }> {
    const res = await fetch(getFalkorUrl('/api/falkordb/company/seed'), {
      method: 'POST',
      headers: { Accept: 'application/json' },
    });
    return requireJson<{ success: boolean; message?: string; error?: string }>(res, `Company Brain preparation failed (${res.status}).`);
  },

  async askCompanyBrain(question: string, tenantId = 'demo', sessionId = DEMO_SESSION_ID): Promise<CompanyBrainAnswer> {
    const res = await fetch(getFalkorUrl('/api/falkordb/company/ask'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ question, tenantId, sessionId }),
    });
    return requireJson<CompanyBrainAnswer>(res, `Company Brain query failed (${res.status}).`);
  },

  async executeCompanyAction(action: CompanyBrainAction, tenantId = 'demo', sessionId = DEMO_SESSION_ID): Promise<{ success: boolean; message: string; actionId: string }> {
    const res = await fetch(getFalkorUrl('/api/falkordb/company/action'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ ...action, tenantId, sessionId }),
    });
    return requireJson<{ success: boolean; message: string; actionId: string }>(res, `Company Brain action failed (${res.status}).`);
  },

  async recallCompanyMemory(tenantId = 'demo', sessionId = DEMO_SESSION_ID): Promise<CompanyMemorySnapshot> {
    const res = await fetch(getFalkorUrl('/api/falkordb/company/memory'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ tenantId, sessionId }),
    });
    return requireJson<CompanyMemorySnapshot>(res, `Company memory recall failed (${res.status}).`);
  },

  async createCompanyHandoff(summary: string, tenantId = 'demo', sessionId = DEMO_SESSION_ID): Promise<{ success: boolean; handoffId: string; message: string }> {
    const res = await fetch(getFalkorUrl('/api/falkordb/company/handoff'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ summary, tenantId, sessionId }),
    });
    return requireJson<{ success: boolean; handoffId: string; message: string }>(res, `Company handoff failed (${res.status}).`);
  },

  /** Associative Memory Recall: retrieves relevant past experiences for the current intent */
  async recallExperiences(category: string): Promise<any[]> {
    const cypher = `
      MATCH (e:Experience)-[:USES]->(t:Tool)
      WHERE e.category = $category AND e.confidence >= 0.7
      RETURN e.id AS id, e.summary AS summary, e.solution AS solution, e.confidence AS confidence, collect(t.name) AS tools
      ORDER BY e.confidence DESC
      LIMIT 5
    `;
    const result = await this.query(cypher, { category });
    return result.data || [];
  },

  /** Forges a new verified experience into the FalkorDB Graph */
  async recordExperience(taskId: string, prompt: string, category: string, solution: string, tools: string[]) {
    const cypher = `
      MERGE (t:Task {id: $taskId})
      ON CREATE SET t.prompt = $prompt, t.category = $category, t.timestamp = timestamp()

      CREATE (e:Experience {
        id: 'exp_' + toString(timestamp()),
        summary: $prompt,
        solution: $solution,
        category: $category,
        outcome: 'success',
        confidence: 1.0,
        created_at: timestamp()
      })
      CREATE (t)-[:RECALLS]->(e)

      WITH e
      UNWIND $tools AS toolName
      MERGE (tool:Tool {name: toolName})
      CREATE (e)-[:USES]->(tool)
      RETURN e.id AS experience_id
    `;
    return this.query(cypher, { taskId, prompt, category, solution, tools });
  },

  /** Traverses multi-step skill dependencies to discover security/approval requirements */
  async validateSkillTools(skillName: string): Promise<any[]> {
    const cypher = `
      MATCH (s:Skill {name: $skillName})-[:DEPENDS_ON*1..3]->(t:Tool)
      RETURN t.name AS tool_name, t.permission_level AS permission_level, t.domain AS domain
    `;
    const result = await this.query(cypher, { skillName });
    return result.data || [];
  },
};
