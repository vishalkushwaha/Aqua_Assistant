import { FalkorDB } from 'falkordb';
import { CompanyBrainRepository, type GraphQueryResult } from '../companyBrainRepository.ts';

const QUERY_TIMEOUT_MS = 15_000;

export interface SwitchCompanyBrainDatabase {
  repository: CompanyBrainRepository;
  close: () => Promise<void>;
}

export function createSwitchCompanyBrainDatabase(): SwitchCompanyBrainDatabase {
  const config = {
    host: process.env.FALKORDB_HOST?.trim() || '127.0.0.1',
    port: Number(process.env.FALKORDB_PORT) || 6379,
    username: process.env.FALKORDB_USER?.trim() || '',
    password: process.env.FALKORDB_PASSWORD || '',
    graphName: process.env.FALKORDB_GRAPH?.trim() || 'AquaCompanyBrain',
    tls: process.env.FALKORDB_TLS === 'true',
    allowInsecureRemote: process.env.FALKORDB_ALLOW_INSECURE_REMOTE === 'true',
  };

  const local = ['127.0.0.1', 'localhost', '::1', '[::1]'].includes(config.host.toLowerCase());
  if (!local && !config.tls && !config.allowInsecureRemote) {
    throw new Error('Remote FalkorDB connections require TLS unless FALKORDB_ALLOW_INSECURE_REMOTE=true is explicitly set.');
  }

  let client: any = null;
  let graph: any = null;

  const getGraph = async () => {
    if (client && graph) return graph;
    client = await FalkorDB.connect({
      username: config.username,
      password: config.password,
      socket: {
        host: config.host,
        port: config.port,
        tls: config.tls,
        connectTimeout: 4000,
      },
    });
    client.on('error', (error: unknown) => {
      console.warn('[switch-company-brain] FalkorDB connection failed:', error instanceof Error ? error.message : String(error));
      client = null;
      graph = null;
    });
    graph = client.selectGraph(config.graphName);
    return graph;
  };

  const query = async (selectedGraph: any, cypher: string, options?: { params?: Record<string, unknown> }): Promise<GraphQueryResult> => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        selectedGraph.query(cypher, options),
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new Error(`FalkorDB query exceeded ${QUERY_TIMEOUT_MS} ms.`)), QUERY_TIMEOUT_MS);
        }),
      ]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  };

  return {
    repository: new CompanyBrainRepository(getGraph, query),
    close: async () => {
      const openClient = client;
      client = null;
      graph = null;
      if (openClient) await openClient.close();
    },
  };
}
