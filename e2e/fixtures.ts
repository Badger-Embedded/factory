import { test as base, expect } from '@playwright/test';
import { ADMIN, Factory, startFactory } from './server';

export { expect };
export * from './server';

let counter = 0;

export const test = base.extend<
  { project: string },
  { factory: Factory }
>({
  // One server and database per worker, so workers never share state.
  factory: [
    async ({}, use) => {
      const { factory, stop } = await startFactory();
      await use(factory);
      await stop();
    },
    { scope: 'worker' },
  ],
  baseURL: async ({ factory }, use) => {
    await use(factory.url);
  },
  extraHTTPHeaders: async ({}, use) => {
    await use({ 'Tailscale-User-Login': ADMIN });
  },
  // A project of its own, so a test can change state without touching others.
  project: async ({ factory }, use, testInfo) => {
    const name = `p${testInfo.workerIndex}-${testInfo.parallelIndex}-${counter++}`;
    factory.fixture('project', '--name', name, '--login', ADMIN);
    await use(name);
  },
});
