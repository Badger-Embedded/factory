// Onboarding (/onboard) use cases, by test name:
//
// - Onboarding a repository whose checks pass, through the fake gh, creates the
//   project, and the project appears in the picker:
//   "onboarding a repository whose checks pass creates the project"
// - A repository whose check fails shows the failure and creates no project:
//   "a repository whose check fails shows the failure and creates no project"
import type { Page } from '@playwright/test';
import { ADMIN, expect, startFactory, test, type Factory } from '../fixtures';
import { startForgejoStub, type ForgejoStub } from '../forgejo-stub';

let counter = 0;

function uniqueLogin(workerIndex: number) {
  return `onboard-${workerIndex}-${counter++}@e2e.test`;
}

async function saveToken(page: Page) {
  await page.goto('/settings/forges/github');
  await page.getByLabel('Token').fill('tok-e2e-alice');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to alice-bot.')).toBeVisible();
}

test('onboarding a repository whose checks pass creates the project', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  const name = `onboarded-${testInfo.workerIndex}-${counter}`;
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await saveToken(page);
  await page.goto('/onboard');
  await page.getByLabel('Repository').fill('e2e/good');
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByRole('button', { name: 'Run checks' }).click();

  await expect(page.getByText('All required checks passed.')).toBeVisible();
  await page.getByRole('button', { name: 'Skip extras and create' }).click();

  await expect(page).toHaveURL(new RegExp(`/\\?project=${name}$`));
  await expect(page.getByLabel('Project', { exact: true })).toContainText(name);
  await expect(page.locator('#picker-list li[data-id]')).toHaveCount(1);
  await expect(page.locator('#picker-list li[data-id] a')).toHaveAttribute('href', `/?project=${name}`);
  await expect(page.locator('.missing-project-token')).toBeVisible();
  await page.goto(`/settings/projects/${name}`);
  await expect(page.locator('input[name="on"]')).toBeChecked();
});

test('a repository whose check fails shows the failure and creates no project', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await saveToken(page);
  await page.goto('/onboard');
  await page.getByLabel('Repository').fill('e2e/missing');
  await page.getByRole('button', { name: 'Run checks' }).click();

  await expect(page.locator('ul.check-errors')).toContainText(
    'The repository e2e/missing does not exist or is not visible to alice-bot.',
  );
  await expect(page.getByRole('button', { name: 'Create project' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Skip extras and create' })).toHaveCount(0);

  await page.goto('/');
  await expect(page.getByText('You are not a collaborator on any project.')).toBeVisible();
});

test('a login with no token is blocked and linked to the forge page', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await page.goto('/onboard');
  await page.getByLabel('Repository').fill('e2e/good');
  await page.getByRole('button', { name: 'Run checks' }).click();

  await expect(page.locator('ul.check-errors')).toContainText('You have no GitHub token stored');
  await expect(page.getByRole('button', { name: 'Skip extras and create' })).toHaveCount(0);
  await page.getByRole('link', { name: 'Open the token page' }).click();
  await expect(page).toHaveURL(/\/settings\/forges\/github$/);
});

test('a Salsa project onboards with the forge selector and a nested path', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  const name = `salsa-${testInfo.workerIndex}-${counter}`;
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await page.goto('/settings/forges/salsa');
  await page.getByLabel('Token').fill('tok-e2e-deb');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to deb-bot.')).toBeVisible();

  await page.goto('/onboard');
  await expect(page.getByLabel('GitHub')).toBeChecked();
  await page.getByLabel('Salsa').check();
  await page.getByLabel('Repository').fill('e2e/sub/good');
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByRole('button', { name: 'Run checks' }).click();

  await expect(page.getByLabel('Salsa')).toBeChecked();
  await expect(page.getByText('All required checks passed.')).toBeVisible();
  await page.getByRole('button', { name: 'Skip extras and create' }).click();

  await expect(page).toHaveURL(new RegExp(`/\\?project=${name}$`));
  await page.goto(`/settings/projects/${name}`);
  await expect(page.getByRole('link', { name: 'e2e/sub/good', exact: true })).toHaveAttribute(
    'href',
    'https://salsa.debian.org/e2e/sub/good',
  );
});

test('a Salsa project the account cannot push to shows the failure', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await page.goto('/settings/forges/salsa');
  await page.getByLabel('Token').fill('tok-e2e-deb');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to deb-bot.')).toBeVisible();

  await page.goto('/onboard');
  await page.getByLabel('Salsa').check();
  await page.getByLabel('Repository').fill('e2e/sub/reporter');
  await page.getByRole('button', { name: 'Run checks' }).click();
  await expect(page.locator('ul.check-errors')).toContainText('deb-bot has no push access');
  await expect(page.getByRole('button', { name: 'Skip extras and create' })).toHaveCount(0);

  await page.getByLabel('Repository').fill('e2e/sub/nope');
  await page.getByRole('button', { name: 'Run checks' }).click();
  await expect(page.locator('ul.check-errors')).toContainText('not visible to deb-bot');
});

test('a Salsa onboarding with no Salsa token links the Salsa page', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
  await saveToken(page);

  await page.goto('/onboard');
  await page.getByLabel('Salsa').check();
  await page.getByLabel('Repository').fill('e2e/sub/good');
  await page.getByRole('button', { name: 'Run checks' }).click();
  await expect(page.locator('ul.check-errors')).toContainText('You have no Salsa token stored');
  await page.getByRole('link', { name: 'Open the token page' }).click();
  await expect(page).toHaveURL(/\/settings\/forges\/salsa$/);
});

test('the repository guidance follows the selected forge before submitting', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await page.goto('/onboard');
  const repo = page.getByLabel('Repository');
  await expect(repo).toHaveAttribute('placeholder', 'owner/name');
  await expect(page.getByText('On GitHub, owner/name')).toBeVisible();
  await expect(page.getByText('On Salsa, the full project path')).toBeHidden();

  await page.getByLabel('Salsa').check();
  await expect(repo).toHaveAttribute('placeholder', 'group/subgroup/project');
  await expect(page.getByText('On Salsa, the full project path')).toBeVisible();
  await expect(page.getByText('On GitHub, owner/name')).toBeHidden();

  await page.getByLabel('GitHub').check();
  await expect(repo).toHaveAttribute('placeholder', 'owner/name');
});

const REPO = { permissions: { admin: false, push: true, pull: true }, allow_squash_merge: true, has_pull_requests: true, has_actions: true };

function forgejoRepo(stub: ForgejoStub, repo: () => object | undefined, protectedBranch: () => boolean) {
  stub.route((request, url) => {
    const path = url.pathname.replace('/api/v1', '');
    if (path === '/user') {
      return request.headers.authorization === 'token tok-e2e-forgejo'
        ? { body: { login: 'forgejo-bot', id: 9 } }
        : { status: 401, body: { message: 'bad token' } };
    }
    if (request.headers.authorization !== 'token tok-e2e-forgejo') return undefined;
    if (path === '/repos/team/proj') {
      const body = repo();
      return body ? { body } : { status: 404, body: { message: 'not found' } };
    }
    if (path === '/repos/team/proj/branches/main') {
      return { body: { protected: protectedBranch(), user_can_merge: true } };
    }
    if (path === '/repos/team/proj/contents/.forgejo/workflows') {
      return { body: [{ name: 'ci.yaml', type: 'file' }] };
    }
    return undefined;
  });
}

test('onboarding on a Forgejo instance follows its guidance, warnings and errors', async ({ browser }) => {
  const { factory, stop }: { factory: Factory; stop: () => Promise<void> } = await startFactory();
  const stub = await startForgejoStub(factory);
  try {
    let repo: object | undefined = REPO;
    let protectedBranch = false;
    forgejoRepo(stub, () => repo, () => protectedBranch);

    const open = async (login: string) => {
      const context = await browser.newContext({
        baseURL: factory.url,
        extraHTTPHeaders: { 'Tailscale-User-Login': login },
      });
      return context.newPage();
    };
    const admin = await open(ADMIN);
    await admin.goto('/settings/forgejo');
    await admin.getByLabel('Name', { exact: true }).fill('mine');
    await admin.getByLabel('URL', { exact: true }).fill(stub.url);
    await admin.getByRole('button', { name: 'Add server' }).click();
    await expect(admin.getByText(`Added mine at ${stub.url}.`)).toBeVisible();

    const page = await open('forgejo-onboarder@e2e.test');
    await page.goto('/onboard');
    const repoField = page.getByLabel('Repository');
    await page.getByLabel('mine', { exact: true }).check();
    await expect(repoField).toHaveAttribute('placeholder', 'owner/name');
    await expect(page.getByText('On mine, owner/name')).toBeVisible();
    await expect(page.getByText('On GitHub, owner/name')).toBeHidden();

    await repoField.fill('team/proj');
    await page.getByRole('button', { name: 'Run checks' }).click();
    await expect(page.locator('ul.check-errors')).toContainText('You have no mine token stored');
    expect(stub.seen().filter((line) => line.includes('/repos/'))).toEqual([]);
    await page.getByRole('link', { name: 'Open the token page' }).click();
    await expect(page).toHaveURL(/\/settings\/forges\/mine$/);
    await page.getByLabel('Token').fill('tok-e2e-forgejo');
    await page.getByRole('button', { name: 'Save token' }).click();
    await expect(page.getByText('Saved. The token belongs to forgejo-bot.')).toBeVisible();

    await page.goto('/onboard');
    await page.getByLabel('mine', { exact: true }).check();
    await repoField.fill('team/proj');
    await page.getByRole('button', { name: 'Run checks' }).click();
    await expect(page.getByLabel('mine', { exact: true })).toBeChecked();
    await expect(page.getByText('On mine, owner/name')).toBeVisible();
    await expect(page.getByText('All required checks passed.')).toBeVisible();
    await expect(page.locator('ul.check-warnings')).toContainText('has no protection');

    repo = { ...REPO, allow_squash_merge: false };
    await page.getByRole('button', { name: 'Re-run checks' }).click();
    await expect(page.locator('ul.check-errors')).toContainText('does not allow squash merges');
    await expect(page.getByRole('button', { name: 'Skip extras and create' })).toHaveCount(0);

    repo = REPO;
    protectedBranch = true;
    await page.getByRole('button', { name: 'Run checks' }).click();
    await expect(page.locator('ul.check-warnings')).toHaveCount(0);
    await page.getByLabel('Name', { exact: true }).fill('forgejo-proj');
    await page.getByRole('button', { name: 'Skip extras and create' }).click();
    await expect(page).toHaveURL(/\/\?project=forgejo-proj$/);
    await page.goto('/settings/projects/forgejo-proj');
    await expect(page.getByRole('link', { name: 'team/proj', exact: true })).toHaveAttribute(
      'href',
      `${stub.url}/team/proj`,
    );
  } finally {
    await stub.stop();
    await stop();
  }
});

test('a stale onboarding form needs a fresh forge selection before it continues', async ({ browser }) => {
  const { factory, stop }: { factory: Factory; stop: () => Promise<void> } = await startFactory();
  const stub = await startForgejoStub(factory);
  try {
    const open = async (login: string) => {
      const context = await browser.newContext({
        baseURL: factory.url,
        extraHTTPHeaders: { 'Tailscale-User-Login': login },
      });
      return context.newPage();
    };
    const admin = await open(ADMIN);
    const register = async () => {
      await admin.goto('/settings/forgejo');
      await admin.getByLabel('Name', { exact: true }).fill('mine');
      await admin.getByLabel('URL', { exact: true }).fill(stub.url);
      await admin.getByRole('button', { name: 'Add server' }).click();
      await expect(admin.getByText(`Added mine at ${stub.url}.`)).toBeVisible();
    };
    await register();

    const page = await open('stale-onboarder@e2e.test');
    await page.goto('/onboard');
    await page.getByLabel('mine', { exact: true }).check();
    await page.getByLabel('Repository').fill('team/proj');

    await admin.locator('.forgejo-row', { hasText: 'mine' }).getByRole('button', { name: 'Delete' }).click();
    await expect(admin.getByText('Deleted mine')).toBeVisible();
    await register();

    await page.getByRole('button', { name: 'Run checks' }).click();
    await expect(page.locator('ul.check-errors')).toContainText('changed since this page loaded');
    for (const radio of await page.getByRole('radio').all()) {
      await expect(radio).not.toBeChecked();
      await expect(radio).toHaveAttribute('required', '');
    }

    await page.getByLabel('Repository').press('Enter');
    await expect(page.locator('ul.check-errors')).toContainText('changed since this page loaded');

    await page.getByLabel('mine', { exact: true }).check();
    await page.getByRole('button', { name: 'Run checks' }).click();
    await expect(page.locator('ul.check-errors')).toContainText('You have no mine token stored');
    await expect(page.getByLabel('mine', { exact: true })).toBeChecked();
  } finally {
    await stub.stop();
    await stop();
  }
});
