// Factory-wide settings use cases, by test name. Each test runs against its own
// server, since these pages change settings every project shares.
//
// - Workers, Default gates, GitHub and Renovate each save a changed value, and
//   the value is still there after a reload:
//   "Workers saves and keeps its values", "Default gates saves and keeps its
//   values", "GitHub saves and keeps its value", "Renovate saves and keeps its
//   values"
// - Claude: set a token and clear it: "Claude sets and clears the token"
// - Codex:
//   - upload an auth.json and clear it: "Codex stores and clears an auth.json"
//   - add a price row and delete it: "Codex adds and deletes a price row"
//   - start a device login and see the fake's code, and cancel a login:
//     "Codex device login shows the code and can be cancelled"
// - Copilot: set a token and clear it: "Copilot sets and clears the token";
//   start a device login and cancel it:
//   "Copilot device login shows the code and can be cancelled"
// - MCP servers: add a server, edit it, add and remove header-name rows, and
//   delete the server: "MCP servers: add, edit with header rows, and delete"
// - A login that is not an admin gets "This page is for factory admins.":
//   "a login that is not an admin is turned away from the factory-wide settings"
import type { Page } from '@playwright/test';
import { expect, startFactory, test as base, type Factory, COLLABORATOR } from '../fixtures';

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

const gate = (page: Page, name: string) => page.getByRole('checkbox', { name });

test('Workers saves and keeps its values', async ({ page }) => {
  await page.goto('/settings/workers');
  await page.getByLabel('Workers at a time').fill('3');
  await page.getByLabel('Image').fill('registry.example/worker:e2e');
  await page.getByLabel('Memory').fill('5g');
  await page.getByLabel('CPUs').fill('2');
  await expect(page.getByText('Unsaved changes.')).toBeVisible();
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/workers');
  await expect(page.getByLabel('Workers at a time')).toHaveValue('3');
  await expect(page.getByLabel('Image')).toHaveValue('registry.example/worker:e2e');
  await expect(page.getByLabel('Memory')).toHaveValue('5g');
  await expect(page.getByLabel('CPUs')).toHaveValue('2');
});

test('Default gates saves and keeps its values', async ({ page }) => {
  await page.goto('/settings/gates');
  const spec = gate(page, 'Approve the spec');
  const plan = gate(page, 'Approve the plan');
  const publish = gate(page, 'Approve before pushing');
  const before = [await spec.isChecked(), await plan.isChecked(), await publish.isChecked()];
  await spec.setChecked(!before[0]);
  await plan.setChecked(!before[1]);
  await publish.setChecked(!before[2]);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/gates');
  await expect(gate(page, 'Approve the spec')).toBeChecked({ checked: !before[0] });
  await expect(gate(page, 'Approve the plan')).toBeChecked({ checked: !before[1] });
  await expect(gate(page, 'Approve before pushing')).toBeChecked({ checked: !before[2] });
});

test('Pull request polling saves and keeps its value', async ({ page }) => {
  await page.goto('/settings/polling');
  await page.getByLabel('Seconds between looks at a pull request').fill('77');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/polling');
  await expect(page.getByLabel('Seconds between looks at a pull request')).toHaveValue('77');
});

test('a form that still posts to /settings/github saves the interval', async ({ page }) => {
  await page.goto('/settings/polling');
  await page.getByLabel('Seconds between looks at a pull request').fill('55');
  await page.locator('form.settings-form').evaluate((form: HTMLFormElement) => {
    form.action = '/settings/github';
  });
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/\/settings\/polling$/);
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/polling');
  await expect(page.getByLabel('Seconds between looks at a pull request')).toHaveValue('55');
});

test('Renovate saves and keeps its values', async ({ page }) => {
  await page.goto('/settings/renovate');
  await page.getByLabel('Automatic reruns').fill('1');
  await page.getByLabel('Total reruns').fill('6');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/renovate');
  await expect(page.getByLabel('Automatic reruns')).toHaveValue('1');
  await expect(page.getByLabel('Total reruns')).toHaveValue('6');
});

test('Claude sets and clears the token', async ({ page }) => {
  await page.goto('/settings/agents/claude');
  const token = page.getByLabel('Token');
  await expect(token).toHaveAttribute('placeholder', 'not set');
  await token.fill('sk-ant-oat01-e2e-token');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/agents/claude');
  await expect(page.getByLabel('Token')).toHaveAttribute('placeholder', 'set');
  await expect(page.getByText('A token is stored.')).toBeVisible();

  await page.getByRole('button', { name: 'Clear token' }).click();
  await expect(page.getByText('Cleared.', { exact: true })).toBeVisible();
  await page.goto('/settings/agents/claude');
  await expect(page.getByLabel('Token')).toHaveAttribute('placeholder', 'not set');
});

test('Codex stores and clears an auth.json', async ({ page }) => {
  const auth = JSON.stringify({
    tokens: { access_token: 'access-e2e-aaaaaaaa', refresh_token: 'refresh-e2e-bbbbbbbb' },
    last_refresh: '2026-01-02T03:04:05Z',
  });
  await page.goto('/settings/agents/codex');
  await expect(page.getByText('No login is stored.').first()).toBeVisible();
  await page.getByLabel('auth.json').fill(auth);
  await page.getByRole('button', { name: 'Save login' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/agents/codex');
  await expect(page.getByText('A login is stored.').first()).toBeVisible();
  await expect(page.getByLabel('auth.json')).toHaveAttribute('placeholder', 'set');

  await page.getByRole('button', { name: 'Clear login' }).click();
  await expect(page.getByText('Cleared.', { exact: true })).toBeVisible();
  await page.goto('/settings/agents/codex');
  await expect(page.getByText('No login is stored.').first()).toBeVisible();
});

test('Codex adds and deletes a price row', async ({ page }) => {
  await page.goto('/settings/agents/codex');
  await expect(page.getByText('No model prices are set.')).toBeVisible();
  const add = page.locator('form.price-add');
  await add.getByLabel('Model').fill('gpt-e2e');
  await add.locator('input[name=input]').fill('1.25');
  await add.locator('input[name=cached_input]').fill('0.125');
  await add.locator('input[name=output]').fill('10');
  await add.getByRole('button', { name: 'Add price' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/agents/codex');
  const row = page.locator('form.price-row').filter({ hasText: 'gpt-e2e' });
  await expect(row.locator('input[name=input]')).toHaveValue('1.25');
  await expect(row.locator('input[name=cached_input]')).toHaveValue('0.125');
  await expect(row.locator('input[name=output]')).toHaveValue('10');

  await row.getByRole('button', { name: 'Delete' }).click();
  await expect(page.getByText('Deleted.', { exact: true })).toBeVisible();
  await expect(page.locator('form.price-row')).toHaveCount(0);
  await expect(page.getByText('No model prices are set.')).toBeVisible();
});

for (const agent of [
  { name: 'Codex', path: 'codex', start: 'Sign in with ChatGPT' },
  { name: 'Copilot', path: 'copilot', start: 'Sign in with GitHub' },
]) {
  test(`${agent.name} device login shows the code and can be cancelled`, async ({ page }) => {
    await page.goto(`/settings/agents/${agent.path}`);
    await page.getByRole('button', { name: agent.start }).click();
    await expect(page.locator('.code-prompt')).toHaveText('ABCD-EFGH');
    await expect(page.getByText('Waiting for you to finish in the browser…')).toBeVisible();

    await page.getByRole('button', { name: 'Cancel' }).click();
    await expect(page.locator('.code-prompt')).toHaveCount(0);
    await expect(page.getByRole('button', { name: agent.start })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Cancel' })).toHaveCount(0);
  });
}

test('Copilot sets and clears the token', async ({ page }) => {
  await page.goto('/settings/agents/copilot');
  await expect(page.getByText('No token is stored.').first()).toBeVisible();
  await page.getByLabel('Token').fill('gho_e2eCopilotToken0123456789abcdefghij');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/agents/copilot');
  await expect(page.getByText('A token is stored.').first()).toBeVisible();
  await expect(page.getByLabel('Token')).toHaveAttribute('placeholder', 'set');

  await page.getByRole('button', { name: 'Clear token' }).click();
  await expect(page.getByText('Cleared.', { exact: true })).toBeVisible();
  await page.goto('/settings/agents/copilot');
  await expect(page.getByText('No token is stored.').first()).toBeVisible();
});

test('MCP servers: add, edit with header rows, and delete', async ({ page }) => {
  await page.goto('/settings/mcp');
  await expect(page.getByText('No servers yet.')).toBeVisible();
  await page.getByLabel('Name', { exact: true }).fill('docs');
  await page.getByLabel('URL').fill('https://mcp.example.test/mcp');
  const headers = page.getByLabel('Header name');
  await headers.first().fill('X-Api-Key');
  await page.getByRole('button', { name: 'Add header' }).click();
  await expect(headers).toHaveCount(2);
  await headers.nth(1).fill('X-Team');
  await page.getByRole('button', { name: 'Add server' }).click();
  await expect(page.getByText('Added.', { exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'docs', level: 1 })).toBeVisible();

  await expect(headers).toHaveCount(3);
  await expect(headers.nth(0)).toHaveValue('X-Api-Key');
  await expect(headers.nth(1)).toHaveValue('X-Team');
  await page.getByRole('button', { name: 'Remove' }).nth(1).click();
  await expect(headers).toHaveCount(2);
  await page.getByRole('button', { name: 'Add header' }).click();
  await headers.last().fill('X-Region');
  await page.getByLabel('URL', { exact: true }).fill('https://mcp.example.test/v2/mcp');
  await expect(page.getByText('Unsaved changes.')).toBeVisible();
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/mcp/docs');
  await expect(page.getByLabel('URL', { exact: true })).toHaveValue('https://mcp.example.test/v2/mcp');
  await expect(headers.nth(0)).toHaveValue('X-Api-Key');
  await expect(headers.nth(1)).toHaveValue('X-Region');
  await expect(headers).toHaveCount(3);

  await page.getByLabel('Server name').fill('docs');
  await page.getByRole('button', { name: 'Delete docs' }).click();
  await expect(page.getByText('Deleted.', { exact: true })).toBeVisible();
  await expect(page.getByText('No servers yet.')).toBeVisible();
});

test('MCP servers: add a stdio server with argument rows, then edit it', async ({ page }) => {
  await page.goto('/settings/mcp');
  const url = page.getByLabel('URL', { exact: true });
  const command = page.getByLabel('Command', { exact: true });
  await expect(url).toBeVisible();
  await expect(command).toBeHidden();

  await page.getByLabel('stdio', { exact: true }).check();
  await expect(url).toBeHidden();
  await expect(page.getByLabel('Header name')).toBeHidden();
  await expect(command).toBeVisible();

  await page.getByLabel('Name', { exact: true }).fill('playwright');
  await command.fill('npx');
  const args = page.getByLabel('Argument');
  await expect(args).toHaveCount(3);
  await args.nth(0).fill('-y');
  await args.nth(1).fill('--junk');
  await args.nth(2).fill('@playwright/mcp@0.0.83');
  await page.getByRole('button', { name: 'Remove' }).nth(1).click();
  await expect(args).toHaveCount(2);
  await page.getByRole('button', { name: 'Add argument' }).click();
  await expect(args).toHaveCount(3);
  await page.getByRole('button', { name: 'Add server' }).click();

  await expect(page.getByText('Added.', { exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'playwright', level: 1 })).toBeVisible();
  await expect(page.getByText('Kind: stdio')).toBeVisible();
  await expect(command).toHaveValue('npx');
  await expect(args.nth(0)).toHaveValue('-y');
  await expect(args.nth(1)).toHaveValue('@playwright/mcp@0.0.83');
  await expect(page.getByLabel('Header name')).toHaveCount(0);

  await args.nth(2).fill('--no-sandbox');
  await expect(page.getByText('Unsaved changes.')).toBeVisible();
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto('/settings/mcp');
  await expect(page.getByText('stdio: npx -y @playwright/mcp@0.0.83 --no-sandbox')).toBeVisible();
});

test('MCP servers: a stale remote field cannot block a stdio add, and argument rows refill', async ({ page }) => {
  await page.goto('/settings/mcp');
  await page.getByLabel('URL', { exact: true }).fill('not a url');
  await page.getByLabel('Header name').first().fill('bad header:');
  await page.getByLabel('stdio', { exact: true }).check();
  await page.getByLabel('Name', { exact: true }).fill('rows');
  await page.getByLabel('Command', { exact: true }).fill('npx');
  const args = page.getByLabel('Argument');
  for (let i = 0; i < 3; i++) await page.getByRole('button', { name: 'Remove' }).last().click();
  await expect(args).toHaveCount(0);
  await page.getByRole('button', { name: 'Add argument' }).click();
  await expect(args).toHaveCount(1);
  await args.first().fill('-y');
  await page.getByRole('button', { name: 'Add server' }).click();
  await expect(page.getByText('Added.', { exact: true })).toBeVisible();
  await expect(args.first()).toHaveValue('-y');
});

test.describe('without JavaScript', () => {
  test.use({ javaScriptEnabled: false });

  test('MCP servers: arguments with line breaks survive a command edit and can be edited or removed', async ({
    page,
  }) => {
    const added = await page.request.post('/settings/mcp', {
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      data: new URLSearchParams([
        ['name', 'multi'],
        ['kind', 'stdio'],
        ['command', 'npx'],
        ['arg', 'a\nb'],
        ['arg', 'c\rd'],
        ['arg', '--headless'],
      ]).toString(),
    });
    expect(added.ok()).toBe(true);
    const save = async () => {
      await page.getByRole('button', { name: 'Save', exact: true }).click();
      await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
    };

    await page.goto('/settings/mcp/multi');
    await page.getByLabel('Command', { exact: true }).fill('uvx');
    await save();
    await expect(page.getByLabel('Argument with a line break')).toHaveCount(2);
    await expect(page.getByLabel('Argument with a line break').nth(0)).toHaveValue('a\\nb');
    await expect(page.getByLabel('Argument with a line break').nth(1)).toHaveValue('c\\rd');

    await page.getByLabel('Argument with a line break').nth(0).fill('x\\ny\\\\z');
    await page.getByLabel('Argument with a line break').nth(1).fill('');
    await save();
    await expect(page.getByLabel('Argument with a line break')).toHaveCount(1);
    await expect(page.getByLabel('Argument with a line break')).toHaveValue('x\\ny\\\\z');
    await expect(page.getByLabel('Argument', { exact: true }).first()).toHaveValue('--headless');
  });
});

test('a login that is not an admin is turned away from the factory-wide settings', async ({
  page,
}) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  for (const path of [
    '/settings/workers',
    '/settings/gates',
    '/settings/polling',
    '/settings/renovate',
    '/settings/agents/claude',
    '/settings/agents/codex',
    '/settings/agents/copilot',
    '/settings/mcp',
  ]) {
    const response = await page.goto(path);
    expect(response?.status()).toBe(403);
    await expect(page.getByText('This page is for factory admins.')).toBeVisible();
  }
});
