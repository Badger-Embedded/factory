// Project settings (/settings/projects/{name}) use cases, by test name. The
// project fixture gives each test its own project; the MCP tests run against
// a server of their own that trusts the TLS stub MCP server's CA.
//
// - Pipeline: selecting a stage, a gate or merge shows its panel and updates
//   ?stage= in the address: "selecting a stage, a gate or merge shows its panel
//   and updates the address"
// - Changing a stage's agent redraws its model and effort choices:
//   "changing a stage's agent redraws its model and effort choices"
// - A saved agent, model, effort and gate persist after a reload:
//   "a saved agent, model, effort, gate and merge setting persist after a reload"
// - A changed form shows "Unsaved changes." and leaving the page asks first:
//   "a changed form shows Unsaved changes. and leaving the page asks first"
// - Collaborators: add one and remove one: "a collaborator can be added and removed"
// - The maintenance settings save: "the maintenance settings save"
// - Removing a project takes it off the picker:
//   "removing a project takes it off the picker"
// - Project MCP page:
//   - enable a server, save a header value, run the connection check against
//     the stub and see the result: "MCP: enable a server, save a header value and check the connection"
//   - refresh the tool list: "MCP: a refresh lists the tools the server offers now"
//   - filter the tools, All on and All off, and save tool toggles that persist:
//     "MCP: filter the tools, use All on and All off, and keep the saved toggles"
// - A login that is not a collaborator gets the 404 page for the project, its
//   settings, its jobs and its MCP pages:
//   "a login that is not a collaborator gets the 404 page"
import type { Page } from '@playwright/test';
import { startMcpStub, STUB_TOOLS, type McpStub } from '../mcp-stub';
import {
  ADMIN,
  COLLABORATOR,
  expect,
  startFactory,
  test,
  type Factory,
} from '../fixtures';

const node = (page: Page, part: string) => page.locator(`.pipeline [data-part="${part}"]`);
const panel = (page: Page, part: string) => page.locator(`[data-panel="${part}"]`);
const settingsPath = (project: string, query = '') => `/settings/projects/${project}${query}`;

test('selecting a stage, a gate or merge shows its panel and updates the address', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project));
  await expect(panel(page, 'clarify')).toBeVisible();
  await expect(panel(page, 'spec')).toBeHidden();

  await node(page, 'spec').click();
  await expect(panel(page, 'spec')).toBeVisible();
  await expect(panel(page, 'clarify')).toBeHidden();
  await expect(node(page, 'spec')).toHaveAttribute('aria-current', 'step');
  await expect(page).toHaveURL(new RegExp(`/settings/projects/${project}\\?stage=spec$`));

  await node(page, 'gate-plan').click();
  await expect(panel(page, 'gate-plan')).toBeVisible();
  await expect(panel(page, 'gate-plan').getByRole('heading', { name: 'Approve the plan' })).toBeVisible();
  await expect(panel(page, 'spec')).toBeHidden();
  await expect(page).toHaveURL(/\?stage=gate-plan$/);

  await node(page, 'merge').click();
  await expect(panel(page, 'merge')).toBeVisible();
  await expect(panel(page, 'gate-plan')).toBeHidden();
  await expect(page).toHaveURL(/\?stage=merge$/);

  await page.goto(settingsPath(project, '?stage=implement'));
  await expect(panel(page, 'implement')).toBeVisible();
  await expect(panel(page, 'clarify')).toBeHidden();
});

test("changing a stage's agent redraws its model and effort choices", async ({ page, project }) => {
  await page.goto(settingsPath(project, '?stage=spec'));
  const spec = panel(page, 'spec');
  const chips = spec.locator('.model-chips .chip');
  const efforts = spec.locator('[data-effort-seg] label');
  await expect(chips).toHaveText(['CLI default', 'fable', 'opus', 'sonnet', 'haiku']);
  await expect(efforts).toHaveText(['low', 'medium', 'high', 'xhigh', 'max']);
  await expect(spec.getByRole('radio', { name: 'medium' })).toBeChecked();

  // A model of the old agent is cleared, and the node follows the field.
  await spec.getByRole('button', { name: 'opus' }).click();
  await expect(spec.getByLabel('Model')).toHaveValue('opus');
  await expect(node(page, 'spec').locator('[data-model-label]')).toHaveText('opus');
  await spec.getByRole('radio', { name: 'high', exact: true }).check();

  await spec.getByRole('radio', { name: 'Codex' }).check();
  await expect(chips).toHaveText(['CLI default', 'gpt-6.1-sol', 'gpt-6.1-codex']);
  await expect(efforts).toHaveText(['minimal', 'low', 'medium', 'high', 'xhigh']);
  await expect(spec.getByRole('radio', { name: 'high', exact: true })).toBeChecked();
  await expect(spec.getByLabel('Model')).toHaveValue('');
  await expect(node(page, 'spec').locator('[data-model-label]')).toHaveText('CLI default');
  await expect(spec.locator('[data-model-hint]')).toContainText('installed codex CLI');

  // An effort the next agent lacks falls back to that agent's default.
  await spec.getByRole('radio', { name: 'minimal' }).check();
  await spec.getByRole('radio', { name: 'Claude' }).check();
  await expect(efforts).toHaveText(['low', 'medium', 'high', 'xhigh', 'max']);
  await expect(spec.getByRole('radio', { name: 'medium' })).toBeChecked();
  await expect(chips).toHaveText(['CLI default', 'fable', 'opus', 'sonnet', 'haiku']);
});

test('a saved agent, model, effort, gate and merge setting persist after a reload', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project, '?stage=spec'));
  const spec = panel(page, 'spec');
  await spec.getByRole('radio', { name: 'Codex' }).check();
  await spec.getByRole('button', { name: 'gpt-6.1-sol' }).click();
  await spec.getByRole('radio', { name: 'xhigh' }).check();

  await node(page, 'gate-plan').click();
  const planGate = panel(page, 'gate-plan').getByRole('checkbox', { name: 'The job waits for you here' });
  await expect(planGate).toBeChecked();
  await planGate.uncheck();
  await expect(node(page, 'gate-plan')).toHaveClass(/changed/);
  await expect(panel(page, 'gate-plan').locator('.gate-changed')).toBeVisible();

  await node(page, 'merge').click();
  await panel(page, 'merge').getByRole('checkbox', { name: /Merge pull requests/ }).check();
  await expect(node(page, 'merge').locator('[data-merge-label]')).toHaveText('Automatic');

  await page.getByRole('button', { name: `Save ${project}` }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(panel(page, 'merge')).toBeVisible();

  await page.goto(settingsPath(project, '?stage=spec'));
  await expect(spec.getByRole('radio', { name: 'Codex' })).toBeChecked();
  await expect(spec.getByLabel('Model')).toHaveValue('gpt-6.1-sol');
  await expect(spec.getByRole('radio', { name: 'xhigh' })).toBeChecked();
  await expect(node(page, 'spec').locator('[data-model-label]')).toHaveText('gpt-6.1-sol');
  await expect(node(page, 'gate-plan')).toHaveClass(/changed/);
  await node(page, 'gate-plan').click();
  await expect(planGate).not.toBeChecked();
  await expect(panel(page, 'gate-plan').getByRole('button', { name: 'Use the default' })).toBeVisible();
  await node(page, 'merge').click();
  await expect(panel(page, 'merge').getByRole('checkbox', { name: /Merge pull requests/ })).toBeChecked();
});

test('a changed form shows Unsaved changes. and leaving the page asks first', async ({
  page,
  project,
}) => {
  const dialogs: string[] = [];
  page.on('dialog', (dialog) => {
    dialogs.push(dialog.type());
    void dialog.dismiss();
  });
  await page.goto(settingsPath(project));
  const bar = page.locator('.pipeline-form .save-bar-note');
  await expect(bar).toHaveText('No unsaved changes.');

  // Chromium shows the prompt only after a real input on the page.
  await node(page, 'spec').click();
  await panel(page, 'spec').getByLabel('Model').pressSequentially('opus');
  await expect(bar).toHaveText('Unsaved changes.');

  await page.getByRole('link', { name: 'Settings' }).first().click();
  await expect.poll(() => dialogs).toEqual(['beforeunload']);
  await expect(page).toHaveURL(new RegExp(`/settings/projects/${project}`));

  // A save does not ask, and a saved form is no longer dirty.
  await page.getByRole('button', { name: `Save ${project}` }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(bar).toHaveText('No unsaved changes.');
  await page.getByRole('link', { name: 'Settings' }).first().click();
  await expect(page).not.toHaveURL(new RegExp(project));
  expect(dialogs).toEqual(['beforeunload']);
});

test('a collaborator can be added and removed', async ({ page, project }) => {
  await page.goto(settingsPath(project));
  const row = (login: string) => page.locator('.server-row').filter({ hasText: login });
  await expect(row(ADMIN)).toBeVisible();
  // The only collaborator cannot be removed.
  await expect(row(ADMIN).getByRole('button', { name: 'Remove' })).toHaveCount(0);

  await page.getByLabel('Tailscale login').fill('bob@e2e.test');
  await page.getByRole('button', { name: 'Add collaborator' }).click();
  await expect(page.getByText('Added bob@e2e.test.')).toBeVisible();
  await expect(row('bob@e2e.test')).toContainText(`added by ${ADMIN}`);

  await page.goto(settingsPath(project));
  await expect(row('bob@e2e.test')).toBeVisible();
  await row('bob@e2e.test').getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByText('Removed bob@e2e.test.')).toBeVisible();
  await expect(row('bob@e2e.test')).toHaveCount(0);

  await page.goto(settingsPath(project));
  await expect(row('bob@e2e.test')).toHaveCount(0);
  await expect(row(ADMIN)).toBeVisible();
});

test('the local checks command saves and clears', async ({ page, project }) => {
  await page.goto(settingsPath(project));
  const card = page.locator('form.settings-form').filter({ hasText: 'Local checks' });
  const field = card.getByLabel('Local checks command (optional)');
  await expect(field).toHaveValue('');
  await field.fill('  just lint && just test  ');
  await card.getByRole('button', { name: 'Save checks' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto(settingsPath(project));
  await expect(field).toHaveValue('just lint && just test');
  await field.fill('');
  await card.getByRole('button', { name: 'Save checks' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto(settingsPath(project));
  await expect(field).toHaveValue('');
});

test('the worker packages save, show the unsaved bar and refuse a bad name', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project));
  const card = page.locator('form.settings-form').filter({ hasText: 'Worker packages' });
  const field = card.getByLabel('nixpkgs packages');
  const note = card.locator('.save-bar-note');
  await expect(field).toHaveValue('');
  await expect(note).toHaveText('No unsaved changes.');
  await field.fill('playwright-mcp\npython3Packages.requests');
  await expect(note).toHaveText('Unsaved changes.');
  await card.getByRole('button', { name: 'Save packages' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto(settingsPath(project));
  await expect(field).toHaveValue('playwright-mcp\npython3Packages.requests');

  await field.fill('nixpkgs#foo');
  await card.getByRole('button', { name: 'Save packages' }).click();
  await expect(page.getByText('"nixpkgs#foo" is not a nixpkgs package name')).toBeVisible();

  await page.goto(settingsPath(project));
  await expect(field).toHaveValue('playwright-mcp\npython3Packages.requests');
});

test('a display name shows in the picker, the sidebar and the heading', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project));
  const card = page.locator('form.settings-form').filter({ hasText: 'Project name' });
  const field = card.getByLabel('Name', { exact: true });
  await expect(field).toHaveValue(project);
  await field.fill('  My Project  ');
  await card.getByRole('button', { name: 'Save name' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`${settingsPath(project)}\\?notice=Saved\\.$`));
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('My Project');
  await expect(
    page.locator('.settings-nav').getByRole('link', { name: 'My Project' }),
  ).toHaveAttribute('href', settingsPath(project));

  await page.goto('/');
  await expect(page.getByLabel('Project', { exact: true }).locator('.picker-current')).toHaveText('My Project');
  const current = page.locator('#picker-list a[aria-current="page"]');
  await expect(current.locator('.picker-name')).toHaveText('My Project');
  await expect(current).toHaveAttribute('href', `/?project=${project}`);

  await page.goto(settingsPath(project));
  await field.fill('');
  await card.getByRole('button', { name: 'Save name' }).click();
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(project);
});

const saveProjectToken = async (page: Page, token: string) => {
  const card = page.locator('#forge-account');
  await card.getByLabel('Project token').fill(token);
  await card.getByRole('button', { name: 'Save project token' }).click();
};

test('the maintenance settings save', async ({ page, project }) => {
  await page.goto(settingsPath(project));
  await saveProjectToken(page, 'tok-e2e-alice');
  await expect(page.getByText('Saved. The project token belongs to alice-bot.')).toBeVisible();
  const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  await card.getByLabel('Renovate pull requests').check();
  await card.getByLabel('Packaging project').check();
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  // A packaging project needs a manifest file, and the form comes back with its fields.
  await expect(page.getByText('Give at least one manifest file.')).toBeVisible();
  await expect(card.getByLabel('Manifest files')).toBeVisible();

  await card.getByLabel('Type').selectOption('deb');
  await card.getByLabel('Manifest files').fill('debian/control\ndebian/rules');
  await card.getByLabel('Build config files').fill('packaging/build.yaml');
  await card.getByLabel('Upstream repository').fill('e2e/upstream');
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto(settingsPath(project));
  await expect(card.getByLabel('Renovate pull requests')).toBeChecked();
  await expect(card.getByLabel('Packaging project')).toBeChecked();
  await expect(card.getByLabel('Type')).toHaveValue('deb');
  await expect(card.getByLabel('Manifest files')).toHaveValue('debian/control\ndebian/rules');
  await expect(card.getByLabel('Build config files')).toHaveValue('packaging/build.yaml');
  await expect(card.getByLabel('Upstream repository')).toHaveValue('e2e/upstream');
});

test('the upstream forge is checked and an unreadable upstream is refused', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project));
  await saveProjectToken(page, 'tok-e2e-alice');
  const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  await card.getByLabel('Renovate pull requests').check();
  await card.getByLabel('Packaging project').check();
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await card.getByLabel('Type').selectOption('deb');
  await card.getByLabel('Manifest files').fill('debian/control');
  await expect(card.getByLabel('Upstream forge')).toHaveValue('github');

  await card.getByLabel('Upstream repository').fill('e2e/missing');
  await card.getByRole('button', { name: 'Check connection' }).click();
  await expect(page.getByText('e2e/missing could not be read on GitHub')).toBeVisible();
  // The check redraws the form as unchanged, and Save waits for an edit.
  await card.getByLabel('Manifest files').fill('debian/control\ndebian/rules');
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await expect(page.getByText('Nothing was saved.')).toBeVisible();

  await card.getByLabel('Upstream repository').fill('e2e/upstream');
  await card.getByRole('button', { name: 'Check connection' }).click();
  await expect(
    page.getByText('Checked with the project token of alice-bot: e2e/upstream is readable on GitHub.'),
  ).toBeVisible();
});

test('a private upstream on another forge is configured from scratch with its own token', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project));
  await saveProjectToken(page, 'tok-e2e-alice');
  const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  await card.getByLabel('Renovate pull requests').check();
  await card.getByLabel('Packaging project').check();
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await card.getByLabel('Type').selectOption('deb');
  await card.getByLabel('Manifest files').fill('debian/control');
  await card.getByLabel('Upstream forge').selectOption('salsa');
  await card.getByLabel('Upstream repository').fill('e2e/sub/private');
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await expect(page.getByRole('status')).toContainText('e2e/sub/private could not be read on Salsa');
  await expect(card.locator('#upstream-token-salsa')).toContainText('No Salsa token is stored.');

  // The token saves for the forge chosen in the form, which is not saved yet.
  await card.getByLabel('Salsa upstream token').fill('tok-e2e-deb');
  await card.getByRole('button', { name: 'Save Salsa upstream token' }).click();
  await expect(page.getByText('deb-bot', { exact: true })).toBeVisible();
  await expect(card.getByLabel('Upstream forge')).toHaveValue('salsa');
  await expect(card.getByLabel('Upstream repository')).toHaveValue('e2e/sub/private');
  await expect(card.getByLabel('Salsa upstream token')).toHaveValue('');

  await card.getByLabel('Manifest files').fill('debian/control\ndebian/rules');
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(card.getByLabel('Upstream forge')).toHaveValue('salsa');
  await card.getByLabel('Upstream repository').fill('e2e/sub/private');
  await card.getByRole('button', { name: 'Check connection' }).click();
  await expect(
    page.getByText('Checked with the upstream token of deb-bot: e2e/sub/private is readable on Salsa.'),
  ).toBeVisible();
});

test('an upstream token button acts on the forge it sits beside', async ({ page, project }) => {
  await page.goto(settingsPath(project));
  await saveProjectToken(page, 'tok-e2e-alice');
  const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  await card.getByLabel('Renovate pull requests').check();
  await card.getByLabel('Packaging project').check();
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await card.getByLabel('Manifest files').fill('debian/control');
  await card.getByLabel('Upstream forge').selectOption('salsa');
  await card.getByLabel('Salsa upstream token').fill('tok-e2e-deb');
  await card.getByRole('button', { name: 'Save Salsa upstream token' }).click();
  await expect(card.locator('#upstream-token-salsa')).toContainText('deb-bot');

  // Switching the select moves no status and no button target. Clearing GitHub's
  // token, which is not stored, leaves the Salsa token and the project token alone.
  await card.getByLabel('Upstream forge').selectOption('github');
  await expect(card.locator('#upstream-token-salsa')).toContainText('deb-bot');
  await card.getByRole('button', { name: 'Clear GitHub upstream token' }).click();
  await expect(page.getByText('Cleared.')).toBeVisible();
  await expect(card.locator('#upstream-token-salsa')).toContainText('deb-bot');
  await expect(page.locator('#forge-account')).toContainText('alice-bot');

  await card.getByRole('button', { name: 'Clear Salsa upstream token' }).click();
  await expect(card.locator('#upstream-token-salsa')).toContainText('No Salsa token is stored.');
  await expect(page.locator('#forge-account')).toContainText('alice-bot');
});

test('turning Renovate on checks the project account', async ({ page, project }) => {
  await page.goto(settingsPath(project));
  const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  const save = async () => {
    await card.getByLabel('Renovate pull requests').check();
    await card.getByRole('button', { name: 'Save maintenance' }).click();
  };

  await save();
  await expect(page.getByText('Renovate pull requests need a project token.')).toBeVisible();

  await saveProjectToken(page, 'tok-e2e-bob');
  await save();
  await expect(page.getByText('bob-bot has only Read access')).toBeVisible();
  await expect(page.getByText('cannot rerun failed CI')).toBeVisible();

  await saveProjectToken(page, 'tok-e2e-alice');
  await save();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(page.getByText('Read access')).toHaveCount(0);
});

test('a Salsa project asks for its Renovate account and a GitHub project does not', async ({
  page,
  project,
}, testInfo) => {
  await page.goto(settingsPath(project));
  const github = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  await expect(github.getByLabel('Renovate pull requests')).toBeVisible();
  await expect(github.getByLabel('Renovate account')).toHaveCount(0);

  const login = `renovate-owner-${testInfo.workerIndex}-${Date.now()}`;
  const name = `salsa-renovate-${testInfo.workerIndex}-${Date.now()}`;
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
  await page.goto('/settings/forges/salsa');
  await page.getByLabel('Token').fill('tok-e2e-deb');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('Saved. The token belongs to deb-bot.')).toBeVisible();
  await page.goto('/onboard');
  await page.getByLabel('Salsa').check();
  await page.getByLabel('Repository').fill('e2e/sub/renovate');
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByRole('button', { name: 'Run checks' }).click();
  await page.getByRole('button', { name: 'Skip extras and create' }).click();

  await page.goto(settingsPath(name));
  await saveProjectToken(page, 'tok-e2e-deb');
  const card = page.locator('form.settings-form').filter({ hasText: 'Maintenance' });
  await card.getByLabel('Renovate pull requests').check();
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await expect(page.getByText('Give the Renovate account')).toBeVisible();

  await card.getByLabel('Renovate account').fill('renovate-bot');
  await card.getByRole('button', { name: 'Save maintenance' }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await page.goto(settingsPath(name));
  await expect(card.getByLabel('Renovate account')).toHaveValue('renovate-bot');
});

test('the Forge account card saves a project token by its access and keeps the switch per login', async ({
  page,
  project,
}) => {
  await page.goto(settingsPath(project));
  const card = page.locator('#forge-account');
  await expect(page.getByText('has no project token')).toBeVisible();
  await expect(card.getByRole('link', { name: `e2e/${project}` })).toHaveAttribute(
    'href',
    `https://github.com/e2e/${project}`,
  );
  await expect(card.getByRole('link', { name: `e2e/${project}` })).toHaveAttribute(
    'target',
    '_blank',
  );

  const save = async (token: string) => {
    await card.getByLabel('Project token').fill(token);
    await card.getByRole('button', { name: 'Save project token' }).click();
  };
  await save('tok-e2e-ghost');
  await expect(page.getByText(`ghost-bot cannot see e2e/${project}`)).toBeVisible();
  await expect(page.getByText('has no project token')).toBeVisible();

  await save('tok-e2e-bob');
  await expect(page.getByText('only Read access')).toBeVisible();
  await expect(card.locator('.forge-account')).toHaveText('bob-bot');

  await save('tok-e2e-alice');
  await expect(page.getByText('Saved. The project token belongs to alice-bot.')).toBeVisible();
  expect(await page.content()).not.toContain('tok-e2e-alice');
  await expect(page.getByText('has no project token')).toHaveCount(0);
  await page.goto(`/?project=${project}`);
  await expect(page.getByText('has no project token')).toHaveCount(0);

  await page.goto(settingsPath(project));
  const mine = card.getByLabel('Use my account');
  await expect(mine).not.toBeChecked();
  await mine.check();
  await card.getByRole('button', { name: 'Save switch' }).click();
  await expect(card.getByText('You have no GitHub token')).toBeVisible();
  await expect(card.getByRole('link', { name: 'Save your GitHub token' })).toHaveAttribute(
    'href',
    '/settings/forges/github',
  );
  await expect(card.getByLabel('Use my account')).toBeChecked();

  await card.getByRole('button', { name: 'Clear project token' }).click();
  await expect(page.getByText('has no project token')).toBeVisible();
});

test('removing a project takes it off the picker', async ({ page, project }) => {
  await page.goto(`/?project=${project}`);
  await expect(page.getByLabel('Project', { exact: true }).locator('.picker-current')).toHaveText(project);
  await expect(page.locator(`#picker-list li[data-id="${project}"]`)).toHaveCount(1);

  await page.goto(settingsPath(project));
  // The name must match exactly, so a wrong one removes nothing.
  await page.getByLabel('Project name').fill(`${project}x`);
  await page.getByRole('button', { name: `Remove ${project}` }).click();
  await expect(page.getByText(`Nothing was removed. Type ${project} to remove the project.`)).toBeVisible();

  await page.getByLabel('Project name').fill(project);
  await page.getByRole('button', { name: `Remove ${project}` }).click();
  await expect(page.getByText(`Removed project ${project}.`)).toBeVisible();

  await page.goto('/');
  await expect(page.locator(`#picker-list li[data-id="${project}"]`)).toHaveCount(0);
  const response = await page.goto(settingsPath(project));
  expect(response?.status()).toBe(404);
});

test('a login that is not a collaborator gets the 404 page', async ({ page, project, factory }) => {
  const job = factory.fixture('job', '--project', project, '--stage', 'requested');
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  for (const path of [`/?project=${project}`, settingsPath(project), settingsPath(project, '?stage=spec')]) {
    expect((await page.goto(path))?.status(), path).toBe(404);
    await expect(page.getByText(`no project ${project}`)).toBeVisible();
  }

  const jobPage = await page.goto(`/jobs/${job}`);
  expect(jobPage?.status()).toBe(404);
  await expect(page.getByText(`no job ${job}`)).toBeVisible();

  // An admin who collaborates on the project still sees it.
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': ADMIN });
  expect((await page.goto(settingsPath(project)))?.status()).toBe(200);
  expect((await page.goto(`/jobs/${job}`))?.status()).toBe(200);
});

// The project MCP page talks to a stub MCP server over TLS, which the
// factory trusts through FACTORY_MCP_CA_FILE. Each test has its own project on
// a server of the file's own, since a registered MCP server is factory-wide.
const SERVER = 'docs';
const HEADER = 'X-Key';
const SECRET = 'secret-e2e-value';

interface McpEnv {
  factory: Factory;
  stub: McpStub;
}

let mcpCounter = 0;

const mcpTest = test.extend<{ mcpProject: string }, { mcp: McpEnv }>({
  mcp: [
    async ({}, use) => {
      const { factory, stop } = await startFactory();
      try {
        const stub = await startMcpStub(factory);
        try {
          const added = await fetch(`${factory.url}/settings/mcp`, {
            method: 'POST',
            headers: { 'Tailscale-User-Login': ADMIN, 'content-type': 'application/x-www-form-urlencoded' },
            body: new URLSearchParams({ name: SERVER, url: stub.url, header: HEADER }),
            redirect: 'manual',
          });
          expect(added.status).toBeLessThan(400);
          await use({ factory, stub });
        } finally {
          await stub.stop();
        }
      } finally {
        await stop();
      }
    },
    { scope: 'worker' },
  ],
  baseURL: async ({ mcp }, use) => {
    await use(mcp.factory.url);
  },
  mcpProject: async ({ mcp }, use, testInfo) => {
    const name = `m${testInfo.workerIndex}-${mcpCounter++}`;
    mcp.factory.fixture('project', '--name', name, '--login', ADMIN);
    await use(name);
  },
});

const mcpPath = (project: string) => `/settings/projects/${project}/mcp/${SERVER}`;
const toolBox = (page: Page, tool: string) =>
  page.locator('.mcp-tool').filter({ has: page.locator(`code:text-is("${tool}")`) }).getByRole('checkbox');

mcpTest(
  'MCP: enable a server, save a header value and check the connection',
  async ({ page, mcp, mcpProject }) => {
    await page.goto(settingsPath(mcpProject));
    await expect(page.locator('.server-row').filter({ hasText: SERVER })).toContainText('Off');
    await page.getByRole('link', { name: 'Configure' }).click();
    await expect(page).toHaveURL(mcpPath(mcpProject));
    await expect(page.getByRole('heading', { name: `${SERVER} for ${mcpProject}` })).toBeVisible();

    await page.getByLabel(`Run this server for ${mcpProject}`).check();
    await page.getByRole('checkbox', { name: 'spec', exact: true }).check();
    await page.getByRole('checkbox', { name: 'plan', exact: true }).check();
    await expect(page.getByLabel(HEADER, { exact: true })).toHaveAttribute('placeholder', 'not set');
    await page.getByLabel(HEADER, { exact: true }).fill(SECRET);
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

    await page.goto(mcpPath(mcpProject));
    await expect(page.getByLabel(`Run this server for ${mcpProject}`)).toBeChecked();
    await expect(page.getByRole('checkbox', { name: 'spec', exact: true })).toBeChecked();
    await expect(page.getByRole('checkbox', { name: 'clarify', exact: true })).not.toBeChecked();
    await expect(page.getByLabel(HEADER, { exact: true })).toHaveAttribute('placeholder', 'set');
    await expect(page.getByText('A value is stored.')).toBeVisible();

    await page.getByRole('button', { name: 'Refresh connection' }).click();
    await expect(page.locator('.mcp-check-result')).toContainText('Connected: e2e-stub 1.2.3, protocol 2025-03-26.');
    await expect(page.locator('.mcp-check-result')).toContainText(`Listed ${STUB_TOOLS.length} tools.`);
    expect(mcp.stub.seen(HEADER)).toContain(SECRET);

    await page.goto(settingsPath(mcpProject));
    await expect(page.getByText(`On in 2 of 5 stages · ${STUB_TOOLS.length} of ${STUB_TOOLS.length} tools on`)).toBeVisible();
    await expect(page.getByText('Key stored')).toBeVisible();
  },
);

mcpTest('MCP: a refresh lists the tools the server offers now', async ({ page, mcp, mcpProject }) => {
  await page.goto(mcpPath(mcpProject));
  await expect(page.getByText('No tools listed yet.')).toBeVisible();
  await page.getByRole('button', { name: 'Refresh connection' }).click();
  await expect(page.locator('.mcp-tool')).toHaveCount(STUB_TOOLS.length);
  await expect(page.locator('.mcp-tool-count')).toHaveText(`${STUB_TOOLS.length} of ${STUB_TOOLS.length} on`);

  mcp.stub.setTools([...STUB_TOOLS.slice(1), { name: 'brand_new', description: 'Appeared later' }]);
  try {
    await page.getByRole('button', { name: 'Refresh connection' }).click();
    await expect(toolBox(page, 'brand_new')).toBeVisible();
    await expect(page.locator('.mcp-tool').filter({ hasText: 'read_file' })).toContainText('no longer offered');
    await expect(page.locator('.mcp-tool').filter({ hasText: 'brand_new' })).toContainText('Appeared later');
    await expect(page.locator('.mcp-tool').filter({ hasText: 'brand_new' }).locator('.badge')).toHaveText('new');
  } finally {
    mcp.stub.setTools(STUB_TOOLS);
  }
});

mcpTest(
  'MCP: filter the tools, use All on and All off, and keep the saved toggles',
  async ({ page, mcpProject }) => {
    await page.goto(mcpPath(mcpProject));
    await page.getByLabel(`Run this server for ${mcpProject}`).check();
    await page.getByRole('checkbox', { name: 'spec', exact: true }).check();
    await page.getByRole('button', { name: 'Refresh connection' }).click();
    const tools = page.locator('.mcp-tool');
    await expect(tools).toHaveCount(STUB_TOOLS.length);
    const count = page.locator('.mcp-tool-count');
    const filter = page.getByPlaceholder(`Filter ${STUB_TOOLS.length} tools`);

    await filter.fill('file');
    await expect(page.locator('.mcp-tool:visible')).toHaveCount(2);
    await expect(toolBox(page, 'read_file')).toBeVisible();
    await expect(toolBox(page, 'list_dir')).toBeHidden();

    // The buttons act on the tools the filter shows.
    await page.getByRole('button', { name: 'All off' }).click();
    await expect(count).toHaveText(`${STUB_TOOLS.length - 2} of ${STUB_TOOLS.length} on`);
    await filter.fill('');
    await expect(page.locator('.mcp-tool:visible')).toHaveCount(STUB_TOOLS.length);
    await page.getByRole('button', { name: 'All off' }).click();
    await expect(count).toHaveText(`0 of ${STUB_TOOLS.length} on`);
    await page.getByRole('button', { name: 'All on' }).click();
    await expect(count).toHaveText(`${STUB_TOOLS.length} of ${STUB_TOOLS.length} on`);

    await toolBox(page, 'write_file').uncheck();
    await toolBox(page, 'fetch_page').uncheck();
    await expect(count).toHaveText(`${STUB_TOOLS.length - 2} of ${STUB_TOOLS.length} on`);
    await expect(page.locator('form.settings-form .save-bar-note')).toHaveText('Unsaved changes.');
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

    await page.goto(mcpPath(mcpProject));
    await expect(count).toHaveText(`${STUB_TOOLS.length - 2} of ${STUB_TOOLS.length} on`);
    await expect(toolBox(page, 'write_file')).not.toBeChecked();
    await expect(toolBox(page, 'fetch_page')).not.toBeChecked();
    await expect(toolBox(page, 'read_file')).toBeChecked();
    await expect(toolBox(page, 'list_dir')).toBeChecked();
    await expect(toolBox(page, 'search_docs')).toBeChecked();
  },
);

mcpTest('a login that is not a collaborator gets the 404 page for a project MCP page', async ({
  page,
  mcpProject,
}) => {
  await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': COLLABORATOR });
  const response = await page.goto(mcpPath(mcpProject));
  expect(response?.status()).toBe(404);
  await expect(page.getByText(`no project ${mcpProject}`)).toBeVisible();
});

test('a stdio MCP server takes extra arguments, keeps them when off and shows in the pipeline', async ({
  page,
  project,
}) => {
  const server = `pw-${Math.random().toString(36).slice(2, 8)}`;
  const added = await page.request.post('/settings/mcp', {
    form: { name: server, kind: 'stdio', command: 'playwright-mcp', arg: '--no-sandbox' },
    maxRedirects: 0,
  });
  expect(added.status()).toBeLessThan(400);
  const path = `/settings/projects/${project}/mcp/${server}`;
  const enable = page.getByLabel(`Run this server for ${project}`);
  const args = page.getByLabel('Argument');
  const line = page.locator('.mcp-command-line');

  await page.goto(settingsPath(project));
  await expect(page.locator('.server-row').filter({ hasText: server })).toContainText('Off');
  await page.getByRole('link', { name: 'Configure' }).click();
  await expect(page).toHaveURL(path);
  await expect(page.getByRole('button', { name: 'Refresh connection' })).toHaveCount(0);
  await expect(page.locator('.mcp-tool')).toHaveCount(0);
  await expect(page.getByText('so the factory cannot check its connection')).toBeVisible();
  await expect(line).toHaveText('playwright-mcp --no-sandbox');

  await enable.check();
  await page.getByRole('checkbox', { name: 'implement', exact: true }).check();
  await args.nth(0).fill('--headless');
  await page.getByRole('button', { name: 'Add argument' }).click();
  await args.last().fill('--viewport-size=1280x720');
  await args.nth(1).fill('--junk');
  await page.getByRole('button', { name: 'Remove' }).nth(1).click();
  await page.getByRole('button', { name: 'Add argument' }).click();
  await args.last().fill('a  b');
  await expect(page.getByText('Unsaved changes.')).toBeVisible();
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

  await page.goto(path);
  await expect(args.nth(0)).toHaveValue('--headless');
  await expect(args.nth(1)).toHaveValue('--viewport-size=1280x720');
  await expect(line).toHaveText("playwright-mcp --no-sandbox --headless --viewport-size=1280x720 'a  b'");
  expect(await line.evaluate((el) => getComputedStyle(el).whiteSpace)).toBe('pre-wrap');

  await enable.uncheck();
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await enable.check();
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await expect(args.nth(1)).toHaveValue('--viewport-size=1280x720');

  await page.goto(settingsPath(project, '?stage=spec'));
  const specBox = panel(page, 'spec').getByRole('checkbox', { name: server });
  await specBox.check();
  await page.getByRole('button', { name: `Save ${project}` }).click();
  await expect(page.getByText('Saved.', { exact: true })).toBeVisible();
  await page.goto(path);
  await expect(page.getByRole('checkbox', { name: 'spec', exact: true })).toBeChecked();
  await expect(args.nth(1)).toHaveValue('--viewport-size=1280x720');
});

let navSeeded = false;
const seedNav = (factory: Factory) => {
  if (navSeeded) return;
  navSeeded = true;
  for (let i = 0; i < 12; i++) factory.fixture('project', '--name', `nav${i}-${Date.now() % 100000}`, '--login', ADMIN);
};
const navBox = (page: Page) =>
  page.locator('nav.settings-nav').evaluate((nav) => {
    const r = nav.getBoundingClientRect();
    return {
      top: r.top,
      bottom: r.bottom,
      height: r.height,
      scrollHeight: nav.scrollHeight,
      clientHeight: nav.clientHeight,
      scrollWidth: nav.scrollWidth,
      clientWidth: nav.clientWidth,
      scrollTop: nav.scrollTop,
    };
  });

test('the settings sidebar fills a short window and follows its height without a reload', async ({ page, factory, project }) => {
  seedNav(factory);
  await page.setViewportSize({ width: 1280, height: 500 });
  await page.goto(settingsPath(project));
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
  const short = await navBox(page);
  expect(short.top).toBeGreaterThanOrEqual(56);
  expect(Math.abs(short.bottom - (500 - 16))).toBeLessThanOrEqual(1);
  expect(short.scrollHeight).toBeGreaterThan(short.clientHeight);
  expect(short.scrollWidth).toBeLessThanOrEqual(short.clientWidth);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);

  await page.setViewportSize({ width: 1280, height: 800 });
  await expect.poll(async () => (await navBox(page)).bottom).toBeLessThanOrEqual(800);
  const tall = await navBox(page);
  expect(await page.evaluate(() => window.scrollY)).toBe(0);
  const fits = tall.scrollHeight === tall.clientHeight;
  if (fits) expect(tall.height).toBeCloseTo(tall.scrollHeight + 2, 0);
  else {
    expect(Math.abs(tall.bottom - (800 - 16))).toBeLessThanOrEqual(1);
    expect(Math.abs(tall.height - short.height - 300)).toBeLessThanOrEqual(2);
  }
});

test('the settings sidebar scrolls without moving the window and stays pinned', async ({ page, factory, project }) => {
  seedNav(factory);
  await page.setViewportSize({ width: 1280, height: 500 });
  await page.goto(settingsPath(project));
  const add = page.getByRole('link', { name: 'Onboard a project' });
  await page.locator('nav.settings-nav').hover();
  await page.mouse.wheel(0, 5000);
  await expect.poll(async () => (await navBox(page)).scrollTop).toBeGreaterThan(0);
  await expect(add).toBeInViewport();
  expect(await page.evaluate(() => window.scrollY)).toBe(0);

  await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  const after = await navBox(page);
  expect(after.top).toBeGreaterThanOrEqual(56);
  expect(after.bottom).toBeLessThanOrEqual(500);
  await expect(add).toBeInViewport();
});

test('the settings sidebar shows the open project after a load', async ({ page, factory }) => {
  seedNav(factory);
  await page.setViewportSize({ width: 1280, height: 500 });
  await page.goto('/settings/polling');
  const href = await page
    .locator('nav.settings-nav a[href^="/settings/projects/"]')
    .last()
    .getAttribute('href');
  await page.goto(href!);
  const inside = await page.evaluate(() => {
    const nav = document.querySelector('nav.settings-nav')!.getBoundingClientRect();
    const link = document.querySelector('nav.settings-nav [aria-current="page"]')!.getBoundingClientRect();
    return { in: link.top >= nav.top && link.bottom <= nav.bottom, scrollY: window.scrollY };
  });
  expect(inside).toEqual({ in: true, scrollY: 0 });
});

test('a tabbed link in the settings sidebar scrolls into view with its whole outline', async ({ page, factory, project }) => {
  seedNav(factory);
  await page.setViewportSize({ width: 1280, height: 500 });
  await page.goto('/settings/polling');
  const outlineOf = () =>
    page.evaluate(() => {
      const el = document.activeElement!;
      const nav = document.querySelector('nav.settings-nav')!.getBoundingClientRect();
      const link = el.getBoundingClientRect();
      const style = getComputedStyle(el);
      const offset = parseFloat(style.outlineOffset);
      const width = parseFloat(style.outlineWidth);
      const grow = Math.max(offset, 0) + width;
      return {
        drawn: style.outlineStyle !== 'none' && width > 0,
        inside:
          link.top - grow >= nav.top &&
          link.bottom + grow <= nav.bottom &&
          link.left - grow >= nav.left &&
          link.right + grow <= nav.right,
      };
    });
  await page.keyboard.press('Tab');
  while (!(await page.evaluate(() => !!document.activeElement?.closest('nav.settings-nav')))) {
    await page.keyboard.press('Tab');
  }
  expect(await outlineOf()).toEqual({ drawn: true, inside: true });
  const add = page.getByRole('link', { name: 'Onboard a project' });
  for (let i = 0; i < 60 && !(await add.evaluate((el) => el === document.activeElement)); i++) {
    await page.keyboard.press('Tab');
  }
  await expect(add).toBeFocused();
  expect(await outlineOf()).toEqual({ drawn: true, inside: true });
  expect(await page.locator('nav.settings-nav').evaluate((nav) => nav.scrollTop)).toBeGreaterThan(0);
});

test("the settings sidebar keeps its content height on a large window and is a plain row when narrow", async ({ browser }) => {
  const { factory, stop } = await startFactory();
  try {
    const context = await browser.newContext({
      baseURL: factory.url,
      extraHTTPHeaders: { 'Tailscale-User-Login': ADMIN },
      viewport: { width: 1920, height: 1400 },
    });
    const big = await context.newPage();
    await big.goto('/settings/polling');
    const fit = await navBox(big);
    expect(fit.scrollHeight).toBe(fit.clientHeight);
    expect(await big.locator('nav.settings-nav').evaluate((n) => n.scrollHeight <= n.clientHeight)).toBe(true);

    await big.setViewportSize({ width: 700, height: 500 });
    const narrow = await big.locator('nav.settings-nav').evaluate((n) => {
      const s = getComputedStyle(n);
      return { position: s.position, maxHeight: s.maxHeight, scrollsSideways: n.scrollWidth > n.clientWidth || s.overflowX === 'auto' };
    });
    expect(narrow).toEqual({ position: 'static', maxHeight: 'none', scrollsSideways: true });
    await context.close();
  } finally {
    await stop();
  }
});
