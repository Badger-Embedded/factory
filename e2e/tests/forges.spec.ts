import {
  ADMIN,
  COLLABORATOR,
  expect,
  startFactory,
  test as base,
  type Factory,
} from '../fixtures';
import { startForgejoStub } from '../forgejo-stub';

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

const PAGE = '/settings/forges/github';

test('a non-admin saves and clears their GitHub token', async ({ page }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto(PAGE);
  await expect(page.getByText('No token is stored.').first()).toBeVisible();

  await page.getByLabel('Token').fill('tok-e2e-alice');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to alice-bot.')).toBeVisible();
  expect(await page.content()).not.toContain('tok-e2e-alice');

  await page.goto(PAGE);
  await expect(page.getByText('A token is stored.').first()).toBeVisible();
  await expect(page.locator('.forge-account')).toHaveText('alice-bot');
  await expect(page.getByLabel('Token')).toHaveAttribute('placeholder', 'set');
  await expect(page.getByLabel('Token')).toHaveValue('');
  expect(await page.content()).not.toContain('tok-e2e-alice');

  await page.getByRole('button', { name: 'Clear token' }).click();
  await expect(page.getByText('Cleared.', { exact: true })).toBeVisible();
  await expect(page.getByText('No token is stored.').first()).toBeVisible();
});

test('a rejected token is not stored', async ({ page }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto(PAGE);
  await page.getByLabel('Token').fill('tok-e2e-wrong');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('GitHub rejected the token. Nothing was saved.')).toBeVisible();
  expect(await page.content()).not.toContain('tok-e2e-wrong');
  await expect(page.getByText('No token is stored.').first()).toBeVisible();
});

test('two logins see only their own token', async ({ browser, own }) => {
  const open = async (login: string) => {
    const context = await browser.newContext({
      baseURL: own.url,
      extraHTTPHeaders: { 'Tailscale-User-Login': login },
    });
    return context.newPage();
  };
  const carol = await open('carol@e2e.test');
  const dave = await open('dave@e2e.test');
  for (const [page, token] of [
    [carol, 'tok-e2e-alice'],
    [dave, 'tok-e2e-bob'],
  ] as const) {
    await page.goto(PAGE);
    await page.getByLabel('Token').fill(token);
    await page.getByRole('button', { name: 'Save token' }).click();
  }
  await carol.goto(PAGE);
  await dave.goto(PAGE);
  await expect(carol.locator('.forge-account')).toHaveText('alice-bot');
  await expect(dave.locator('.forge-account')).toHaveText('bob-bot');
});

test('a Salsa token is checked against Salsa and saved', async ({ page }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto('/settings/forges/salsa');
  await page.getByLabel('Token').fill('tok-e2e-wrong');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Salsa rejected the token. Nothing was saved.')).toBeVisible();

  await page.getByLabel('Token').fill('tok-e2e-deb');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to deb-bot.')).toBeVisible();
  expect(await page.content()).not.toContain('tok-e2e-deb');
});

test('the sidebar lists the forges and an unknown forge is a 404', async ({ page }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto('/settings/notifications');
  await expect(page.locator('.settings-group', { hasText: 'Forges' })).toBeVisible();
  await page.getByRole('link', { name: 'GitHub', exact: true }).click();
  await expect(page).toHaveURL(/\/settings\/forges\/github$/);
  await expect(page.getByRole('link', { name: 'GitHub', exact: true })).toHaveAttribute(
    'aria-current',
    'page',
  );

  const response = await page.goto('/settings/forges/other');
  expect(response?.status()).toBe(404);
});

test('forms posted from another origin change nothing', async ({ page, own }) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  await page.goto(PAGE);
  await page.getByLabel('Token').fill('tok-e2e-alice');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to alice-bot.')).toBeVisible();

  const evil = 'http://evil.example';
  await page.route(`${evil}/**`, (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: `<form method="post" action="${own.url}${PAGE}/token">
        <input name="token" value="tok-e2e-bob"><button id="save">save</button></form>
        <form method="post" action="${own.url}${PAGE}/token/clear"><button id="clear">clear</button></form>`,
    }),
  );
  for (const button of ['#save', '#clear']) {
    await page.goto(`${evil}/`);
    const [response] = await Promise.all([
      page.waitForResponse((r) => r.url().startsWith(own.url) && r.request().method() === 'POST'),
      page.click(button),
    ]);
    expect(response.status()).toBe(403);
  }

  await page.goto(PAGE);
  await expect(page.locator('.forge-account')).toHaveText('alice-bot');
});

test('a token for a registered Forgejo server is checked against it and saved', async ({
  page,
  own,
}) => {
  const stub = await startForgejoStub(own);
  try {
    stub.route((request, url) => {
      if (!url.pathname.endsWith('/api/v1/user')) return undefined;
      return request.headers.authorization === 'token tok-e2e-forgejo'
        ? { body: { login: 'forgejo-bot', id: 9 } }
        : { status: 401, body: { message: 'bad token' } };
    });

    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': ADMIN });
    await page.goto('/settings/forgejo');
    await page.getByLabel('Name', { exact: true }).fill('mine');
    await page.getByLabel('URL', { exact: true }).fill(stub.url);
    await page.getByRole('button', { name: 'Add server' }).click();
    await expect(page.getByText(`Added mine at ${stub.url}.`)).toBeVisible();

    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
    await page.goto('/settings/notifications');
    await page.getByRole('link', { name: 'mine', exact: true }).click();
    await expect(page).toHaveURL(/\/settings\/forges\/mine$/);
    await expect(page.getByRole('link', { name: 'mine', exact: true })).toHaveAttribute(
      'aria-current',
      'page',
    );
    await expect(page.locator('.forgejo-scopes')).toContainText('read:user');
    await expect(page.locator('.forgejo-scopes')).toContainText('write:repository');
    await expect(page.locator('.forgejo-scopes')).toContainText('write:issue');

    await page.getByLabel('Token').fill('tok-e2e-wrong');
    await page.getByRole('button', { name: 'Save token' }).click();
    await expect(page.getByText('mine rejected the token. Nothing was saved.')).toBeVisible();
    expect(await page.content()).not.toContain('tok-e2e-wrong');
    await expect(page.getByText('No token is stored.').first()).toBeVisible();

    await page.getByLabel('Token').fill('tok-e2e-forgejo');
    await page.getByRole('button', { name: 'Save token' }).click();
    await expect(page.getByText('Saved. The token belongs to forgejo-bot.')).toBeVisible();
    expect(await page.content()).not.toContain('tok-e2e-forgejo');

    const unknown = await page.goto('/settings/forges/nowhere');
    expect(unknown?.status()).toBe(404);
  } finally {
    await stub.stop();
  }
});

test('a newly registered Forgejo server is selectable at onboarding and as an upstream', async ({
  page,
  own,
}) => {
  const stub = await startForgejoStub(own);
  try {
    stub.route((request, url) => {
      if (!url.pathname.endsWith('/api/v1/user')) return undefined;
      return request.headers.authorization === 'token tok-e2e-forgejo'
        ? { body: { login: 'forgejo-bot', id: 9 } }
        : { status: 401, body: { message: 'bad token' } };
    });
    own.fixture('project', '--name', 'packager', '--login', ADMIN);

    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': ADMIN });
    await page.goto('/settings/forgejo');
    await page.getByLabel('Name', { exact: true }).fill('mine');
    await page.getByLabel('URL', { exact: true }).fill(stub.url);
    await page.getByRole('button', { name: 'Add server' }).click();
    await expect(page.getByText(`Added mine at ${stub.url}.`)).toBeVisible();

    await page.goto('/onboard');
    await expect(page.getByLabel('GitHub')).toBeChecked();
    await page.getByLabel('mine', { exact: true }).check();
    await expect(page.getByLabel('mine', { exact: true })).toBeChecked();

    await page.goto('/settings/projects/packager');
    await page.locator('#forge-account').getByLabel('Project token').fill('tok-e2e-alice');
    await page.getByRole('button', { name: 'Save project token' }).click();
    await expect(page.getByText('Saved. The project token belongs to alice-bot.')).toBeVisible();
    const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
    await card.getByLabel('Renovate pull requests').check();
    await card.getByLabel('Packaging project').check();
    await card.getByRole('button', { name: 'Save maintenance' }).click();
    await card.getByLabel('Manifest files').fill('debian/control');
    await expect(card.getByLabel('Upstream forge').locator('option[value="mine"]')).toHaveCount(1);
    await card.getByLabel('Upstream forge').selectOption('mine');

    await expect(card.locator('#upstream-token-mine')).toContainText('No mine token is stored.');
    await card.getByLabel('mine upstream token').fill('tok-e2e-forgejo');
    await card.getByRole('button', { name: 'Save mine upstream token' }).click();
    await expect(card.locator('#upstream-token-mine')).toContainText('forgejo-bot');
    await expect(card.getByLabel('Upstream forge')).toHaveValue('mine');
    expect(await page.content()).not.toContain('tok-e2e-forgejo');

    await card.getByRole('button', { name: 'Clear mine upstream token' }).click();
    await expect(card.locator('#upstream-token-mine')).toContainText('No mine token is stored.');
    await expect(page.locator('#forge-account')).toContainText('alice-bot');
  } finally {
    await stub.stop();
  }
});
