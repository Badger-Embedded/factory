// Narrow screens: the top bar and the pages fit a phone, and the laptop layout holds.
//
// - The board fits a phone screen without sideways scroll, and the brand, the
//   project picker, Onboard and Settings are inside the viewport:
//   "the board fits a 390 px phone", "the board fits a 320 px phone"
// - The login is shortened with an ellipsis and keeps its full value in title:
//   "the login is inside the viewport and keeps its full value in title"
// - A long project name does not push the top bar out of the screen:
//   "a 30 character project name fits the top bar"
// - The top bar stays at the top while the board scrolls:
//   "the top bar stays at the top of the viewport after scrolling"
// - The job page fits a phone and its title is not under the top bar:
//   "the job page fits a phone and its title sits below the top bar"
// - The onboarding and settings pages fit a phone:
//   "$path fits a phone", "/onboard fits a 320 px phone"
// - On a laptop the top bar stays one row with the whole login:
//   "the laptop top bar is one row with the whole login"
import type { Locator, Page } from '@playwright/test';
import { ADMIN, PHONE_USER, expect, test } from '../fixtures';

let counter = 0;

async function noSidewaysScroll(page: Page) {
  const { scroll, client } = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    client: document.documentElement.clientWidth,
  }));
  expect(scroll).toBeLessThanOrEqual(client);
}

async function inViewport(locator: Locator, page: Page) {
  const box = await locator.boundingBox();
  const width = page.viewportSize()!.width;
  expect(box).not.toBeNull();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(width);
}

async function topbarInViewport(page: Page) {
  await inViewport(page.getByRole('link', { name: 'factory' }), page);
  await inViewport(page.getByLabel('Project', { exact: true }), page);
  await inViewport(page.getByRole('link', { name: 'Onboard' }), page);
  await inViewport(page.getByRole('link', { name: 'Settings' }), page);
}

function phoneProject(
  factory: { fixture(...args: string[]): string },
  testInfo: { workerIndex: number },
  name = `n${testInfo.workerIndex}-${counter++}`,
): string {
  factory.fixture('project', '--name', name, '--login', ADMIN, '--login', PHONE_USER);
  return name;
}

function seedJobs(factory: { fixture(...args: string[]): string }, project: string, count = 1) {
  for (const stage of ['spec', 'implementing', 'failed', 'merged']) {
    for (let i = 0; i < count; i++) {
      factory.fixture('job', '--project', project, '--stage', stage, '--request', `A ${stage} job ${i}`);
    }
  }
}

test.describe('phone, 390 px', () => {
  test.use({
    viewport: { width: 390, height: 844 },
    extraHTTPHeaders: { 'Tailscale-User-Login': PHONE_USER },
  });

  test('the board fits a 390 px phone', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    seedJobs(factory, project);
    await page.goto(`/?project=${project}`);

    await noSidewaysScroll(page);
    await topbarInViewport(page);
  });

  test('the other-projects badge fits row one', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    const other = phoneProject(factory, testInfo);
    factory.fixture('job', '--project', other, '--stage', 'spec');
    factory.fixture('job', '--project', other, '--stage', 'failed');
    await page.goto(`/?project=${project}`);

    const badge = page.locator('#picker-badge');
    await expect(badge).toBeVisible();
    await noSidewaysScroll(page);
    await inViewport(badge, page);
    const row = await page.getByRole('link', { name: 'factory' }).boundingBox();
    const box = await badge.boundingBox();
    expect(box!.y).toBeLessThan(row!.y + row!.height);
  });

  test('the login is inside the viewport and keeps its full value in title', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    await page.goto(`/?project=${project}`);

    const user = page.locator('span.user');
    await expect(user).toHaveAttribute('title', PHONE_USER);
    await inViewport(user, page);
  });

  test('a 30 character project name fits the top bar', async ({ page, factory }, testInfo) => {
    const name = `w${testInfo.workerIndex}-${counter++}-`.padEnd(30, 'x');
    expect(name).toHaveLength(30);
    phoneProject(factory, testInfo, name);
    await page.goto(`/?project=${name}`);

    await noSidewaysScroll(page);
    await inViewport(page.getByLabel('Project', { exact: true }), page);
  });

  test('the top bar stays at the top of the viewport after scrolling', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    seedJobs(factory, project, 4);
    await page.goto(`/?project=${project}`);

    await page.evaluate(() => window.scrollTo(0, 600));
    await expect.poll(() => page.evaluate(() => window.scrollY)).toBeGreaterThan(0);
    const box = await page.locator('.topbar').boundingBox();
    expect(box!.y).toBe(0);
  });

  test('the job page fits a phone and its title sits below the top bar', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    const id = factory.fixture('job', '--project', project, '--stage', 'implementing', '--request', 'An implementing job');
    await page.goto(`/jobs/${id}`);

    await noSidewaysScroll(page);
    await inViewport(page.getByRole('link', { name: 'factory' }), page);
    await inViewport(page.getByRole('link', { name: 'Onboard' }), page);
    await inViewport(page.getByRole('link', { name: 'Settings' }), page);
    const bar = await page.locator('.topbar').boundingBox();
    const h1 = await page.getByRole('heading', { level: 1 }).boundingBox();
    expect(h1!.y).toBeGreaterThanOrEqual(bar!.y + bar!.height);
  });

  for (const path of ['/onboard', '/settings/notifications']) {
    test(`${path} fits a phone`, async ({ page }) => {
      await page.goto(path);
      await noSidewaysScroll(page);
    });
  }
});

test.describe('phone, 320 px', () => {
  test.use({
    viewport: { width: 320, height: 568 },
    extraHTTPHeaders: { 'Tailscale-User-Login': PHONE_USER },
  });

  test('/onboard fits a 320 px phone', async ({ page }) => {
    await page.goto('/onboard');
    await noSidewaysScroll(page);
  });

  test('the board fits a 320 px phone', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    seedJobs(factory, project);
    await page.goto(`/?project=${project}`);

    await noSidewaysScroll(page);
    await topbarInViewport(page);
  });
});

test.describe('laptop, 1280 px', () => {
  test.use({
    viewport: { width: 1280, height: 800 },
    extraHTTPHeaders: { 'Tailscale-User-Login': PHONE_USER },
  });

  test('the laptop top bar is one row with the whole login', async ({ page, factory }, testInfo) => {
    const project = phoneProject(factory, testInfo);
    await page.goto(`/?project=${project}`);

    const items = [
      page.getByRole('link', { name: 'factory' }),
      page.getByLabel('Project', { exact: true }),
      page.getByRole('link', { name: 'Onboard' }),
      page.getByRole('link', { name: 'Settings' }),
      page.locator('span.user'),
    ];
    const centres: number[] = [];
    for (const item of items) {
      const box = await item.boundingBox();
      centres.push(box!.y + box!.height / 2);
    }
    expect(Math.max(...centres) - Math.min(...centres)).toBeLessThanOrEqual(2);
    const fit = await page.locator('span.user').evaluate((el) => el.scrollWidth <= el.clientWidth);
    expect(fit).toBe(true);
  });
});
