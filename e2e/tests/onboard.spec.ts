// Onboarding (/onboard) use cases, by test name:
//
// - Onboarding a repository whose checks pass, through the fake gh, creates the
//   project, and the project appears in the picker:
//   "onboarding a repository whose checks pass creates the project"
// - A repository whose check fails shows the failure and creates no project:
//   "a repository whose check fails shows the failure and creates no project"
import { expect, test } from '../fixtures';

let counter = 0;

function uniqueLogin(workerIndex: number) {
  return `onboard-${workerIndex}-${counter++}@e2e.test`;
}

test('onboarding a repository whose checks pass creates the project', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  const name = `onboarded-${testInfo.workerIndex}-${counter}`;
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await page.goto('/onboard');
  await page.getByLabel('Repository').fill('e2e/good');
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByRole('button', { name: 'Run checks' }).click();

  await expect(page.getByText('All required checks passed.')).toBeVisible();
  await page.getByRole('button', { name: 'Skip extras and create' }).click();

  await expect(page).toHaveURL(new RegExp(`/\\?project=${name}$`));
  await expect(page.getByLabel('Project')).toHaveValue(name);
  await expect(page.getByLabel('Project').locator('option')).toHaveText([name]);
});

test('a repository whose check fails shows the failure and creates no project', async ({ page }, testInfo) => {
  const login = uniqueLogin(testInfo.workerIndex);
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });

  await page.goto('/onboard');
  await page.getByLabel('Repository').fill('e2e/missing');
  await page.getByRole('button', { name: 'Run checks' }).click();

  await expect(page.locator('ul.check-errors')).toContainText(
    'The repository e2e/missing does not exist or is not visible to the bot.',
  );
  await expect(page.getByRole('button', { name: 'Create project' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Skip extras and create' })).toHaveCount(0);

  await page.goto('/');
  await expect(page.getByText('You are not a collaborator on any project.')).toBeVisible();
});
