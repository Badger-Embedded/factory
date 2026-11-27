import {
  ADMIN,
  COLLABORATOR,
  expect,
  startFactory,
  test as base,
  type Factory,
} from '../fixtures';

const test = base.extend<{ own: Factory }>({
  own: async ({}, use) => {
    const { factory, stop } = await startFactory();
    await use(factory);
    await stop();
  },
  baseURL: async ({ own }, use) => {
    await use(own.url);
  },
});

const PAGE = '/settings/local';

test('a non-admin saves, keeps and clears their Local API key', async ({ page }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto('/settings');
  await page.getByRole('link', { name: 'Local', exact: true }).click();
  await expect(page).toHaveURL(PAGE);
  await expect(page.getByText('no local server is set')).toBeVisible();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');

  await page.getByLabel('Key').fill('sk-e2e-personal');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'set');
  expect(await page.content()).not.toContain('sk-e2e-personal');

  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'set');

  await page.getByRole('button', { name: 'Clear key' }).click();
  await expect(page.getByText('Cleared.', { exact: true })).toBeVisible();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');
});

test('a login picks its CLI and the choice names the factory default', async ({ page, own }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto(PAGE);
  const cli = page.getByLabel('CLI');
  await expect(cli.locator('option:checked')).toHaveText('Factory default (Codex)');

  await cli.selectOption({ label: 'Claude Code' });
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(cli.locator('option:checked')).toHaveText('Claude Code');

  await page.reload();
  await expect(cli.locator('option:checked')).toHaveText('Claude Code');
});

test('two logins see only their own key', async ({ browser, own }) => {
  const open = async (login: string) => {
    const context = await browser.newContext({
      baseURL: own.url,
      extraHTTPHeaders: { 'Tailscale-User-Login': login },
    });
    return context.newPage();
  };
  const carol = await open('carol@e2e.test');
  const dave = await open('dave@e2e.test');
  await carol.goto(PAGE);
  await carol.getByLabel('Key').fill('sk-e2e-carol');
  await carol.getByRole('button', { name: 'Save', exact: true }).click();
  await dave.goto(PAGE);
  await expect(dave.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');
  await carol.goto(PAGE);
  await expect(carol.getByLabel('Key')).toHaveAttribute('placeholder', 'set');
});

test('a key typed on a page of the old server is not saved', async ({ browser, own }) => {
  const open = async (login: string) => {
    const context = await browser.newContext({
      baseURL: own.url,
      extraHTTPHeaders: { 'Tailscale-User-Login': login },
    });
    return context.newPage();
  };
  const admin = await open(ADMIN);
  const carol = await open('carol@e2e.test');
  const setServer = async (url: string) => {
    await admin.goto('/settings/agents/local');
    await admin.getByLabel('Server URL').fill(url);
    await admin.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(admin.getByText(/^Saved\./)).toBeVisible();
  };
  await setServer('http://127.0.0.1:9');

  await carol.goto(PAGE);
  await expect(carol.getByText('http://127.0.0.1:9', { exact: true })).toBeVisible();
  await carol.getByLabel('Key').fill('sk-e2e-stale');

  await setServer('http://127.0.0.1:10');

  await carol.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(carol.getByText(/the key was not saved/)).toBeVisible();
  await expect(carol.getByText('http://127.0.0.1:10', { exact: true })).toBeVisible();
  await expect(carol.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');
  expect(await carol.content()).not.toContain('sk-e2e-stale');

  await carol.getByLabel('Key').fill('sk-e2e-fresh');
  await carol.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(carol.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(carol.getByLabel('Key')).toHaveAttribute('placeholder', 'set');
});
