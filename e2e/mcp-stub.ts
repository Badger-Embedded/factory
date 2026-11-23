import fs from 'node:fs';
import https from 'node:https';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import type { Factory } from './server';

export const STUB_TOOLS = [
  { name: 'read_file', description: 'Reads a file' },
  { name: 'write_file', description: 'Writes a file' },
  { name: 'list_dir', description: 'Lists a directory' },
  { name: 'search_docs', description: 'Searches the docs' },
  { name: 'fetch_page', description: 'Fetches a web page' },
];

export interface McpStub {
  /** The URL an admin registers, on 127.0.0.1 over TLS. */
  url: string;
  /** The `name` header of each request the stub received, newest last. */
  seen(header: string): string[];
  /** Replaces the tools `tools/list` answers with. */
  setTools(tools: { name: string; description?: string }[]): void;
  stop(): Promise<void>;
}

/**
 * A stub MCP server over TLS with the certificate `e2e_fixture tls` wrote for
 * the factory. It answers `initialize` with JSON and a session id, `tools/list`
 * as an SSE event, and the closing DELETE, which is what the factory's
 * connection check sends and accepts.
 */
export async function startMcpStub(factory: Pick<Factory, 'tls'>): Promise<McpStub> {
  let tools: { name: string; description?: string }[] = STUB_TOOLS;
  const requests: Record<string, string>[] = [];
  const server = https.createServer(
    {
      cert: fs.readFileSync(path.join(factory.tls, 'cert.pem')),
      key: fs.readFileSync(path.join(factory.tls, 'key.pem')),
    },
    (request, response) => {
      const chunks: Buffer[] = [];
      request.on('data', (chunk) => chunks.push(chunk));
      request.on('end', () => {
        requests.push(
          Object.fromEntries(
            Object.entries(request.headers).map(([key, value]) => [key, String(value)]),
          ),
        );
        if (request.method === 'DELETE') {
          response.writeHead(200).end();
          return;
        }
        const body = JSON.parse(Buffer.concat(chunks).toString() || '{}');
        if (body.method === 'initialize') {
          response.writeHead(200, {
            'content-type': 'application/json',
            'mcp-session-id': 'stub-session',
          });
          response.end(
            JSON.stringify({
              jsonrpc: '2.0',
              id: body.id,
              result: {
                protocolVersion: '2025-03-26',
                capabilities: { tools: {} },
                serverInfo: { name: 'e2e-stub', version: '1.2.3' },
              },
            }),
          );
        } else if (body.method === 'tools/list') {
          const reply = { jsonrpc: '2.0', id: body.id, result: { tools } };
          response.writeHead(200, { 'content-type': 'text/event-stream' });
          response.end(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
        } else {
          response.writeHead(202).end();
        }
      });
    },
  );
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address() as AddressInfo;
  return {
    url: `https://127.0.0.1:${port}/mcp`,
    seen: (header) => requests.map((headers) => headers[header.toLowerCase()]).filter(Boolean),
    setTools: (next) => {
      tools = next;
    },
    stop: () =>
      new Promise<void>((resolve) => {
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
