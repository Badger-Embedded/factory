import type { Page } from '@playwright/test';
import { ADMIN, COLLABORATOR, SHARED_PROJECT, expect, test, type Factory } from '../fixtures';

let counter = 0;

function uniqueLogin(workerIndex: number) {
  return `picker-${workerIndex}-${counter++}@e2e.test`;
}

function projectFor(factory: Factory, login: string, name: string) {
  factory.fixture('project', '--name', name, '--login', login);
}

function job(factory: Factory, project: string, stage: string) {
  return Number(factory.fixture('job', '--project', project, '--stage', stage, '--request', `A ${stage} job`).trim());
}

const button = (page: Page) => page.getByRole('button', { name: 'Project', exact: true });
const row = (page: Page, project: string) => page.locator(`#picker-list li[data-id="${project}"]`);

test.describe('as a login of its own', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('the picker is a button that opens a panel of project links', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `a${tag}`);
    projectFor(factory, login, `b${tag}`);
    job(factory, `b${tag}`, 'spec');

    await page.goto(`/?project=a${tag}`);
    await expect(page.locator('select')).toHaveCount(0);
    await expect(button(page)).toContainText(`a${tag}`);
    await expect(button(page)).toHaveAttribute('aria-expanded', 'false');
    await expect(page.locator('.picker-panel')).toBeHidden();

    await button(page).click();
    await expect(button(page)).toHaveAttribute('aria-expanded', 'true');
    await expect(page.locator('#picker-list li')).toHaveCount(2);
    await expect(page.locator('#picker-list a[aria-current="page"]')).toHaveAttribute('href', `/?project=a${tag}`);

    await page.locator(`#picker-list a[href="/?project=b${tag}"]`).click();
    await expect(page).toHaveURL(new RegExp(`[?&]project=b${tag}$`));
    await expect(button(page)).toContainText(`b${tag}`);

    await page.goto('/');
    await expect(button(page)).toContainText(`b${tag}`);
  });

  test('the row counts equal the board section counts', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    const name = `c${tag}`;
    projectFor(factory, login, name);
    for (const stage of ['questions', 'spec', 'plan', 'reviewed', 'failed', 'failed', 'merged']) {
      job(factory, name, stage);
    }

    await page.goto(`/?project=${name}`);
    const you = page.locator('section.inbox-you h2 .count');
    const failed = page.locator('section.inbox-failed h2 .count');
    await expect(failed).toHaveText('2');
    await page.getByRole('button', { name: 'Dismiss' }).first().click();
    await page.goto(`/?project=${name}`);
    await expect(failed).toHaveText('1');

    await button(page).click();
    const youCount = (await you.textContent())!.trim();
    await expect(row(page, name).locator('.badge.you')).toHaveText(`${youCount} need you`);
    await expect(row(page, name).locator('.badge.bad')).toHaveText('1 failed');
    await expect(row(page, name)).toHaveAttribute('data-attention', 'you');
  });

  test('a running project shows a dot and an idle one shows its name only', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `idle${tag}`);
    projectFor(factory, login, `run${tag}`);
    job(factory, `run${tag}`, 'implementing');

    await page.goto(`/?project=idle${tag}`);
    await button(page).click();
    await expect(row(page, `run${tag}`).locator('.picker-running')).toHaveCount(1);
    await expect(row(page, `run${tag}`).locator('.badge')).toHaveCount(0);
    await expect(row(page, `run${tag}`)).toContainText('running');
    await expect(row(page, `idle${tag}`).locator('.badge, .picker-running')).toHaveCount(0);
  });

  test('the rows read needs you, failed, running, then idle', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    const ids = ['a', 'b', 'c', 'd'].map((letter) => `${letter}-${tag}`);
    for (const id of ids) projectFor(factory, login, id);
    job(factory, ids[1], 'failed');
    job(factory, ids[2], 'spec');
    job(factory, ids[3], 'implementing');

    await page.goto(`/?project=${ids[0]}`);
    await button(page).click();
    await expect(page.locator('#picker-list li')).toHaveCount(4);
    const order = await page.locator('#picker-list li').evaluateAll((items) => items.map((item) => item.getAttribute('data-id')));
    expect(order).toEqual([ids[2], ids[1], ids[3], ids[0]]);
  });

  test('a click outside and Esc close the panel', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `x${tag}`);

    await page.goto(`/?project=x${tag}`);
    await button(page).click();
    await expect(page.locator('.picker-panel')).toBeVisible();
    await page.locator('main').click({ position: { x: 5, y: 300 } });
    await expect(page.locator('.picker-panel')).toBeHidden();
    await expect(button(page)).toHaveAttribute('aria-expanded', 'false');

    await button(page).click();
    await expect(page.locator('.picker-panel')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('.picker-panel')).toBeHidden();
    await expect(button(page)).toBeFocused();
  });

  test('a row updates without a reload while the panel is open', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `here${tag}`);
    projectFor(factory, login, `there${tag}`);

    await page.goto(`/?project=here${tag}`);
    await button(page).click();
    await expect(row(page, `there${tag}`).locator('.badge')).toHaveCount(0);

    job(factory, `there${tag}`, 'spec');
    await expect(row(page, `there${tag}`).locator('.badge.you')).toHaveText('1 need you', { timeout: 15_000 });
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(button(page)).toHaveAttribute('aria-expanded', 'true');
    await expect(page.locator('#picker-list li').first()).toHaveAttribute('data-id', `there${tag}`);
  });

  test('a project the login does not collaborate on has no row', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `mine${tag}`);
    projectFor(factory, ADMIN, `theirs${tag}`);
    job(factory, `theirs${tag}`, 'spec');

    await page.goto(`/?project=mine${tag}`);
    await expect(page.locator('#picker-list li')).toHaveCount(1);
    await expect(row(page, `theirs${tag}`)).toHaveCount(0);
    const response = await page.request.get(`/fragments/picker?project=theirs${tag}`);
    expect(response.status()).toBe(404);
    const own = await page.request.get(`/fragments/picker?project=mine${tag}`);
    expect(own.status()).toBe(200);
    expect(await own.text()).not.toContain(`theirs${tag}`);
  });

  test('a display name shows in the button and the row', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    const name = `named${tag}`;
    projectFor(factory, login, name);

    await page.goto(`/settings/projects/${name}`);
    const card = page.locator('form.settings-form').filter({ hasText: 'Project name' });
    await card.getByLabel('Name', { exact: true }).fill('Pretty Name');
    await card.getByRole('button', { name: 'Save name' }).click();
    await expect(page.getByText('Saved.', { exact: true })).toBeVisible();

    await page.goto(`/?project=${name}`);
    await expect(button(page)).toHaveText('Pretty Name');
    await button(page).click();
    const link = page.locator('#picker-list a[aria-current="page"]');
    await expect(link.locator('.picker-name')).toHaveText('Pretty Name');
    await expect(link).toHaveAttribute('href', `/?project=${name}`);
  });

  test('removing the selected project in another tab keeps the draft and lists the remaining projects', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `gone${tag}`);
    projectFor(factory, login, `stay${tag}`);

    await page.goto(`/?project=gone${tag}`);
    await expect(row(page, `stay${tag}`)).toHaveCount(1);
    const draft = page.locator('textarea').first();
    await draft.fill('half-written request');

    const other = await page.context().newPage();
    await other.goto(`/settings/projects/gone${tag}`);
    await other.getByLabel('Project name').fill(`gone${tag}`);
    await other.getByRole('button', { name: `Remove gone${tag}` }).click();
    await expect(other.getByText(`Removed project gone${tag}.`)).toBeVisible();

    await expect(row(page, `gone${tag}`)).toHaveCount(0, { timeout: 15_000 });
    await expect(row(page, `stay${tag}`)).toHaveCount(1);
    await expect(page).toHaveURL(new RegExp(`project=gone${tag}$`));
    await expect(draft).toHaveValue('half-written request');
    const hidden = await page.request.get(`/fragments/picker?project=gone${tag}`);
    expect(hidden.status()).toBe(404);

    job(factory, `stay${tag}`, 'spec');
    await expect(row(page, `stay${tag}`).locator('.badge.you')).toHaveText(/1/, { timeout: 15_000 });
    await expect(page.locator('#picker-list [aria-current]')).toHaveCount(0);
    await expect(button(page)).toHaveText(`gone${tag}`);
    await expect(draft).toHaveValue('half-written request');
  });

  test('after the selected project is removed the badge counts every remaining project and hides with the last', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    const [gone, a, b] = [`gone${tag}`, `a${tag}`, `b${tag}`];
    for (const name of [gone, a, b]) projectFor(factory, login, name);
    job(factory, gone, 'spec');
    job(factory, a, 'failed');
    job(factory, b, 'spec');

    await page.goto(`/?project=${gone}`);
    const badge = page.locator('#picker-badge');
    await expect(badge).toHaveAttribute('aria-label', '1 need you in 1 other project, 1 failed in 1 other project');

    const other = await page.context().newPage();
    const remove = async (name: string) => {
      await other.goto(`/settings/projects/${name}`);
      await other.getByLabel('Project name').fill(name);
      await other.getByRole('button', { name: `Remove ${name}` }).click();
      await expect(other.getByText(`Removed project ${name}.`)).toBeVisible();
    };
    await remove(gone);

    await expect(row(page, gone)).toHaveCount(0, { timeout: 15_000 });
    await expect(badge).toHaveAttribute('aria-label', '1 need you in 1 other project, 1 failed in 1 other project');
    await expect(page.locator('#picker-list')).toHaveAttribute('hx-get', /^\/fragments\/picker\?v=/);

    await remove(a);
    await expect(badge).toHaveAttribute('aria-label', '1 need you in 1 other project', { timeout: 15_000 });
    await remove(b);
    await expect(row(page, b)).toHaveCount(0, { timeout: 15_000 });
    await expect(badge).toBeHidden();
  });

  test('removing the last visible project leaves an empty list that finds a new project', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `only${tag}`);

    await page.goto(`/?project=only${tag}`);
    const draft = page.locator('textarea').first();
    await draft.fill('half-written request');

    const other = await page.context().newPage();
    await other.goto(`/settings/projects/only${tag}`);
    await other.getByLabel('Project name').fill(`only${tag}`);
    await other.getByRole('button', { name: `Remove only${tag}` }).click();
    await expect(other.getByText(`Removed project only${tag}.`)).toBeVisible();

    await expect(row(page, `only${tag}`)).toHaveCount(0, { timeout: 15_000 });
    await expect(draft).toHaveValue('half-written request');

    projectFor(factory, login, `new${tag}`);
    await expect(row(page, `new${tag}`)).toHaveCount(1, { timeout: 15_000 });
    await expect(page.locator('#picker-list [aria-current]')).toHaveCount(0);
    await expect(draft).toHaveValue('half-written request');
  });

  test('a rename updates the button and keeps focus on the same row while the panel is open', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    projectFor(factory, login, `ren${tag}`);
    projectFor(factory, login, `other${tag}`);

    await page.goto(`/?project=ren${tag}`);
    await button(page).click();
    await expect(page.getByLabel('Find a project')).toBeFocused();
    const link = page.locator(`#picker-list a[href="/?project=other${tag}"]`);
    await link.focus();
    await expect(link).toBeFocused();

    const second = await page.context().newPage();
    await second.goto(`/settings/projects/ren${tag}`);
    const card = second.locator('form.settings-form').filter({ hasText: 'Project name' });
    await card.getByLabel('Name', { exact: true }).fill('Renamed');
    await card.getByRole('button', { name: 'Save name' }).click();
    await expect(second.getByText('Saved.', { exact: true })).toBeVisible();

    await expect(button(page)).toHaveText('Renamed', { timeout: 15_000 });
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(page.locator(`#picker-list a[href="/?project=other${tag}"]`)).toBeFocused();
  });

  test('the button is a named button and the panel fits a short narrow viewport', async ({ page, factory }, testInfo) => {
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.setViewportSize({ width: 390, height: 400 });
    for (let i = 0; i < 30; i++) projectFor(factory, login, `s${String(i).padStart(2, '0')}${tag}`);

    await page.goto(`/?project=s00${tag}`);
    await expect(page.getByRole('button', { name: 'Project', exact: true })).toHaveCount(1);
    await button(page).click();
    const panel = await page.locator('.picker-panel').boundingBox();
    expect(panel!.y + panel!.height).toBeLessThanOrEqual(400);

    await page.setViewportSize({ width: 390, height: 180 });
    await page.reload();
    await button(page).click();
    const short = await page.locator('.picker-panel').boundingBox();
    expect(short!.y + short!.height).toBeLessThanOrEqual(180);
  });
});

for (const { width, height } of [
  { width: 1280, height: 800 },
  { width: 390, height: 844 },
]) {
  test(`50 projects keep the panel inside the viewport at ${width} px`, async ({ page, factory }, testInfo) => {
    test.setTimeout(120_000);
    const login = uniqueLogin(testInfo.workerIndex);
    const tag = login.split('@')[0];
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.setViewportSize({ width, height });
    const names = Array.from({ length: 50 }, (_, i) => `p${String(i).padStart(2, '0')}-${tag}`);
    names[0] = `p00-${'long'.repeat(20)}-${tag}`;
    for (const name of names) projectFor(factory, login, name);
    job(factory, names[7], 'spec');
    job(factory, names[8], 'failed');

    await page.goto(`/?project=${names[20]}`);
    await button(page).click();
    await expect(page.locator('#picker-list li')).toHaveCount(50);

    const panel = await page.locator('.picker-panel').boundingBox();
    expect(panel).not.toBeNull();
    expect(panel!.x).toBeGreaterThanOrEqual(0);
    expect(panel!.x + panel!.width).toBeLessThanOrEqual(width);
    expect(panel!.y + panel!.height).toBeLessThanOrEqual(height);

    const list = page.locator('#picker-list');
    const metrics = await list.evaluate((el) => ({ scroll: el.scrollHeight, client: el.clientHeight }));
    expect(metrics.scroll).toBeGreaterThan(metrics.client);
    const heights = await page
      .locator('#picker-list a')
      .evaluateAll((links) => links.map((link) => link.getBoundingClientRect().height));
    expect(Math.max(...heights)).toBeLessThanOrEqual(Math.min(...heights) + 1);

    const long = page.locator('#picker-list li', { hasText: 'long'.repeat(20) });
    await expect(long.locator('.picker-name')).toHaveAttribute('title', names[0]);
    await page.locator('#picker-list li').nth(0).locator('a').scrollIntoViewIfNeeded();
    const sideways = await page.evaluate(() => ({
      scroll: document.documentElement.scrollWidth,
      client: document.documentElement.clientWidth,
    }));
    expect(sideways.scroll).toBeLessThanOrEqual(sideways.client);
  });
}

test.describe('without JavaScript', () => {
  test.use({ javaScriptEnabled: false });

  test('the panel opens and a row switches the board', async ({ page }) => {
    await page.goto('/');
    await page.getByLabel('Project', { exact: true }).click();
    await page.locator('#picker-list a').first().waitFor();
    await page.locator(`#picker-list a[href="/?project=${SHARED_PROJECT}"]`).click();
    await expect(page).toHaveURL(new RegExp(`[?&]project=${SHARED_PROJECT}$`));
    await expect(page.getByRole('link', { name: 'Support a second USB hub' })).toBeVisible();
  });
});

test.describe('as a collaborator', () => {
  test.use({ extraHTTPHeaders: { 'Tailscale-User-Login': COLLABORATOR } });

  test('the shared project row carries the board counts', async ({ page }) => {
    await page.goto(`/?project=${SHARED_PROJECT}`);
    const you = (await page.locator('section.inbox-you h2 .count').textContent())!.trim();
    const failed = (await page.locator('section.inbox-failed h2 .count').textContent())!.trim();
    await expect(row(page, SHARED_PROJECT).locator('.badge.you')).toHaveText(`${you} need you`);
    await expect(row(page, SHARED_PROJECT).locator('.badge.bad')).toHaveText(`${failed} failed`);
  });
});
