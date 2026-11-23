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
