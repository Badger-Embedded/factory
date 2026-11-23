import { ChildProcess, execFileSync, spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';

export const repoRoot = path.resolve(__dirname, '..');
const target = path.join(repoRoot, 'target', 'debug');

export const ADMIN = 'admin@e2e.test';
export const COLLABORATOR = 'alice@e2e.test';
export const OUTSIDER = 'nobody@e2e.test';
export const PHONE_USER = 'talhahavadar.phone@e2e.test';
export const SHARED_PROJECT = 'maestro';

export interface Factory {
  url: string;
  db: string;
  /** Holds `ca.pem`, `cert.pem` and `key.pem`; the server trusts `ca.pem` for MCP servers. */
  tls: string;
  /** Runs `e2e_fixture` against the server's database and returns its output. */
  fixture(...args: string[]): string;
}

// Port 0 asks the kernel for a free port, so a dashboard on 8080 or 8081 never collides.
function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.once('error', reject);
    probe.listen(0, '127.0.0.1', () => {
      const { port } = probe.address() as net.AddressInfo;
      probe.close(() => resolve(port));
    });
  });
}

async function waitForServer(url: string, child: ChildProcess): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (child.exitCode !== null) throw new Error(`factory-server exited with ${child.exitCode}`);
    try {
      const response = await fetch(url);
      if (response.ok) return;
    } catch {
      // The server is not listening yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`factory-server did not answer on ${url}`);
}

/** Seeds a fresh database and starts the real server on free ports. */
export async function startFactory(): Promise<{ factory: Factory; stop(): Promise<void> }> {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'factory-e2e-'));
  const db = path.join(dir, 'factory.db');
  const fixture = (...args: string[]) =>
    execFileSync(path.join(target, 'examples', 'e2e_fixture'), [...args, '--db', db], {
      encoding: 'utf8',
    }).trim();
  const tls = path.join(dir, 'tls');
  try {
    fixture('seed');
    fixture('tls', '--dir', tls);
  } catch (error) {
    fs.rmSync(dir, { recursive: true, force: true });
    throw error;
  }

  const port = await freePort();
  const proxyPort = await freePort();
  const imports = path.join(dir, 'projects');
  fs.mkdirSync(imports);
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    FACTORY_LISTEN: `127.0.0.1:${port}`,
    FACTORY_PROXY_LISTEN: `127.0.0.1:${proxyPort}`,
    FACTORY_DATA: path.join(dir, 'data'),
    FACTORY_DB: db,
    FACTORY_IMPORT_PROJECTS: imports,
    FACTORY_ADMINS: ADMIN,
    FACTORY_MASTER_KEY: randomBytes(32).toString('hex'),
    FACTORY_GH: path.join(__dirname, 'fakes', 'gh'),
    FACTORY_GLAB: path.join(__dirname, 'fakes', 'glab'),
    FACTORY_PODMAN: path.join(__dirname, 'fakes', 'podman'),
    FACTORY_MCP_CA_FILE: path.join(tls, 'ca.pem'),
  };
  delete env.FACTORY_WORKERS;
  delete env.GH_TOKEN;
  const child = spawn(path.join(target, 'factory-server'), [], {
    env,
    cwd: dir,
    stdio: ['ignore', 'ignore', 'inherit'],
    detached: true,
  });
  // Registered at spawn, so an exit before cleanup is not missed.
  const exited = new Promise<void>((resolve) => child.once('exit', () => resolve()));
  // The server leads its own process group, so one kill also reaches a fake login
  // that no test cancelled, including one that starts as the server stops.
  const cleanup = async () => {
    try {
      process.kill(-child.pid!, 'SIGKILL');
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ESRCH') throw error;
    }
    await exited;
    fs.rmSync(dir, { recursive: true, force: true });
  };
  const url = `http://127.0.0.1:${port}`;
  try {
    await waitForServer(`${url}/`, child);
  } catch (error) {
    await cleanup();
    throw error;
  }
  return { factory: { url, db, tls, fixture }, stop: cleanup };
}
