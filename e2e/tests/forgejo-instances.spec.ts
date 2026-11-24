import type { Page } from '@playwright/test';
import { ADMIN, COLLABORATOR, expect, startFactory, test as base, type Factory } from '../fixtures';
import { startForgejoStub, type ForgejoStub } from '../forgejo-stub';

const test = base.extend<{ own: Factory; stub: ForgejoStub }>({
  own: async ({}, use) => {
    const { factory, stop } = await startFactory();
    await use(factory);
    await stop();
  },
  stub: async ({ own }, use) => {
    const stub = await startForgejoStub(own);
    await use(stub);
    await stub.stop();
  },
  baseURL: async ({ own }, use) => {
    await use(own.url);
  },
});

const PAGE = '/settings/forgejo';

/** Submits the add form and returns the answer to the POST. */
async function add(page: Page, name: string, url: string) {
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('URL', { exact: true }).fill(url);
  const [response] = await Promise.all([
    page.waitForResponse((reply) => reply.request().method() === 'POST'),
    page.getByRole('button', { name: 'Add server' }).click(),
  ]);
  return response;
}

const row = (page: Page, name: string) =>
  page.locator('.forgejo-row').filter({ has: page.locator('.server-name', { hasText: name }) });

test('adding a server lists it with its version', async ({ page, stub }) => {
  await page.goto(PAGE);
  await expect(page.getByText('No servers yet.')).toBeVisible();
  stub.setVersion('17.0.0-121-39320c0+gitea-1.22.0');
  await add(page, 'mine', `${stub.url}/`);

  await expect(page.getByText(`Added mine at ${stub.url}.`)).toBeVisible();
  const mine = row(page, 'mine');
  await expect(mine).toContainText('version 17.0.0-121-39320c0+gitea-1.22.0');
  await expect(mine.locator('.forgejo-warning')).toHaveCount(0);
  const link = mine.getByRole('link', { name: stub.url });
  await expect(link).toHaveAttribute('target', '_blank');
  await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  await expect(page.getByRole('link', { name: 'Forgejo servers' })).toHaveAttribute(
    'aria-current',
    'page',
  );
  expect(stub.seen()).toContain('GET /api/v1/version');

  await page.goto(PAGE);
  await expect(row(page, 'mine')).toContainText('version 17.0.0-121-39320c0+gitea-1.22.0');
});

test('an old server shows what it cannot do', async ({ page, stub }) => {
  await page.goto(PAGE);
  stub.setVersion('16.2.0+gitea-1.22.0');
  await add(page, 'sixteen', stub.url);
  await expect(row(page, 'sixteen').locator('.forgejo-warning')).toHaveCount(1);
  await expect(row(page, 'sixteen')).toContainText('cannot rerun failed CI');

  stub.setVersion('15.0.0+gitea-1.22.0');
  await add(page, 'fifteen', `${stub.url}/sub`);
  expect(stub.seen()).toContain('GET /sub/api/v1/version');
  await expect(row(page, 'fifteen').locator('.forgejo-warning')).toHaveCount(2);
  await expect(row(page, 'fifteen')).toContainText('cannot serve failed job logs');
});

test('a refused add stores nothing', async ({ page, stub }) => {
  await page.goto(PAGE);
  await add(page, 'mine', stub.url);
  await expect(row(page, 'mine')).toBeVisible();

  const host = new URL(stub.url).host;
  const refusals: [string, string, string][] = [
    ['github', `${stub.url}/a`, 'built-in forge'],
    ['salsa', `${stub.url}/a`, 'built-in forge'],
    ['mine', `${stub.url}/a`, 'already exists'],
    ['other', `${stub.url}/`, 'already registered'],
    ['other', `http://${host}/a`, 'must use https'],
    ['other', `https://user:hunter2secret@${host}/a`, 'user name or password'],
    ['other', `https://@${host}/a`, 'user name or password'],
    ['other', `${stub.url}/a?x=1`, 'must not hold a query'],
    ['other', `${stub.url}/a#top`, 'must not hold a fragment'],
    ['other', 'https://127.0.0.1:1', 'did not answer as a Forgejo server'],
  ];
  for (const [name, url, message] of refusals) {
    const response = await add(page, name, url);
    expect(response.status(), `${name} ${url}`).toBe(400);
    await expect(page.locator('.card.failed')).toContainText(message);
    await expect(page.getByLabel('Name', { exact: true })).toHaveValue(name);
    await expect(page.getByLabel('URL', { exact: true })).toHaveValue('');
    expect(await page.content()).not.toContain('hunter2secret');
    await expect(page.locator('.forgejo-row')).toHaveCount(1);
  }

  // A server that answers, but as Gitea, which Forgejo's version string tells apart.
  stub.setVersion('1.22.0');
  await add(page, 'gitea', `${stub.url}/gitea`);
  await expect(page.locator('.card.failed')).toContainText('did not answer as a Forgejo server');
  await expect(page.locator('.forgejo-row')).toHaveCount(1);
});

test('a login that is not an admin is turned away', async ({ page, own }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  const response = await page.goto(PAGE);
  expect(response?.status()).toBe(403);
  await expect(page.getByText('This page is for factory admins.')).toBeVisible();

  const headers = { 'Tailscale-User-Login': COLLABORATOR };
  const added = await page.request.post(`${own.url}${PAGE}`, {
    headers,
    form: { csrf: 'x', name: 'mine', url: 'https://example.org' },
  });
  expect(added.status()).toBe(403);
  const deleted = await page.request.post(`${own.url}${PAGE}/mine/delete`, {
    headers,
    form: { csrf: 'x' },
  });
  expect(deleted.status()).toBe(403);
});

test('delete removes an unused server and refuses one a project uses', async ({
  page,
  own,
  stub,
}) => {
  await page.goto(PAGE);
  await add(page, 'mine', stub.url);
  await add(page, 'spare', `${stub.url}/spare`);
  own.fixture('project', '--name', 'on-forgejo', '--forge', 'mine', '--login', ADMIN);

  await row(page, 'mine').getByRole('button', { name: 'Delete' }).click();
  await expect(page.locator('.card.failed')).toContainText('Nothing was deleted');
  await expect(page.locator('.card.failed')).toContainText('on-forgejo');
  await expect(row(page, 'mine')).toBeVisible();

  await row(page, 'spare').getByRole('button', { name: 'Delete' }).click();
  await expect(page.getByText('Deleted spare')).toBeVisible();
  await expect(row(page, 'spare')).toHaveCount(0);
  await page.goto(PAGE);
  await expect(page.locator('.forgejo-row')).toHaveCount(1);
});
