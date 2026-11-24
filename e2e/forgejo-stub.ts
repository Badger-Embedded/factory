import fs from 'node:fs';
import https from 'node:https';
import type { IncomingMessage, ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import type { Factory } from './server';

/** What a route answers with. `body` is sent as JSON unless it is a string. */
export interface StubReply {
  status?: number;
  body?: unknown;
}

export type StubRoute = (request: IncomingMessage, url: URL) => StubReply | undefined;

export interface ForgejoStub {
  /** The base URL an admin registers, on 127.0.0.1 over TLS. */
  url: string;
  /** Replaces the text `/api/v1/version` answers with. */
  setVersion(version: string): void;
  /**
   * Adds a route tried before the built-in ones. It returns a reply for the
   * requests it handles and `undefined` for the rest, so a test can answer per
   * repository or per token, which the `Authorization` header carries.
   */
  route(route: StubRoute): void;
  /** The `method path` of each request the stub received, newest last. */
  seen(): string[];
  stop(): Promise<void>;
}

/** A version string as Forgejo writes it, which Gitea does not. */
export const FORGEJO_VERSION = '17.0.2+gitea-1.22.0';

/**
 * A stub Forgejo server over TLS with the certificate `e2e_fixture tls` wrote
 * for the factory, which the factory trusts through `FACTORY_FORGEJO_CA_FILE`.
 * It answers `/api/v1/version` and whatever a test adds with `route`.
 */
export async function startForgejoStub(factory: Pick<Factory, 'tls'>): Promise<ForgejoStub> {
  let version = FORGEJO_VERSION;
  const routes: StubRoute[] = [];
  const requests: string[] = [];
  const send = (response: ServerResponse, reply: StubReply) => {
    const body = reply.body ?? {};
    response.writeHead(reply.status ?? 200, {
      'content-type': typeof body === 'string' ? 'text/plain' : 'application/json',
    });
    response.end(typeof body === 'string' ? body : JSON.stringify(body));
  };
  const server = https.createServer(
    {
      cert: fs.readFileSync(path.join(factory.tls, 'cert.pem')),
      key: fs.readFileSync(path.join(factory.tls, 'key.pem')),
    },
    (request, response) => {
      const url = new URL(request.url ?? '/', 'https://stub');
      requests.push(`${request.method} ${url.pathname}`);
      for (const route of [...routes].reverse()) {
        const reply = route(request, url);
        if (reply) {
          send(response, reply);
          return;
        }
      }
      if (url.pathname.endsWith('/api/v1/version')) {
        send(response, { body: { version } });
      } else {
        send(response, { status: 404, body: { message: 'not found' } });
      }
    },
  );
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address() as AddressInfo;
  return {
    url: `https://127.0.0.1:${port}`,
    setVersion: (next) => {
      version = next;
    },
    route: (route) => {
      routes.push(route);
    },
    seen: () => [...requests],
    stop: () =>
      new Promise<void>((resolve) => {
        server.closeAllConnections();
        server.close(() => resolve());
      }),
  };
}
