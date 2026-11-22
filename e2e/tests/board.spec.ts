// Board (/) use cases, by test name:
//
// - The board lists the seeded jobs in their sections, with the totals line and
//   the worker tiles: "lists the seeded jobs in their sections with totals and workers"
// - The project picker switches the board without a submit button:
//   "the project picker switches the board"
// - A login on no project sees the empty state and the onboarding link:
//   "a login on no project is sent to onboarding"
// - Submitting the New request form creates a job that shows on the board:
//   "a new request creates a job that appears on the board"
// - Attaching a file and pasting an image list the attachments, and the job page
//   links them: "attached and pasted files are listed and linked from the job page"
// - A job created after page load appears without a reload and typed text
//   survives: "a job created after page load appears and keeps the typed request"
// - An opened "Show N older" disclosure stays open across a poll:
//   "an opened Show older disclosure stays open across a poll"
import { COLLABORATOR, OUTSIDER, SHARED_PROJECT, expect, test } from '../fixtures';

// A 1x1 PNG, so the server's byte check accepts it as an image.
const PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==';

test('lists the seeded jobs in their sections with totals and workers', async ({ page }) => {
  await page.goto(`/?project=${SHARED_PROJECT}`);

  const section = (name: string) => page.locator(`section.inbox-${name}`);
  await expect(section('you').getByRole('heading', { name: 'Needs you' })).toBeVisible();
  await expect(section('you').getByRole('link', { name: 'Support a second USB hub' })).toBeVisible();
  await expect(section('you').getByRole('link', { name: 'Add a reboot button, spec ready' })).toBeVisible();
  await expect(section('you').getByRole('link', { name: 'Add a reboot button, plan ready' })).toBeVisible();
  await expect(section('running').getByRole('link', { name: 'Show the SD card write speed' })).toBeVisible();
  await expect(section('running').getByRole('link', { name: 'Add a reboot button, implementing' })).toBeVisible();
  await expect(section('you').getByRole('link', { name: 'Add a reboot button, watching' })).toBeVisible();
  await expect(section('failed').getByRole('link', { name: 'Retry flashing when the SD mux is busy' })).toBeVisible();
  await expect(section('done').getByRole('link', { name: 'Merged change 5' })).toBeVisible();
  await expect(section('done').locator('details.older summary')).toHaveText('Show 1 older');

  const totals = page.locator('dl.totals#stats');
  await expect(totals).toContainText('Jobs created');
  await expect(totals).toContainText('14');
  const workers = page.locator('section.workers#workers');
  await expect(workers).toBeVisible();
  await expect(workers.locator('li.worker')).toHaveCount(2);
  await expect(workers.locator('li.worker.idle').first()).toBeVisible();
});

test('the project picker switches the board', async ({ page, project }) => {
  await page.goto(`/?project=${project}`);
  await expect(page.getByRole('heading', { name: 'Needs you' })).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'Support a second USB hub' })).toHaveCount(0);

  await page.getByLabel('Project').selectOption(SHARED_PROJECT);

  await expect(page).toHaveURL(new RegExp(`[?&]project=${SHARED_PROJECT}$`));
  await expect(page.getByRole('link', { name: 'Support a second USB hub' })).toBeVisible();
});

test.describe('without a project', () => {
  test.use({ extraHTTPHeaders: { 'Tailscale-User-Login': OUTSIDER } });

  test('a login on no project is sent to onboarding', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByText('You are not a collaborator on any project.')).toBeVisible();
    await expect(page.getByRole('link', { name: 'Onboard a project' })).toHaveAttribute('href', '/onboard');
  });
});

test('a new request creates a job that appears on the board', async ({ page, project }) => {
  await page.goto(`/?project=${project}`);
  await page.getByPlaceholder(`Describe a change for ${project}…`).fill('Add a fan speed readout');
  await page.getByRole('button', { name: 'Request' }).click();

  await expect(page).toHaveURL(/\/jobs\/\d+$/);
  await page.goto(`/?project=${project}`);
  await expect(page.getByRole('link', { name: 'Add a fan speed readout' })).toBeVisible();
});

test('attached and pasted files are listed and linked from the job page', async ({ page, project }) => {
  await page.goto(`/?project=${project}`);
  await page.locator('#images').setInputFiles({
    name: 'notes.txt',
    mimeType: 'text/plain',
    buffer: Buffer.from('The relay is on channel 3.\n'),
  });
  await expect(page.locator('#pasted-images .file-name')).toHaveText('notes.txt');

  await page.locator('#request').evaluate((textarea, base64) => {
    const bytes = Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
    const transfer = new DataTransfer();
    transfer.items.add(new File([bytes], 'pasted.png', { type: 'image/png' }));
    textarea.dispatchEvent(new ClipboardEvent('paste', { clipboardData: transfer, bubbles: true }));
  }, PNG_BASE64);
  await expect(page.locator('#pasted-images li')).toHaveCount(2);
  await expect(page.getByAltText('Attached image 2')).toBeVisible();

  await page.locator('#request').fill('Read the attachments');
  await page.getByRole('button', { name: 'Request' }).click();

  await expect(page).toHaveURL(/\/jobs\/\d+$/);
  const file = page.locator('ul.request-files a');
  const image = page.locator('ul.request-images a');
  await expect(file).toContainText('notes.txt');
  await expect(image).toHaveCount(1);
  for (const link of [file, image]) {
    const href = await link.getAttribute('href');
    const response = await page.request.get(href!);
    expect(response.status()).toBe(200);
  }
});

test('a job created after page load appears and keeps the typed request', async ({ page, factory, project }) => {
  await page.goto(`/?project=${project}`);
  const request = page.locator('#request');
  await request.fill('A half-typed request');

  factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'Created behind the page');

  await expect(page.getByRole('link', { name: 'Created behind the page' })).toBeVisible();
  await expect(request).toHaveValue('A half-typed request');
});

// Skipped until the details-state.js fix lands: the board swaps with outerHTML and the
// script reopens the section on the replaced element. Change test.fixme to test then.
test.fixme('an opened Show older disclosure stays open across a poll', async ({ page, factory, project }) => {
  for (let number = 1; number <= 6; number++) {
    factory.fixture('job', '--project', project, '--stage', 'cancelled', '--request', `Old change ${number}`);
  }
  await page.goto(`/?project=${project}`);
  const older = page.locator('details.older');
  await older.getByText('Show 1 older').click();
  await expect(older).toHaveJSProperty('open', true);

  factory.fixture('job', '--project', project, '--stage', 'requested', '--request', 'Queued behind the page');

  await expect(page.getByRole('link', { name: 'Queued behind the page' })).toBeVisible();
  await expect(older).toHaveJSProperty('open', true);
});

test.fixme('a closed Show older disclosure stays closed when another region swaps', async ({ page, factory, project }) => {
  for (let number = 1; number <= 6; number++) {
    factory.fixture('job', '--project', project, '--stage', 'cancelled', '--request', `Old change ${number}`);
  }
  await page.goto(`/?project=${project}`);
  const older = page.locator('details.older');
  await older.getByText('Show 1 older').click();

  // A board swap while the section is open records it as open.
  factory.fixture('job', '--project', project, '--stage', 'cancelled', '--request', 'Cancelled behind the page');
  await expect(older.getByText('Show 2 older')).toBeVisible();
  await expect(older).toHaveJSProperty('open', true);

  await older.getByText('Show 2 older').click();
  await expect(older).toHaveJSProperty('open', false);

  // Swap the totals line, a region that holds no disclosure.
  await page.evaluate(async () => {
    const stats = document.querySelector('#stats')!;
    const url = new URL(stats.getAttribute('hx-get')!, location.href);
    url.searchParams.delete('v');
    await (window as any).htmx.ajax('GET', url.pathname + url.search, { target: '#stats', swap: 'outerHTML' });
  });
  await expect(older).toHaveJSProperty('open', false);
});

// Logins other than the admin can use the board of a project they collaborate on.
test.describe('as a collaborator', () => {
  test.use({ extraHTTPHeaders: { 'Tailscale-User-Login': COLLABORATOR } });

  test('sees the shared project', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByLabel('Project')).toHaveValue(SHARED_PROJECT);
  });
});
