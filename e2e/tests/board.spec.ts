// Board (/) use cases, by test name:
//
// - The board lists the seeded jobs in their sections, with the totals line and
//   the worker tiles: "lists the seeded jobs in their sections with totals and workers"
// - The project picker switches the board through its row links:
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
// - The new-request form's pull request picker, under "pull request picker":
//   searching, badges, keyboard, picking, Remove, polls, blocked picks, errors
import type { Page, Route } from '@playwright/test';
import {
  ADMIN,
  COLLABORATOR,
  OUTSIDER,
  SHARED_PROJECT,
  chooseProject,
  expect,
  test,
  type Factory,
} from '../fixtures';

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

  await chooseProject(page, SHARED_PROJECT);

  await expect(page).toHaveURL(new RegExp(`[?&]project=${SHARED_PROJECT}$`));
  await expect(page.getByRole('link', { name: 'Support a second USB hub' })).toBeVisible();
});

async function badgeLogin(page: Page, factory: { fixture(...args: string[]): string }, projects: string[]) {
  const login = `badge-${projects[0]}@example.com`;
  for (const name of projects) factory.fixture('project', '--name', name, '--login', login);
  await page.setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
}

test('the badge totals the other projects', async ({ page, factory }, testInfo) => {
  const id = `b${testInfo.workerIndex}-${Date.now()}`;
  const [home, one, two, three] = [`${id}-h`, `${id}-1`, `${id}-2`, `${id}-3`];
  await badgeLogin(page, factory, [home, one, two, three]);
  factory.fixture('job', '--project', home, '--stage', 'spec');
  await page.goto(`/?project=${home}`);
  await expect(page.locator('#picker-badge')).toBeHidden();

  factory.fixture('job', '--project', one, '--stage', 'spec');
  factory.fixture('job', '--project', one, '--stage', 'plan');
  factory.fixture('job', '--project', two, '--stage', 'spec');
  factory.fixture('job', '--project', three, '--stage', 'failed');
  await page.goto(`/?project=${home}`);

  const badge = page.locator('#picker-badge');
  await expect(badge.locator('.badge.you')).toHaveText('3');
  await expect(badge.locator('.badge.bad')).toHaveText('1');
  const label = '3 need you in 2 other projects, 1 failed in 1 other project';
  await expect(badge).toHaveAttribute('aria-label', label);
  await expect(badge).toHaveAttribute('title', label);
});

test('the badge follows a job in another project', async ({ page, factory }, testInfo) => {
  const id = `f${testInfo.workerIndex}-${Date.now()}`;
  const [home, other] = [`${id}-h`, `${id}-o`];
  await badgeLogin(page, factory, [home, other]);
  await page.goto(`/?project=${home}`);
  await expect(page.locator('#picker-badge')).toBeHidden();

  factory.fixture('job', '--project', other, '--stage', 'spec');
  const badge = page.locator('#picker-badge');
  await expect(badge.locator('.badge.you')).toHaveText('1', { timeout: 10_000 });
  await expect(badge).toHaveAttribute('aria-label', '1 need you in 1 other project');
});

test('the picker search filters and opens the match', async ({ page, factory }, testInfo) => {
  const id = `s${testInfo.workerIndex}-${Date.now()}`;
  const [alpha, alphabet, beta] = [`${id}-alpha`, `${id}-alphabet`, `${id}-beta`];
  await badgeLogin(page, factory, [alpha, alphabet, beta]);
  await page.goto(`/?project=${beta}`);
  const rows = page.locator('#picker-list li[data-id]:not([hidden])');
  const search = page.getByLabel('Find a project');

  await page.getByLabel('Project', { exact: true }).click();
  await search.fill(`${id}-ALPH`.toUpperCase());
  await expect(rows).toHaveCount(2);
  await expect(rows.first()).toHaveClass(/active/);
  await search.press('Enter');
  await expect(page).toHaveURL(new RegExp(`[?&]project=${alpha}$`));

  await page.getByLabel('Project', { exact: true }).click();
  await page.getByLabel('Find a project').fill('ALPH');
  await page.getByLabel('Find a project').press('ArrowDown');
  await page.getByLabel('Find a project').press('Enter');
  await expect(page).toHaveURL(new RegExp(`[?&]project=${alphabet}$`));

  await page.getByLabel('Project', { exact: true }).click();
  await page.getByLabel('Find a project').fill('no-such-project');
  await expect(rows).toHaveCount(0);
  await expect(page.locator('.picker-empty')).toBeVisible();
  await expect(page.locator('.picker-status')).toHaveText('No project matches.');
});

test('broadening or reopening the search highlights the first row', async ({ page, factory }, testInfo) => {
  const id = `r${testInfo.workerIndex}-${Date.now()}`;
  const [alpha, alphabet, beta] = [`${id}-alpha`, `${id}-alphabet`, `${id}-beta`];
  await badgeLogin(page, factory, [alpha, alphabet, beta]);
  await page.goto(`/?project=${beta}`);
  const search = page.getByLabel('Find a project');
  const active = page.locator('#picker-list li[data-id].active');

  await page.getByLabel('Project', { exact: true }).click();
  await search.fill(`${id}-alphabet`);
  await expect(active).toHaveAttribute('data-id', alphabet);
  await search.fill(`${id}-alpha`);
  await expect(active).toHaveAttribute('data-id', alpha);
  await search.press('ArrowDown');
  await expect(active).toHaveAttribute('data-id', alphabet);

  await search.press('Escape');
  await page.getByLabel('Project', { exact: true }).click();
  await expect(active).toHaveAttribute('data-id', alpha);
  await expect(page.locator('.picker-status')).toHaveText(alpha);
});

test('opening the picker focuses the search and closing clears it', async ({ page, factory }, testInfo) => {
  const id = `c${testInfo.workerIndex}-${Date.now()}`;
  const [home, other] = [`${id}-h`, `${id}-o`];
  await badgeLogin(page, factory, [home, other]);
  await page.goto(`/?project=${home}`);
  const button = page.getByLabel('Project', { exact: true });
  const search = page.getByLabel('Find a project');

  await button.click();
  await expect(search).toBeFocused();
  await search.fill('-o');
  await expect(page.locator('#picker-list li[data-id]:not([hidden])')).toHaveCount(1);
  await search.press('Escape');
  await expect(button).toBeFocused();

  await button.click();
  await expect(search).toHaveValue('');
  await expect(page.locator('#picker-list li[data-id]:not([hidden])')).toHaveCount(2);
});

test('a poll keeps the picker search text, focus and filter', async ({ page, factory }, testInfo) => {
  const id = `k${testInfo.workerIndex}-${Date.now()}`;
  const [home, other, third] = [`${id}-h`, `${id}-o`, `${id}-t`];
  await badgeLogin(page, factory, [home, other, third]);
  await page.goto(`/?project=${home}`);
  const search = page.getByLabel('Find a project');
  await page.getByLabel('Project', { exact: true }).click();
  await search.fill('-o');
  const row = page.locator(`#picker-list li[data-id="${other}"]`);
  await expect(row).toHaveAttribute('data-attention', 'idle');

  factory.fixture('job', '--project', other, '--stage', 'spec');
  await expect(row).toHaveAttribute('data-attention', 'you', { timeout: 10_000 });
  await expect(page.locator('#project-picker')).toHaveJSProperty('open', true);
  await expect(search).toHaveValue('-o');
  await expect(search).toBeFocused();
  await expect(row).toBeVisible();
  await expect(page.locator(`#picker-list li[data-id="${third}"]`)).toBeHidden();
});

test.describe('without a project', () => {
  test.use({ extraHTTPHeaders: { 'Tailscale-User-Login': OUTSIDER } });

  test('a login on no project is sent to onboarding', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByText('You are not a collaborator on any project.')).toBeVisible();
    await expect(page.getByRole('link', { name: 'Onboard a project' })).toHaveAttribute('href', '/onboard');
    await expect(page.locator('a.source-link')).toHaveCount(0);
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

test.describe('pull request picker', () => {
  let counter = 0;

  async function pullProject(page: Page, factory: Factory, repo: string, workerIndex: number) {
    const name = `${repo}-${workerIndex}-${counter++}`;
    factory.fixture('project', '--name', name, '--login', ADMIN, '--repo', `e2e/${name}`);
    await page.goto(`/settings/projects/${name}`);
    await page.locator('#forge-account').getByLabel('Project token').fill('tok-e2e-alice');
    await page.getByRole('button', { name: 'Save project token' }).click();
    await expect(page.getByText('Saved. The project token belongs to alice-bot.')).toBeVisible();
    return name;
  }

  const search = (page: Page) => page.getByRole('combobox', { name: 'Pull request' });
  const rows = (page: Page) => page.locator('#pr-results li[role="option"]');

  async function openPicker(page: Page, name: string) {
    await page.goto(`/?project=${name}`);
    await page.locator('#request').click();
    await search(page).click();
  }

  test('an empty query lists the latest pull requests with their badges', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    const held = factory.fixture(
      'renovate', '--project', name, '--number', '104', '--url', `https://github.com/e2e/${name}/pull/104`,
    );
    await openPicker(page, name);

    await expect(rows(page)).toHaveCount(5);
    await expect(rows(page).nth(0)).toContainText('#101');
    await expect(rows(page).nth(0)).toContainText('Add dark mode');
    await expect(rows(page).nth(0)).toContainText('carol');
    await expect(rows(page).nth(0)).toContainText('carol/dark → main');
    await expect(rows(page).nth(1).locator('.badge')).toHaveText('fork');
    await expect(rows(page).nth(2).locator('.badge')).toHaveText('draft');
    await expect(rows(page).nth(3).locator('.badge')).toHaveText(`job #${held}`);
    await expect(rows(page).nth(4)).toContainText('release → next');
    await expect(rows(page).nth(0).locator('.badge')).toHaveCount(0);
    await expect(page.locator('#pr-results .pr-note')).toHaveCount(0);
  });

  test('a title, a number and a number with a hash find pull requests', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await expect(rows(page)).toHaveCount(5);

    await search(page).fill('typo readme');
    await expect(rows(page)).toHaveCount(1);
    await expect(rows(page).first()).toContainText('#102');

    for (const query of ['103', '#103']) {
      await search(page).fill(query);
      await expect(rows(page)).toHaveCount(1);
      await expect(rows(page).first()).toContainText('Rework the parser');
    }

    await search(page).fill('nothing like this');
    await expect(rows(page)).toHaveCount(0);
    await expect(page.locator('#pr-results .pr-note')).toHaveText('No pull request matches.');
    await search(page).fill('#999');
    await expect(page.locator('#pr-results .pr-note')).toHaveText('No pull request matches.');
  });

  test('more than twenty matches show twenty rows and ask to refine the search', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'many', testInfo.workerIndex);
    await openPicker(page, name);

    await expect(rows(page)).toHaveCount(20);
    await expect(page.locator('#pr-results .pr-note')).toHaveText('Refine the search to see more.');
    await search(page).fill('request 21');
    await expect(rows(page)).toHaveCount(1);
    await expect(page.locator('#pr-results .pr-note')).toHaveCount(0);
  });

  test('typing sends one search after the pause and not one per key', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    const queries: string[] = [];
    page.on('request', (request) => {
      const url = new URL(request.url());
      if (url.pathname === `/projects/${name}/pulls`) queries.push(url.searchParams.get('q') ?? '');
    });
    await openPicker(page, name);
    await expect(rows(page)).toHaveCount(5);
    await search(page).pressSequentially('dark', { delay: 30 });
    await expect(rows(page)).toHaveCount(1);
    await page.waitForTimeout(500);
    expect(queries).toEqual(['', 'dark']);
  });

  test('Enter while a replacement search is pending picks nothing, and Escape keeps the list closed', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    let hold = false;
    const held: Route[] = [];
    await page.route(`**/projects/${name}/pulls?*`, async (route) => {
      if (!hold) return route.continue();
      held.push(route);
    });
    const release = async () => {
      hold = false;
      await Promise.all(held.splice(0).map((route) => route.continue().catch(() => undefined)));
    };
    await openPicker(page, name);
    await expect(rows(page)).toHaveCount(5);
    await expect(rows(page).first()).toHaveClass(/active/);

    hold = true;
    await search(page).pressSequentially('#102');
    await page.keyboard.press('Enter');
    await expect(page.locator('input[name="pr"]')).toHaveValue('');
    await expect(page.locator('.pr-picked')).toBeHidden();
    await expect(rows(page)).toHaveCount(0);
    await expect.poll(() => held.length).toBeGreaterThan(0);
    await release();
    await expect(rows(page)).toHaveCount(1);
    await expect(page.locator('#pr-results')).toBeVisible();

    hold = true;
    await search(page).pressSequentially('0');
    await expect.poll(() => held.length).toBeGreaterThan(0);
    await page.keyboard.press('Escape');
    await release();
    await page.waitForTimeout(500);
    await expect(page.locator('#pr-results')).toBeHidden();
    await expect(search(page)).toHaveAttribute('aria-expanded', 'false');
  });

  test('the keyboard picks a pull request, Enter does not submit and Escape closes the list', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await expect(rows(page)).toHaveCount(5);
    await expect(search(page)).toHaveAttribute('aria-expanded', 'true');

    await page.keyboard.press('Escape');
    await expect(page.locator('#pr-results')).toBeHidden();
    await expect(search(page)).toHaveAttribute('aria-expanded', 'false');
    await page.keyboard.press('ArrowDown');
    await expect(page.locator('#pr-results')).toBeVisible();

    await page.keyboard.press('ArrowDown');
    await expect(rows(page).nth(1)).toHaveClass(/active/);
    await expect(search(page)).toHaveAttribute('aria-activedescendant', 'pr-option-102');
    await page.keyboard.press('ArrowUp');
    await expect(rows(page).nth(0)).toHaveClass(/active/);
    await page.keyboard.press('Enter');

    await expect(page).toHaveURL(new RegExp(`/\\?project=${name}$`));
    await expect(page.locator('.pr-picked')).toContainText('#101 Add dark mode');
    await expect(page.locator('input[name="pr"]')).toHaveValue('101');
    await expect(page.locator('#pr-results')).toBeHidden();
  });

  test('a picked pull request links to the forge in a new tab, and Remove clears the pick', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await rows(page).first().click();

    const link = page.locator('.pr-picked a.picked-pr');
    await expect(link).toHaveAttribute('href', `https://github.com/e2e/${name}/pull/101`);
    await expect(link).toHaveAttribute('target', '_blank');
    await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
    await expect(page.getByRole('button', { name: 'Request' })).toBeEnabled();

    await page.getByRole('button', { name: 'Remove' }).click();
    await expect(page.locator('.pr-picked')).toBeHidden();
    await expect(page.locator('input[name="pr"]')).toHaveValue('');
    await expect(search(page)).toBeVisible();
    await expect(search(page)).toBeFocused();
  });

  test('a pick, a query and the typed text survive a board poll', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await search(page).fill('typo');
    await expect(rows(page)).toHaveCount(1);
    await page.locator('#request').fill('Half typed');

    factory.fixture('job', '--project', name, '--stage', 'questions', '--request', 'Created behind the picker');
    await expect(page.getByRole('link', { name: 'Created behind the picker' })).toBeVisible();
    await expect(search(page)).toHaveValue('typo');
    await expect(page.locator('#request')).toHaveValue('Half typed');

    await rows(page).first().click();
    factory.fixture('job', '--project', name, '--stage', 'questions', '--request', 'Created behind the pick');
    await expect(page.getByRole('link', { name: 'Created behind the pick' })).toBeVisible();
    await expect(page.locator('.pr-picked')).toContainText('#102');
    await expect(page.locator('input[name="pr"]')).toHaveValue('102');
    await expect(page.locator('#request')).toHaveValue('Half typed');
  });

  test('Request is disabled for a fork pull request and for one a job holds', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    const held = factory.fixture(
      'renovate', '--project', name, '--number', '104', '--url', `https://github.com/e2e/${name}/pull/104`,
    );
    await openPicker(page, name);
    const request = page.getByRole('button', { name: 'Request' });

    await rows(page).nth(1).click();
    await expect(request).toBeDisabled();
    await expect(page.locator('.pr-hint')).toContainText("cannot push to a fork's branch");

    await page.getByRole('button', { name: 'Remove' }).click();
    await expect(request).toBeEnabled();
    await search(page).click();
    await rows(page).nth(3).click();
    await expect(request).toBeDisabled();
    await expect(page.locator('.pr-hint a')).toHaveAttribute('href', `/jobs/${held}`);

    await page.getByRole('button', { name: 'Remove' }).click();
    await search(page).click();
    await rows(page).nth(2).click();
    await expect(request).toBeEnabled();
  });

  test('a request on a picked pull request creates a job that links the pull request', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await rows(page).first().click();
    await page.locator('#request').fill('Handle the empty case');
    await page.getByRole('button', { name: 'Request' }).click();

    await expect(page).toHaveURL(/\/jobs\/\d+$/);
    const link = page.locator('a.picked-pr');
    await expect(link).toHaveAttribute('href', `https://github.com/e2e/${name}/pull/101`);
    await expect(link).toHaveAttribute('target', '_blank');
    await expect(link).toHaveAttribute('rel', 'noopener noreferrer');

    await page.goto(`/?project=${name}`);
    await expect(page.locator('li.job a.picked-pr')).toHaveText('#101');
    await expect(page.locator('li.job a.picked-pr')).toHaveAttribute('target', '_blank');
  });

  test('a review with an empty box is titled with the pull request on the board and the job page', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await rows(page).first().click();
    await page.getByRole('button', { name: 'Quick review' }).click();

    await expect(page).toHaveURL(/\/jobs\/\d+$/);
    await expect(page.getByRole('heading', { level: 1 })).toContainText('Review #101: Add dark mode');

    await page.goto(`/?project=${name}`);
    await expect(page.locator('li.job')).toContainText('Review #101: Add dark mode');
  });

  test('the review buttons appear only once a pull request is picked', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await page.goto(`/?project=${name}`);
    await expect(page.locator('.review-actions')).toBeHidden();
    await expect(page.getByRole('button', { name: 'Quick review' })).toBeHidden();

    await openPicker(page, name);
    await rows(page).first().click();
    await expect(page.getByRole('button', { name: 'Quick review' })).toBeVisible();
    await expect(page.getByLabel('Also comment on the pull request')).not.toBeChecked();
    await expect(page.getByRole('button', { name: 'Review and fix' })).toBeHidden();

    await page.getByRole('button', { name: 'Remove' }).click();
    await expect(page.locator('.review-actions')).toBeHidden();
  });

  test('the menu opens, moves with the keys, closes on Escape and starts a review and fix', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await rows(page).first().click();
    const toggle = page.getByRole('button', { name: 'More review actions' });
    const item = page.getByRole('menuitem', { name: 'Review and fix' });

    await expect(toggle).toHaveAttribute('aria-expanded', 'false');
    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-expanded', 'true');
    await expect(item).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(item).toBeHidden();
    await expect(toggle).toHaveAttribute('aria-expanded', 'false');
    await expect(toggle).toBeFocused();

    await page.keyboard.press('ArrowDown');
    await expect(item).toBeFocused();
    const posted = page.waitForRequest((request) => request.method() === 'POST' && request.url().endsWith('/jobs'));
    await page.keyboard.press('Enter');
    expect((await posted).postData()).toMatch(/name="mode"\r\n\r\nreview-and-fix\r\n/);
    await expect(page).toHaveURL(/\/jobs\/\d+$/);
    await expect(page.getByRole('heading', { level: 1 })).toContainText('Review #101: Add dark mode');
    await expect(page.locator('.review-badge')).toHaveText('review');
  });

  test('a review submits with an empty box and Request with an empty box is refused', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    await openPicker(page, name);
    await rows(page).first().click();

    await page.getByRole('button', { name: 'Request', exact: true }).click();
    await expect(page).toHaveURL(new RegExp(`/\\?project=${name}$`));
    await expect(page.locator('#request')).toHaveJSProperty('validity.valueMissing', true);

    await page.getByLabel('Also comment on the pull request').check();
    const posted = page.waitForRequest((request) => request.method() === 'POST' && request.url().endsWith('/jobs'));
    await page.getByRole('button', { name: 'Quick review' }).click();
    const data = (await posted).postData();
    expect(data).toMatch(/name="mode"\r\n\r\nquick-review\r\n/);
    expect(data).toMatch(/name="comment"\r\n\r\non\r\n/);
    await expect(page).toHaveURL(/\/jobs\/\d+$/);
    await expect(page.getByRole('heading', { level: 1 })).toContainText('Review #101: Add dark mode');
  });

  test('Review and fix is disabled for a fork pull request and for one a job holds', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    factory.fixture(
      'renovate', '--project', name, '--number', '104', '--url', `https://github.com/e2e/${name}/pull/104`,
    );
    await openPicker(page, name);
    const toggle = page.getByRole('button', { name: 'More review actions' });
    const item = page.getByRole('menuitem', { name: 'Review and fix' });

    for (const index of [1, 3]) {
      await rows(page).nth(index).click();
      await expect(page.getByRole('button', { name: 'Quick review' })).toBeEnabled();
      await toggle.click();
      await expect(item).toBeDisabled();
      await page.getByRole('button', { name: 'Remove' }).click();
      await search(page).click();
    }
    await rows(page).nth(2).click();
    await toggle.click();
    await expect(item).toBeEnabled();
  });

  test('a forge error shows in the picker and the form keeps working', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'broken', testInfo.workerIndex);
    await openPicker(page, name);

    await expect(page.locator('#pr-results .pr-error')).toContainText('Could not search pull requests');
    await expect(page.locator('#pr-results .pr-error')).toContainText('rate limit');
    await page.locator('#request').fill('Still works');
    await page.getByRole('button', { name: 'Request' }).click();
    await expect(page).toHaveURL(/\/jobs\/\d+$/);
  });

  test('the search answers 404 to a login that is not a collaborator', async ({ page, factory }, testInfo) => {
    const name = await pullProject(page, factory, 'prs', testInfo.workerIndex);
    const outsider = await page.request.get(`/projects/${name}/pulls`, {
      headers: { 'Tailscale-User-Login': OUTSIDER },
    });
    expect(outsider.status()).toBe(404);
    const member = await page.request.get(`/projects/${name}/pulls`);
    expect(member.status()).toBe(200);
  });
});

test('an opened Show older disclosure stays open across a poll', async ({ page, factory, project }) => {
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

test('a closed Show older disclosure stays closed when another region swaps', async ({ page, factory, project }) => {
  for (let number = 1; number <= 6; number++) {
    factory.fixture('job', '--project', project, '--stage', 'cancelled', '--request', `Old change ${number}`);
  }
  await page.goto(`/?project=${project}`);
  const older = page.locator('details.older');
  await older.getByText('Show 1 older').click();

  factory.fixture('job', '--project', project, '--stage', 'cancelled', '--request', 'Cancelled behind the page');
  await expect(older.getByText('Show 2 older')).toBeVisible();
  await expect(older).toHaveJSProperty('open', true);

  await older.getByText('Show 2 older').click();
  await expect(older).toHaveJSProperty('open', false);

  await page.evaluate(async () => {
    const stats = document.querySelector('#stats')!;
    const url = new URL(stats.getAttribute('hx-get')!, location.href);
    url.searchParams.delete('v');
    await (window as any).htmx.ajax('GET', url.pathname + url.search, { target: '#stats', swap: 'outerHTML' });
  });
  await expect(older).toHaveJSProperty('open', false);
});

test("the board links the selected project's repository", async ({ page, project, baseURL }) => {
  const origin = new URL(baseURL!).origin;
  const requested: string[] = [];
  page.on('request', (request) => requested.push(request.url()));
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`/?project=${project}`);

  const link = page.locator('div.status-row a.source-link');
  await expect(link).toBeVisible();
  await expect(link).toHaveText(`e2e/${project}`);
  await expect(link).toHaveAttribute('href', `https://github.com/e2e/${project}`);
  await expect(link).toHaveAttribute('target', '_blank');
  await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  await expect(link.locator('svg[aria-hidden="true"]')).toHaveCount(1);
  await expect(page.locator('#workers a.source-link')).toHaveCount(0);
  await expect(page.locator('#usage a.source-link')).toHaveCount(0);
  expect(requested.filter((url) => !url.startsWith(origin))).toEqual([]);

  const box = await link.boundingBox();
  const workers = await page.locator('#workers').boundingBox();
  const row = await page.locator('div.status-row').boundingBox();
  expect(box!.y).toBeGreaterThanOrEqual(workers!.y - 1);
  expect(box!.y + box!.height).toBeLessThanOrEqual(workers!.y + workers!.height + 1);
  expect(box!.x + box!.width).toBeGreaterThan(row!.x + row!.width - 4);

  await page.route('**/fragments/workers*', async (route) => {
    const url = new URL(route.request().url());
    url.searchParams.delete('v');
    const response = await route.fetch({ url: url.toString() });
    const body = (await response.text()).replace('Worker 1', 'Worker 1 polled');
    await route.fulfill({ response, body });
  });
  await expect(page.locator('#workers')).toContainText('Worker 1 polled', { timeout: 15_000 });
  await expect(link).toBeVisible();

  await chooseProject(page, SHARED_PROJECT);
  await expect(link).toHaveText(`e2e/${SHARED_PROJECT}`);
  await expect(link).toHaveAttribute('href', `https://github.com/e2e/${SHARED_PROJECT}`);

  await page.setViewportSize({ width: 480, height: 800 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});

// A repo is unique and each worker has its own server, so each worker seeds the
// project with the real label once.
let factoryLabelSeeded = false;
const seedFactoryLabel = (factory: { fixture: (...args: string[]) => string }) => {
  const name = 'factory-label';
  if (!factoryLabelSeeded) {
    factory.fixture('project', '--name', name, '--repo', 'Badger-Embedded/factory', '--login', ADMIN);
    factoryLabelSeeded = true;
  }
  return name;
};

const frames = (page: Page) =>
  page.evaluate(async () => {
    for (let i = 0; i < 3; i++) await new Promise((resolve) => requestAnimationFrame(resolve));
  });

// source-link.js measures from a ResizeObserver, so the layout settles a frame or two after the swap.
const loadUsage = async (page: Page) => {
  await page.evaluate(() =>
    (window as any).htmx.ajax('GET', '/fragments/plan-usage', { target: '#usage', swap: 'innerHTML' }),
  );
  await frames(page);
};

test('the source link stays at the top right whether the usage strip fits beside the tiles or wraps', async ({ page, factory }) => {
  const project = seedFactoryLabel(factory);
  const windows = (names: string[]) =>
    names
      .map(
        (name) =>
          `<li class="usage-window"><span class="usage-name">${name}</span><span class="usage-track"><span class="usage-fill" style="width: 40%"></span></span><span class="usage-percent">40% used</span><span class="usage-reset">resets <time datetime="2026-11-22T23:30:00Z" data-format="datetime">2026-11-22 23:30 UTC</time></span></li>`,
      )
      .join('');
  const strip = (names: string[]) =>
    `<section class="plan-usage"><h2>Claude plan usage <span class="usage-as-of">as of Mon, 22 Nov, 22:00</span></h2><ul class="usage-windows">${windows(names)}</ul></section>`;
  let body = strip(['5-hour']);
  await page.route('**/fragments/plan-usage*', (route) => route.fulfill({ contentType: 'text/html', body }));

  const cases = [
    { width: 1280, names: ['5-hour'], beside: true },
    { width: 1280, names: ['5-hour', 'Weekly', 'Fable'], beside: false },
    { width: 1024, names: ['5-hour'], beside: true },
    { width: 768, names: ['5-hour'], beside: false },
  ];
  for (const { width, names, beside } of cases) {
    body = strip(names);
    await page.setViewportSize({ width, height: 800 });
    await page.goto(`/?project=${project}`);
    await loadUsage(page);
    await expect(page.locator('#usage .usage-window')).toHaveCount(names.length);
    await expect(page.locator('a.source-link')).toHaveText('Badger-Embedded/factory');

    const link = (await page.locator('a.source-link').boundingBox())!;
    const usage = (await page.locator('#usage').boundingBox())!;
    const workers = (await page.locator('#workers').boundingBox())!;
    const row = (await page.locator('div.status-row').boundingBox())!;
    expect(Math.abs(link.y - workers.y)).toBeLessThanOrEqual(1);
    expect(link.x + link.width).toBeGreaterThan(row.x + row.width - 4);
    expect(link.x).toBeGreaterThanOrEqual(workers.x + workers.width);
    const tiles = page.locator('#workers .worker');
    const [first, second] = [(await tiles.nth(0).boundingBox())!, (await tiles.nth(1).boundingBox())!];
    expect(Math.abs(first.y - second.y)).toBeLessThan(2);
    if (beside) {
      expect(usage.y).toBeLessThan(workers.y + workers.height);
      expect(usage.x).toBeGreaterThanOrEqual(workers.x + workers.width);
      expect(usage.y).toBeGreaterThanOrEqual(link.y + link.height - 1);
      expect(usage.x + usage.width).toBeLessThanOrEqual(row.x + row.width + 1);
    } else {
      expect(usage.y).toBeGreaterThanOrEqual(workers.y + workers.height - 1);
      expect(usage.y).toBeLessThanOrEqual(workers.y + workers.height + 16);
    }
  }
});

test('the source link does not overlap the tiles or the request form with six or zero workers', async ({ page, project }) => {
  const setWorkers = async (count: number) => {
    await page.goto('/settings/workers');
    await page.locator('#max_workers').fill(String(count));
    await page.getByRole('button', { name: 'Save' }).click();
    await expect(page.getByText('Saved.')).toBeVisible();
  };
  await page.setViewportSize({ width: 1280, height: 800 });
  try {
    for (const count of [6, 0]) {
      await setWorkers(count);
      await page.goto(`/?project=${project}`);
      await expect(page.locator('li.worker')).toHaveCount(count);

      const link = (await page.locator('a.source-link').boundingBox())!;
      const row = await page.locator('div.status-row').boundingBox();
      expect(link.x + link.width).toBeGreaterThan(row!.x + row!.width - 4);
      for (const tile of await page.locator('li.worker').all()) {
        const box = (await tile.boundingBox())!;
        const apart = box.x + box.width <= link.x || box.y + box.height <= link.y || link.y + link.height <= box.y;
        expect(apart).toBe(true);
      }
      const form = (await page.locator('form.composer').boundingBox())!;
      expect(link.y + link.height).toBeLessThanOrEqual(form.y + 1);
    }
  } finally {
    await setWorkers(2);
  }
});

const apart = (a: { x: number; y: number; width: number; height: number }, b: typeof a) =>
  a.x + a.width <= b.x + 1 || b.x + b.width <= a.x + 1 || a.y + a.height <= b.y + 1 || b.y + b.height <= a.y + 1;

test('a long name or a wide label never overlaps a tile, the usage strip or the form', async ({ page, factory }) => {
  const strip =
    '<section class="plan-usage"><h2>Claude plan usage</h2><ul class="usage-windows"><li class="usage-window"><span class="usage-name">5-hour</span><span class="usage-percent">40% used</span></li></ul></section>';
  await page.route('**/fragments/plan-usage*', (route) => route.fulfill({ contentType: 'text/html', body: strip }));
  const names = [`long-${'x'.repeat(70)}`, 'W'.repeat(26)];
  for (const name of names) factory.fixture('project', '--name', name, '--login', ADMIN);
  for (const name of names) {
    await page.goto(`/?project=${name}`);
    await loadUsage(page);
    await expect(page.locator('#usage .plan-usage')).toBeVisible();
    for (const width of [1280, 768, 480]) {
      await page.setViewportSize({ width, height: 800 });
      await frames(page);
      const link = page.locator('a.source-link');
      await expect(link).toHaveText(`e2e/${name}`);
      const box = (await link.boundingBox())!;
      for (const tile of await page.locator('li.worker').all()) expect(apart(box, (await tile.boundingBox())!)).toBe(true);
      expect(apart(box, (await page.locator('#usage').boundingBox())!)).toBe(true);
      expect(box.y + box.height).toBeLessThanOrEqual((await page.locator('form.composer').boundingBox())!.y + 1);
      expect(await page.evaluate(() => [...document.querySelectorAll('.status-row, .status-row *')].filter((e) => e.getBoundingClientRect().right > window.innerWidth + 1).map((e) => e.tagName + '.' + e.className).slice(0, 5))).toEqual([]);
    }
  }
});

test('the factory-sized label sits top right and leaves both default tiles on one row at 768 px', async ({ page, factory }) => {
  const name = seedFactoryLabel(factory);
  for (const width of [1280, 768]) {
    await page.setViewportSize({ width, height: 800 });
    await page.goto(`/?project=${name}`);
    const link = page.locator('a.source-link');
    await expect(link).toHaveText('Badger-Embedded/factory');
    await expect(link).toHaveAttribute('href', 'https://github.com/Badger-Embedded/factory');
    const box = (await link.boundingBox())!;
    const row = (await page.locator('div.status-row').boundingBox())!;
    expect(box.x + box.width).toBeGreaterThan(row.x + row.width - 4);
    const tiles = page.locator('#workers .worker');
    expect(await tiles.count()).toBe(2);
    const [a, b] = [(await tiles.nth(0).boundingBox())!, (await tiles.nth(1).boundingBox())!];
    expect(Math.abs(a.y - b.y)).toBeLessThan(2);
    expect(Math.abs(box.y - a.y)).toBeLessThan(2);
    expect(apart(box, a) && apart(box, b)).toBe(true);
  }
});

test.describe('without scripts', () => {
  test.use({ javaScriptEnabled: false });

  test('a long repository name stays in flow, clear of the request form', async ({ page, factory }) => {
    const name = `nojs-${'x'.repeat(70)}`;
    factory.fixture('project', '--name', name, '--login', ADMIN);
    const setWorkers = async (count: number) => {
      await page.goto('/settings/workers');
      await page.locator('#max_workers').fill(String(count));
      await page.getByRole('button', { name: 'Save' }).click();
      await expect(page.getByText('Saved.')).toBeVisible();
    };
    await setWorkers(0);
    try {
      await page.setViewportSize({ width: 768, height: 800 });
      await page.goto(`/?project=${name}`);
      await expect(page.locator('li.worker')).toHaveCount(0);
      const link = (await page.locator('a.source-link').boundingBox())!;
      const form = (await page.locator('form.composer').boundingBox())!;
      expect(link.y + link.height).toBeLessThanOrEqual(form.y + 1);
    } finally {
      await setWorkers(2);
    }
  });

  test('a pull request number typed by hand creates a job on that pull request', async ({ page, factory }, testInfo) => {
    const name = `prs-nojs-${testInfo.workerIndex}-${Date.now()}`;
    factory.fixture('project', '--name', name, '--login', ADMIN, '--repo', `e2e/${name}`);
    await page.goto(`/settings/projects/${name}`);
    await page.locator('#forge-account').getByLabel('Project token').fill('tok-e2e-alice');
    await page.getByRole('button', { name: 'Save project token' }).click();
    await expect(page.getByText('Saved. The project token belongs to alice-bot.')).toBeVisible();

    await page.goto(`/?project=${name}`);
    await page.locator('#request').fill('Fix the parser');
    await page.getByLabel('Pull request', { exact: true }).fill('#103');
    await page.getByRole('button', { name: 'Request' }).click();
    await expect(page).toHaveURL(/\/jobs\/\d+$/);
    await expect(page.locator('a.picked-pr')).toHaveAttribute('href', `https://github.com/e2e/${name}/pull/103`);

    await page.goto(`/?project=${name}`);
    await page.locator('#request').fill('Fix a missing one');
    await page.getByLabel('Pull request', { exact: true }).fill('999');
    await page.getByRole('button', { name: 'Request' }).click();
    await expect(page.getByText('pull request #999 does not exist')).toBeVisible();
  });

  test('a request filed with a Start after selection waits', async ({ page, factory, project }) => {
    const first = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The first job');
    await page.goto(`/?project=${project}`);
    await page.locator('#request').fill('Builds on the first job');
    await page.locator('fieldset.start-after').getByLabel(`#${first} The first job`).check();
    await page.getByRole('button', { name: 'Request' }).click();
    await expect(page).toHaveURL(/\/jobs\/\d+$/);

    await page.goto(`/?project=${project}`);
    const row = page.locator('li.job', { hasText: 'Builds on the first job' });
    await expect(row.locator('.badge')).toHaveText('waiting');
    await expect(row).toContainText(`waits for #${first}`);
  });
});

// Logins other than the admin can use the board of a project they collaborate on.
test.describe('as a collaborator', () => {
  test.use({ extraHTTPHeaders: { 'Tailscale-User-Login': COLLABORATOR } });

  test('sees the shared project', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByLabel('Project', { exact: true })).toContainText(SHARED_PROJECT);
  });
});

test('a project on an unknown forge shows a notice instead of a source link', async ({
  page,
  project,
  factory,
}) => {
  const odd = `${project}-odd`;
  factory.fixture('project', '--name', odd, '--login', ADMIN, '--forge', 'elsewhere');
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`/?project=${odd}`);

  await expect(page.locator('div.card.failed')).toContainText('elsewhere');
  await expect(page.locator('a.source-link')).toHaveCount(0);

  await chooseProject(page, project);
  await expect(page).toHaveURL(new RegExp(`project=${project}$`));
  await expect(page.locator('div.card.failed')).toHaveCount(0);
  const link = page.locator('div.status-row a.source-link');
  await expect(link).toHaveAttribute('href', `https://github.com/e2e/${project}`);
});

test('Renovate pull requests link through the forge and show a plain number on an unknown forge', async ({
  page,
  project,
  factory,
}) => {
  const odd = `${project}-odd`;
  factory.fixture('project', '--name', odd, '--login', ADMIN, '--forge', 'elsewhere');
  const url = 'https://stored.example/pull/7';
  factory.fixture('renovate', '--project', project, '--number', '7', '--url', url);
  factory.fixture('renovate', '--project', odd, '--number', '8', '--url', url);

  await page.goto(`/?project=${project}`);
  const link = page.locator('a.renovate-pr');
  await expect(link).toHaveText('#7');
  await expect(link).toHaveAttribute('href', `https://github.com/e2e/${project}/pull/7`);
  await expect(link).toHaveAttribute('target', '_blank');
  await expect(link).toHaveAttribute('rel', 'noopener noreferrer');

  await page.goto(`/?project=${odd}`);
  await expect(page.locator('.renovate-pr')).toHaveText('#8');
  await expect(page.locator('a.renovate-pr')).toHaveCount(0);
});

test('the Start after picker offers the unfinished jobs of the project', async ({ page, factory, project }) => {
  await page.goto(`/?project=${project}`);
  await expect(page.getByText('Start after')).toHaveCount(0);

  const open = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'An open job');
  const retryable = factory.fixture('job', '--project', project, '--stage', 'failed', '--request', 'A failed job');
  factory.fixture('job', '--project', project, '--stage', 'merged', '--request', 'A merged job');
  factory.fixture('job', '--project', project, '--stage', 'cancelled', '--request', 'A cancelled job');
  await page.goto(`/?project=${project}`);

  await page.locator('#request').fill('Needs the open job');
  const picker = page.locator('fieldset.start-after');
  await expect(picker.locator('legend')).toHaveText('Start after');
  await expect(picker.locator('input[name="after"]')).toHaveCount(2);
  await expect(picker.getByLabel(`#${open} An open job`)).toBeVisible();
  await expect(picker.getByLabel(`#${retryable} A failed job`)).toBeVisible();

  await picker.getByLabel(`#${open} An open job`).check();
  await page.getByRole('button', { name: 'Request' }).click();
  await expect(page).toHaveURL(/\/jobs\/\d+$/);
});

test('a request that waits shows as waiting, then blocked when its prerequisite is cancelled', async ({
  page,
  factory,
  project,
}) => {
  const first = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The first job');
  await page.goto(`/?project=${project}`);
  await page.locator('#request').fill('Builds on the first job');
  await page.locator('fieldset.start-after').getByLabel(`#${first} The first job`).check();
  await page.getByRole('button', { name: 'Request' }).click();
  await expect(page).toHaveURL(/\/jobs\/\d+$/);

  await page.goto(`/?project=${project}`);
  const row = page.locator('li.job', { hasText: 'Builds on the first job' });
  await expect(row.locator('.badge')).toHaveText('waiting');
  await expect(row.getByRole('link', { name: `#${first}` })).toHaveAttribute('href', `/jobs/${first}`);
  await expect(row.getByRole('link', { name: `#${first}` })).not.toHaveAttribute('target', '_blank');
  await expect(page.locator('section.inbox-running').locator('li.job', { hasText: 'Builds on' })).toHaveCount(1);

  factory.fixture('apply', '--job', first, '--event', 'cancel');

  const blocked = page.locator('section.inbox-you li.job', { hasText: 'Builds on the first job' });
  await expect(blocked.locator('.badge')).toHaveText('blocked');
  await expect(blocked).toContainText(`waits for #${first} (cancelled)`);
  await expect(blocked.getByRole('link', { name: 'Open' })).toBeVisible();
});
