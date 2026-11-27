import { COLLABORATOR, expect, startFactory, test as base, type Factory } from '../fixtures';

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

const PAGE = '/settings/openrouter';

test('a non-admin saves, keeps and clears their OpenRouter key', async ({ page }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto('/settings');
  await page.getByRole('link', { name: 'OpenRouter', exact: true }).click();
  await expect(page).toHaveURL(PAGE);
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');

  await page.getByLabel('Key').fill('sk-or-e2e-personal');
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'set');
  expect(await page.content()).not.toContain('sk-or-e2e-personal');

  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'set');

  await page.getByRole('button', { name: 'Clear key' }).click();
  await expect(page.getByText('Cleared.', { exact: true })).toBeVisible();
  await expect(page.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');
});

test('a login picks its OpenRouter CLI and the choice names the factory default', async ({
  page,
}) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto(PAGE);
  const cli = page.getByLabel('CLI');
  await expect(cli.locator('option:checked')).toHaveText('Factory default (Codex)');

  await cli.selectOption({ label: 'Copilot' });
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(cli.locator('option:checked')).toHaveText('Copilot');

  await page.reload();
  await expect(cli.locator('option:checked')).toHaveText('Copilot');
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
  await carol.getByLabel('Key').fill('sk-or-e2e-carol');
  await carol.getByRole('button', { name: 'Save', exact: true }).click();
  await dave.goto(PAGE);
  await expect(dave.getByLabel('Key')).toHaveAttribute('placeholder', 'not set');
  await carol.goto(PAGE);
  await expect(carol.getByLabel('Key')).toHaveAttribute('placeholder', 'set');
});
