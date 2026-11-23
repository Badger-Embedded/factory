// Onboarding (/onboard) use cases, by test name:
//
// - Onboarding a repository whose checks pass, through the fake gh, creates the
//   project, and the project appears in the picker:
//   "onboarding a repository whose checks pass creates the project"
// - A repository whose check fails shows the failure and creates no project:
//   "a repository whose check fails shows the failure and creates no project"
import type { Page } from '@playwright/test';
import { expect, test } from '../fixtures';

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
  await expect(page.locator('#picker-list li')).toHaveCount(1);
  await expect(page.locator('#picker-list li a')).toHaveAttribute('href', `/?project=${name}`);
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
