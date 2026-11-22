// Notifications use cases, by test name:
//
// - With permission granted and notifications on, a job that starts to need the
//   user raises one browser notification, observed through a stub of
//   `Notification`: "a job that starts to need you raises one notification"
// - The permission bar shows while permission is `default` and hides once it is
//   granted: "the permission bar shows while permission is default and hides
//   once granted"
// - The notifications settings page saves the per-login toggle and shows the
//   browser's permission state: "the notifications page saves the toggle and
//   shows the permission state"
import type { Page } from '@playwright/test';
import { COLLABORATOR, expect, test } from '../fixtures';

declare global {
  interface Window {
    __notes: { title: string; body?: string }[];
  }
}

// Replaces `Notification` before the page's scripts run, so the tests do not
// depend on how headless Chromium answers a permission prompt.
async function stubNotification(page: Page, permission: 'default' | 'granted' | 'denied') {
  await page.addInitScript((initial) => {
    window.__notes = [];
    class FakeNotification {
      static permission = initial;
      static requestPermission() {
        FakeNotification.permission = 'granted';
        return Promise.resolve('granted');
      }
      onclick: (() => void) | null = null;
      constructor(title: string, options?: { body?: string }) {
        window.__notes.push({ title, body: options?.body });
      }
      close() {}
    }
    Object.defineProperty(window, 'Notification', { value: FakeNotification, configurable: true });
  }, permission);
}

const polled = (page: Page) =>
  page.waitForResponse((response) => new URL(response.url()).pathname === '/notifications');

test('a job that starts to need you raises one notification', async ({ page, factory, project }) => {
  const enabled = await page.request.post('/settings/notifications', {
    form: { notifications: 'on' },
  });
  expect(enabled.ok()).toBe(true);
  const id = factory.fixture(
    'job',
    '--project',
    project,
    '--stage',
    'started',
    '--request',
    'A job the agent is working on',
  );
  await stubNotification(page, 'granted');

  // The first poll only records what already needs the user.
  const first = polled(page);
  await page.goto('/');
  await first;
  expect(await page.evaluate(() => window.__notes.length)).toBe(0);

  factory.fixture('apply', '--job', id, '--event', 'questions');

  await expect.poll(() => page.evaluate(() => window.__notes.map((note) => note.title))).toEqual([
    `Job #${id} needs you`,
  ]);

  // Another poll sees the same job and does not notify again.
  await polled(page);
  expect(await page.evaluate(() => window.__notes.length)).toBe(1);
});

test('the permission bar shows while permission is default and hides once granted', async ({
  page,
}) => {
  await stubNotification(page, 'default');
  await page.goto('/');

  const bar = page.locator('#permission-bar');
  await expect(bar).toBeVisible();
  await expect(bar.getByRole('link', { name: 'Notification settings' })).toHaveAttribute(
    'href',
    '/settings/notifications',
  );

  await bar.getByRole('button', { name: 'Allow' }).click();
  await expect(bar).toBeHidden();
});

test('the permission bar stays hidden when permission is already granted', async ({ page }) => {
  await stubNotification(page, 'granted');
  await page.goto('/');

  const bar = page.locator('#permission-bar');
  await expect(bar).toBeAttached();
  await expect(bar).toBeHidden();
});

test.describe('the notifications page', () => {
  test.use({ extraHTTPHeaders: { 'Tailscale-User-Login': COLLABORATOR } });

  test('the notifications page saves the toggle and shows the permission state', async ({
    page,
  }) => {
    await stubNotification(page, 'default');
    await page.goto('/settings/notifications');

    const toggle = page.getByLabel('Browser notifications for my jobs');
    const permission = page.locator('#notification-permission');
    // Notifications are on until a login turns them off.
    await expect(toggle).toBeChecked();
    await expect(permission.locator('.permission-state')).toHaveText(
      'Browser permission: not asked yet.',
    );

    await toggle.uncheck();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Saved.')).toBeVisible();
    await page.reload();
    await expect(toggle).not.toBeChecked();

    await permission.getByRole('button', { name: 'Allow notifications' }).click();
    await expect(permission.locator('.permission-state')).toHaveText(
      'Browser permission: granted.',
    );
    await expect(permission.getByRole('button')).toBeHidden();

    await toggle.check();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Saved.')).toBeVisible();
    await page.reload();
    await expect(toggle).toBeChecked();
  });

  test('the notifications page says when the browser blocks notifications', async ({ page }) => {
    await stubNotification(page, 'denied');
    await page.goto('/settings/notifications');

    await expect(page.locator('#notification-permission .permission-state')).toContainText(
      'Browser permission: blocked.',
    );
  });
});
