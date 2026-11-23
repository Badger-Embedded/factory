// Job page (/jobs/{id}) use cases, by test name:
//
// - The page renders for a job in each stage, from requested to cancelled:
//   "the page renders a job in the $stage stage"
// - Answering the questions, approving a spec, approving a plan, requesting
//   changes, cancelling, retrying a failed job and dismissing a failed job each
//   move the job and the page shows it: "answering the questions hands the job
//   to the agent", "approving the spec hands the job to the agent", "approving
//   the plan starts the implementation", "requesting changes sends the job back
//   to the agent", "cancelling a job stops it", "retrying a failed job resumes
//   it", "dismissing a failed job takes it off the Failed section"
// - Editing the request of a queued job saves the new text:
//   "editing a queued job's request saves the new text"
// - A state change on the server shows without a reload:
//   "a state change on the server appears without a reload"
// - Typed text in a form field survives that update, and the log keeps its
//   scroll position: "a draft survives a state change while its field has focus",
//   "the log keeps its scroll position when a poll swaps it"
// - Each of the six content cards collapses to its header, the collapsed cards
//   are remembered per job in localStorage, and they stay collapsed across a
//   poll, a reload and a visit to the board and back.
// - Times render in the browser's time zone on load and after a poll:
//   "times render in the browser's time zone on load and after a poll"
// - GitHub links open in a new tab and factory links do not:
//   "GitHub links open in a new tab and factory links stay in the tab"
import type { Page } from '@playwright/test';
import { ADMIN, expect, test } from '../fixtures';

const STAGES: { stage: string; status: string; detail: string }[] = [
  { stage: 'requested', status: 'queued', detail: 'Waiting for a free worker.' },
  { stage: 'questions', status: 'needs you', detail: 'The agent has questions' },
  { stage: 'spec', status: 'needs you', detail: 'Review the spec below' },
  { stage: 'plan', status: 'needs you', detail: 'Review the plan below' },
  { stage: 'implementing', status: 'agent working', detail: 'The agent is' },
  { stage: 'watching', status: 'in review', detail: 'Pull request #12 waits for CI' },
  { stage: 'merged', status: 'merged', detail: 'Every planned pull request is merged.' },
  { stage: 'failed', status: 'failed', detail: 'the worker ran out of memory' },
  { stage: 'cancelled', status: 'cancelled', detail: 'This job is cancelled.' },
];

function newJob(factory: { fixture(...args: string[]): string }, project: string, stage: string): string {
  return factory.fixture('job', '--project', project, '--stage', stage, '--request', `A ${stage} job`);
}

const status = (page: Page) => page.locator('#job-status');

for (const { stage, status: badge, detail } of STAGES) {
  test(`the page renders a job in the ${stage} stage`, async ({ page, factory, project }) => {
    const id = newJob(factory, project, stage);
    await page.goto(`/jobs/${id}`);

    await expect(page.getByRole('heading', { level: 1 })).toHaveText(`#${id} A ${stage} job`);
    await expect(status(page).locator('.badge').first()).toHaveText(
      stage === 'questions' || stage === 'spec' || stage === 'plan' ? 'needs you' : badge,
    );
    await expect(status(page)).toContainText(detail);
  });
}

test('answering the questions hands the job to the agent', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'questions');
  await page.goto(`/jobs/${id}`);

  await page.locator('#answer_0').fill('The first hub model');
  await page.locator('#answer_1').fill('Yes, one budget');
  await page.getByRole('button', { name: 'Send answers' }).click();

  await expect(page).toHaveURL(new RegExp(`/jobs/${id}$`));
  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(status(page)).toContainText('The agent is reading your answers.');
  await expect(page.getByRole('button', { name: 'Send answers' })).toHaveCount(0);
});

test('approving the spec hands the job to the agent', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'spec');
  await page.goto(`/jobs/${id}`);
  await expect(page.locator('#job-spec')).toContainText('Goal: reboot the device.');

  await page.getByRole('button', { name: 'Approve spec' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(status(page)).toContainText('The agent is writing the plan.');
  await expect(page.getByRole('button', { name: 'Approve spec' })).toHaveCount(0);
});

test('approving the plan starts the implementation', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'plan');
  await page.goto(`/jobs/${id}`);
  await expect(page.locator('#job-plan')).toContainText('maestrod: add a reboot endpoint');

  await page.getByRole('button', { name: 'Approve plan' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(status(page)).toContainText('The agent is implementing the pull request.');
  await expect(page.getByRole('button', { name: 'Approve plan' })).toHaveCount(0);
});

test('requesting changes sends the job back to the agent', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'spec');
  await page.goto(`/jobs/${id}`);

  await page.getByPlaceholder('Or say what should change').fill('Also cover the second hub');
  await page.getByRole('button', { name: 'Request changes' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(status(page)).toContainText('The agent is revising after your feedback.');
  await expect(page.getByRole('button', { name: 'Request changes' })).toHaveCount(0);
});

test('cancelling a job stops it', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'implementing');
  await page.goto(`/jobs/${id}`);

  await page.getByRole('button', { name: 'Cancel job' }).click();

  await expect(status(page).locator('.badge')).toHaveText('cancelled');
  await expect(status(page)).toContainText('This job is cancelled.');
  await expect(page.getByRole('button', { name: 'Cancel job' })).toHaveCount(0);
});

test('retrying a failed job resumes it', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'failed');
  await page.goto(`/jobs/${id}`);

  await page.getByRole('button', { name: 'Retry' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(status(page)).toContainText('The agent is reading the repository.');
  await expect(page.getByText('The job failed')).toHaveCount(0);
});

test('a job whose checks could not run links to the project settings', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'checks-error');
  await page.goto(`/jobs/${id}`);

  await expect(page.getByText('The job failed')).toBeVisible();
  const reason = status(page).locator('.failure-reason');
  await expect(reason).toContainText('could not run');
  await expect(reason).toContainText('command not found');
  await expect(status(page)).toContainText("Revise the project's local checks command, then retry");
  const link = status(page).getByRole('link', { name: `Local checks for ${project}` });
  await expect(link).toHaveAttribute('href', `/settings/projects/${project}`);
  await expect(link).not.toHaveAttribute('target', /.*/);
  await expect(page.getByRole('button', { name: 'Dismiss' })).toBeVisible();

  await page.getByRole('button', { name: 'Retry' }).click();

  await expect(status(page)).toContainText("The factory is running the project's checks.");
  await expect(page.getByText('The job failed')).toHaveCount(0);
});

test('dismissing a failed job takes it off the Failed section', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'failed');
  await page.goto(`/?project=${project}`);
  const failed = page.locator('section.inbox-failed');
  await expect(failed.getByRole('link', { name: 'A failed job' })).toBeVisible();

  await page.goto(`/jobs/${id}`);
  await page.getByRole('button', { name: 'Dismiss' }).click();

  await expect(page.getByText('The job was dismissed')).toBeVisible();
  await expect(status(page).locator('.badge')).toHaveText('dismissed');
  await page.goto(`/?project=${project}`);
  await expect(page.getByRole('link', { name: 'A failed job' })).toBeVisible();
  await expect(failed.getByRole('link', { name: 'A failed job' })).toHaveCount(0);
});

test("editing a queued job's request saves the new text", async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'requested');
  await page.goto(`/jobs/${id}`);

  await page.getByRole('link', { name: 'Edit' }).click();
  await expect(page).toHaveURL(new RegExp(`/jobs/${id}/edit$`));
  await page.locator('#request').fill('A reworded request');
  await page.getByRole('button', { name: 'Save' }).click();

  await expect(page).toHaveURL(new RegExp(`/jobs/${id}$`));
  await expect(page.getByRole('heading', { level: 1 })).toHaveText(`#${id} A reworded request`);
  await expect(page.locator('pre.doc').first()).toHaveText('A reworded request');
});

function waitingJob(
  factory: { fixture(...args: string[]): string },
  project: string,
  first: string,
): string {
  return factory.fixture(
    'job',
    '--project',
    project,
    '--stage',
    'requested',
    '--request',
    'Waits',
    '--after',
    first,
  );
}

test('the edit page ticks the jobs a queued request waits for', async ({ page, factory, project }) => {
  const first = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The first job');
  const second = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The second job');
  const id = waitingJob(factory, project, first);
  await page.goto(`/jobs/${id}/edit`);

  const picker = page.locator('fieldset.start-after');
  await expect(picker.locator('legend')).toHaveText('Start after');
  await expect(picker.getByLabel(`#${first} The first job`)).toBeChecked();
  await expect(picker.getByLabel(`#${second} The second job`)).not.toBeChecked();
  await expect(picker.getByLabel(`#${id}`)).toHaveCount(0);
});

test('saving with the prerequisite unticked lets the job start', async ({ page, factory, project }) => {
  const first = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The first job');
  const id = waitingJob(factory, project, first);
  await page.goto(`/jobs/${id}/edit`);
  await page.locator('fieldset.start-after').getByLabel(`#${first} The first job`).uncheck();
  await page.getByRole('button', { name: 'Save' }).click();

  await expect(page).toHaveURL(new RegExp(`/jobs/${id}$`));
  await expect(status(page).locator('.badge')).toHaveText('queued');
  await expect(status(page)).toContainText('Waiting for a free worker.');
  await expect(page.locator('#job-timeline')).toContainText('You changed the jobs this waits for: none');
  await page.goto(`/?project=${project}`);
  await expect(page.locator('li.job', { hasText: 'Waits' }).locator('.badge')).toHaveText('queued');
});

test('saving with another job selected makes the job wait for it', async ({ page, factory, project }) => {
  const first = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The first job');
  const second = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The second job');
  const id = waitingJob(factory, project, first);
  await page.goto(`/jobs/${id}/edit`);
  const picker = page.locator('fieldset.start-after');
  await picker.getByLabel(`#${first} The first job`).uncheck();
  await picker.getByLabel(`#${second} The second job`).check();
  await page.getByRole('button', { name: 'Save' }).click();

  await expect(page).toHaveURL(new RegExp(`/jobs/${id}$`));
  await expect(status(page).locator('.badge')).toHaveText('waiting');
  await expect(status(page).getByRole('link', { name: `#${second}` })).toHaveAttribute('href', `/jobs/${second}`);
  await expect(status(page).getByRole('link', { name: `#${first}` })).toHaveCount(0);
  await page.goto(`/?project=${project}`);
  await expect(page.locator('li.job', { hasText: 'Waits' })).toContainText(`waits for #${second}`);
});

test.describe('without scripts', () => {
  test.use({ javaScriptEnabled: false });

  test('the edit form changes the jobs a queued request waits for', async ({ page, factory, project }) => {
    const first = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The first job');
    const second = factory.fixture('job', '--project', project, '--stage', 'questions', '--request', 'The second job');
    const id = waitingJob(factory, project, first);
    await page.goto(`/jobs/${id}/edit`);
    const picker = page.locator('fieldset.start-after');
    await expect(picker.getByLabel(`#${first} The first job`)).toBeChecked();
    await picker.getByLabel(`#${first} The first job`).uncheck();
    await picker.getByLabel(`#${second} The second job`).check();
    await page.getByRole('button', { name: 'Save' }).click();

    await expect(page).toHaveURL(new RegExp(`/jobs/${id}$`));
    await expect(status(page).getByRole('link', { name: `#${second}` })).toBeVisible();
  });
});

test('a state change on the server appears without a reload', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'requested');
  await page.goto(`/jobs/${id}`);
  await expect(status(page).locator('.badge')).toHaveText('queued');
  await page.evaluate(() => {
    (window as unknown as { loadedOnce: boolean }).loadedOnce = true;
  });

  factory.fixture('apply', '--job', id, '--event', 'start');

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  expect(await page.evaluate(() => (window as unknown as { loadedOnce?: boolean }).loadedOnce)).toBe(true);
});

test('a waiting job lists its prerequisite and shows it cancelled without a reload', async ({
  page,
  factory,
  project,
}) => {
  const first = newJob(factory, project, 'requested');
  const id = factory.fixture(
    'job',
    '--project',
    project,
    '--stage',
    'requested',
    '--request',
    'Waits',
    '--after',
    first,
  );
  await page.goto(`/jobs/${id}`);
  await expect(status(page).locator('.badge')).toHaveText('waiting');
  await expect(status(page).getByRole('link', { name: `#${first}` })).toHaveAttribute('href', `/jobs/${first}`);
  await expect(status(page)).toContainText('A requested job');
  await expect(status(page)).not.toContainText('Waiting for a free worker.');
  await page.evaluate(() => {
    (window as unknown as { loadedOnce: boolean }).loadedOnce = true;
  });

  factory.fixture('apply', '--job', first, '--event', 'cancel');

  await expect(status(page).locator('.badge')).toHaveText('blocked');
  await expect(status(page)).toContainText(`will not start until #${first} merges or is removed from the list`);
  await expect(status(page)).toContainText('cancelled');
  await expect(status(page).getByRole('link', { name: 'Edit the list' })).toHaveAttribute('href', `/jobs/${id}/edit`);
  expect(await page.evaluate(() => (window as unknown as { loadedOnce?: boolean }).loadedOnce)).toBe(true);
});

test('the prerequisite links stay on the page when the waiting job starts', async ({ page, factory, project }) => {
  const first = newJob(factory, project, 'requested');
  const id = factory.fixture(
    'job',
    '--project',
    project,
    '--stage',
    'requested',
    '--request',
    'Waits',
    '--after',
    first,
  );
  await page.goto(`/jobs/${id}`);
  await expect(status(page).locator('.badge')).toHaveText('waiting');
  await expect(page.locator('#job-queued-after')).toBeEmpty();
  await page.evaluate(() => {
    (window as unknown as { loadedOnce: boolean }).loadedOnce = true;
  });

  factory.fixture('apply', '--job', id, '--event', 'start');

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(page.locator('#job-queued-after')).toContainText('Queued after');
  await expect(page.locator('#job-queued-after').getByRole('link', { name: `#${first}` })).toHaveAttribute(
    'href',
    `/jobs/${first}`,
  );
  expect(await page.evaluate(() => (window as unknown as { loadedOnce?: boolean }).loadedOnce)).toBe(true);
});

test('a draft survives a state change while its field has focus', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'spec');
  await page.goto(`/jobs/${id}`);
  const feedback = page.getByPlaceholder('Or say what should change');
  await feedback.click();
  await feedback.pressSequentially('also cover the second hub');

  factory.fixture('apply', '--job', id, '--event', 'approve');

  await expect(page.locator('#job-timeline li[aria-current="step"] .step-name')).toHaveText('plan');
  await expect(feedback).toHaveValue('also cover the second hub');
  await expect(feedback).toBeFocused();

  await feedback.blur();
  await expect(status(page)).toContainText('The agent is writing the plan.');
  await expect(feedback).toHaveCount(0);
});

test('the log keeps its scroll position when a poll swaps it', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'implementing');
  factory.fixture('log', '--job', id, '--text', 'line', '--count', '80');
  await page.goto(`/jobs/${id}`);
  const log = page.locator('#job-log ol.log');
  await expect(log.locator('li')).toHaveCount(80);

  const scrollable = await log.evaluate((el) => el.scrollHeight - el.clientHeight);
  expect(scrollable).toBeGreaterThan(200);
  await expect.poll(() => log.evaluate((el) => el.scrollHeight - el.clientHeight - el.scrollTop)).toBeLessThan(4);

  await log.evaluate((el) => {
    el.scrollTop = 120;
  });
  factory.fixture('log', '--job', id, '--text', 'more', '--count', '1');
  await expect(log.locator('li')).toHaveCount(81);
  await expect(log).toHaveJSProperty('scrollTop', 120);

  await log.evaluate((el) => {
    el.scrollTop = el.scrollHeight;
  });
  factory.fixture('log', '--job', id, '--text', 'last', '--count', '1');
  await expect(log.locator('li')).toHaveCount(82);
  await expect.poll(() => log.evaluate((el) => el.scrollHeight - el.clientHeight - el.scrollTop)).toBeLessThan(4);
});

test('a reviewed job shows its review pass', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'reviewed');
  await page.goto(`/jobs/${id}`);

  const card = page.locator('#job-reviews');
  await expect(card.getByRole('heading', { name: 'LLM review' })).toBeVisible();
  await expect(card).toContainText('Pass 1');
  await expect(card.locator('.badge')).toHaveText('approved');
});

test('an opened MCP server group stays open across a poll', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'implementing');
  factory.fixture('mcp', '--job', id, '--server', 'github', '--tool', 'get_issue');
  factory.fixture('mcp', '--job', id, '--server', 'github', '--tool', 'list_issues');
  await page.goto(`/jobs/${id}`);

  const card = page.locator('#job-mcp-calls');
  await expect(card.getByRole('heading', { name: 'MCP tool calls' })).toBeVisible();
  await expect(card.locator('.log-count')).toContainText('2 calls');
  const group = card.locator('details.mcp-call-group');
  await group.locator('summary').click();
  await expect(group).toHaveJSProperty('open', true);

  factory.fixture('mcp', '--job', id, '--server', 'github', '--tool', 'create_issue');
  await expect(group.locator('li.mcp-call')).toHaveCount(3);
  await expect(card.locator('.log-count')).toContainText('3 calls');
  await expect(group).toHaveJSProperty('open', true);
});

test.describe('in a time zone that is not UTC', () => {
  test.use({ timezoneId: 'Asia/Kolkata' });

  const local = (iso: string, withSeconds: boolean) =>
    new Intl.DateTimeFormat('en-GB', {
      timeZone: 'Asia/Kolkata',
      hour: '2-digit',
      minute: '2-digit',
      ...(withSeconds ? { second: '2-digit' } : {}),
      hourCycle: 'h23',
    }).format(new Date(iso));

  test("times render in the browser's time zone on load and after a poll", async ({ page, factory, project }) => {
    const id = newJob(factory, project, 'questions');
    await page.goto(`/jobs/${id}`);

    const created = page.locator('p.meta time[data-format="datetime"]').first();
    await expect(created).toHaveText(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d GMT\+5:30$/);
    const iso = (await created.getAttribute('datetime'))!;
    expect((await created.textContent())!).toContain(` ${local(iso, true)} `);
    await expect(created).not.toContainText('UTC');

    factory.fixture('log', '--job', id, '--text', 'after the poll');
    const entry = page.locator('#job-log .log-at time');
    await expect(entry).toHaveCount(1);
    const at = (await entry.getAttribute('datetime'))!;
    await expect(entry).toHaveText(local(at, true));
    expect(at.slice(11, 19)).not.toBe(local(at, true));
  });
});

test('GitHub links open in a new tab and factory links stay in the tab', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'watching');
  await page.goto(`/jobs/${id}`);

  const github = page.locator('a[href^="https://github.com/"]');
  await expect(github).toHaveCount(2);
  await expect(page.locator(`p.meta a[href="https://github.com/e2e/${project}"]`)).toBeVisible();
  await expect(page.getByRole('link', { name: '#12' })).toHaveAttribute(
    'href',
    `https://github.com/e2e/${project}/pull/12`,
  );
  for (const link of await github.all()) {
    await expect(link).toHaveAttribute('target', '_blank');
    await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
  }

  const queued = newJob(factory, project, 'requested');
  await page.goto(`/jobs/${queued}`);
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveAttribute('href', `/jobs/${queued}/edit`);
  const internal = page.locator('a[href^="/"]');
  expect(await internal.count()).toBeGreaterThan(1);
  for (const link of await internal.all()) {
    await expect(link).not.toHaveAttribute('target', '_blank');
  }
});

const SECTIONS: { section: string; heading: string }[] = [
  { section: 'request', heading: 'Request' },
  { section: 'spec', heading: 'Spec' },
  { section: 'plan', heading: 'Plan' },
  { section: 'reviews', heading: 'LLM review' },
  { section: 'mcp-calls', heading: 'MCP tool calls' },
  { section: 'log', heading: 'Log' },
];

// A reviewed job with an MCP call and log lines shows all six cards.
function fullJob(factory: { fixture(...args: string[]): string }, project: string, lines = 3): string {
  const id = newJob(factory, project, 'reviewed');
  factory.fixture('mcp', '--job', id, '--server', 'github', '--tool', 'get_issue');
  factory.fixture('log', '--job', id, '--text', 'line', '--count', String(lines));
  return id;
}

const card = (page: Page, section: string) => page.locator(`[data-section="${section}"]`);
const toggle = (page: Page, section: string) => card(page, section).locator('button.section-toggle');
const body = (page: Page, section: string) => card(page, section).locator('.card-body');
const stored = (page: Page, id: string) => page.evaluate((id) => localStorage.getItem(`factory.job-sections:${id}`), id);

async function expectOnly(page: Page, collapsed: string[]) {
  for (const { section } of SECTIONS) {
    const closed = collapsed.includes(section);
    await expect(toggle(page, section)).toHaveAttribute('aria-expanded', closed ? 'false' : 'true');
    if (closed) await expect(body(page, section)).toBeHidden();
    else await expect(body(page, section)).toBeVisible();
  }
}

test('a card collapses and expands with a click', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  await page.goto(`/jobs/${id}`);
  await expectOnly(page, []);
  await expect(page.locator('#job-status [data-section], #job-timeline [data-section]')).toHaveCount(0);
  await expect(page.locator('#job-status .section-toggle, #job-timeline .section-toggle')).toHaveCount(0);

  for (const { section, heading } of SECTIONS) {
    await toggle(page, section).click();
    await expect(body(page, section)).toBeHidden();
    await expect(card(page, section).getByRole('heading', { name: heading })).toBeVisible();
    await toggle(page, section).click();
    await expect(body(page, section)).toBeVisible();
  }
});

test('a card toggles with Enter and Space', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  await page.goto(`/jobs/${id}`);

  await toggle(page, 'spec').focus();
  await page.keyboard.press('Enter');
  await expect(toggle(page, 'spec')).toHaveAttribute('aria-expanded', 'false');
  await expect(body(page, 'spec')).toBeHidden();
  await page.keyboard.press('Enter');
  await expect(toggle(page, 'spec')).toHaveAttribute('aria-expanded', 'true');
  await page.evaluate(() => {
    (window as unknown as { swaps: number }).swaps = 0;
    document.body.addEventListener('htmx:oobAfterSwap', () => (window as unknown as { swaps: number }).swaps++);
  });
  factory.fixture('log', '--job', id, '--text', 'more', '--count', '1');
  await expect.poll(() => page.evaluate(() => (window as unknown as { swaps: number }).swaps)).toBeGreaterThan(0);
  await page.keyboard.press(' ');
  await expect(toggle(page, 'spec')).toHaveAttribute('aria-expanded', 'false');
  await page.keyboard.press(' ');
  await expect(toggle(page, 'spec')).toHaveAttribute('aria-expanded', 'true');
  await expect(body(page, 'spec')).toBeVisible();
});

test('a collapsed header keeps its counts', async ({ page, factory, project }) => {
  const id = fullJob(factory, project, 5);
  await page.goto(`/jobs/${id}`);

  await toggle(page, 'log').click();
  await toggle(page, 'mcp-calls').click();
  await expect(card(page, 'log').locator('.log-count')).toHaveText('5 lines');
  await expect(card(page, 'mcp-calls').locator('.log-count')).toContainText('1 call');
});

test('the Edit link opens the edit page in a collapsed request card', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'requested');
  await page.goto(`/jobs/${id}`);
  await page.getByRole('link', { name: 'Edit' }).click();
  await expect(page).toHaveURL(new RegExp(`/jobs/${id}/edit$`));

  await page.goto(`/jobs/${id}`);
  await toggle(page, 'request').click();
  await expect(body(page, 'request')).toBeHidden();
  await page.getByRole('link', { name: 'Edit' }).click();
  await expect(page).toHaveURL(new RegExp(`/jobs/${id}/edit$`));
});

test('collapsed cards stay collapsed across a poll', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  await page.goto(`/jobs/${id}`);
  await page.evaluate(() => {
    (window as unknown as { loadedOnce: boolean }).loadedOnce = true;
  });
  await toggle(page, 'log').click();
  await toggle(page, 'spec').click();

  factory.fixture('log', '--job', id, '--text', 'more', '--count', '4');
  await expect(card(page, 'log').locator('.log-count')).toHaveText('7 lines');

  await expectOnly(page, ['log', 'spec']);
  expect(await page.evaluate(() => (window as unknown as { loadedOnce?: boolean }).loadedOnce)).toBe(true);
});

test('a collapsed card is never shown expanded after a swap', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  await page.goto(`/jobs/${id}`);
  await toggle(page, 'log').click();
  await toggle(page, 'spec').click();
  await page.evaluate(() => {
    const seen: string[] = [];
    (window as unknown as { seen: string[] }).seen = seen;
    document.body.addEventListener('htmx:oobAfterSwap', () => {
      for (const section of ['log', 'spec']) {
        const card = document.querySelector(`[data-section="${section}"]`);
        const hidden = card?.querySelector('.card-body')?.hasAttribute('hidden');
        const expanded = card?.querySelector('button.section-toggle')?.getAttribute('aria-expanded');
        seen.push(`${section}:${hidden}:${expanded}`);
      }
    });
  });

  factory.fixture('log', '--job', id, '--text', 'more', '--count', '1');
  await expect(card(page, 'log').locator('.log-count')).toHaveText('4 lines');

  const seen = await page.evaluate(() => (window as unknown as { seen: string[] }).seen);
  expect(seen.length).toBeGreaterThan(0);
  for (const entry of seen) expect(entry).toMatch(/:true:false$/);
});

test('a collapsed card survives a reload and a visit to the board', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  await page.goto(`/jobs/${id}`);
  await toggle(page, 'plan').click();

  await page.reload();
  await expectOnly(page, ['plan']);

  await page.goto('/');
  await page.goto(`/jobs/${id}`);
  await expectOnly(page, ['plan']);

  await toggle(page, 'plan').click();
  await page.reload();
  await expectOnly(page, []);
});

test('collapsing on one job leaves another job expanded', async ({ page, factory, project }) => {
  const a = fullJob(factory, project);
  const b = fullJob(factory, project);
  await page.goto(`/jobs/${a}`);
  await toggle(page, 'spec').click();

  await page.goto(`/jobs/${b}`);
  await expectOnly(page, []);
  expect(await stored(page, b)).toBeNull();
});

test('expanding every card clears the stored entry', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  await page.goto(`/jobs/${id}`);
  await toggle(page, 'spec').click();
  await toggle(page, 'log').click();
  expect(await stored(page, id)).toBe(JSON.stringify(['spec', 'log']));

  await toggle(page, 'spec').click();
  await toggle(page, 'log').click();
  expect(await stored(page, id)).toBeNull();
});

test('a corrupt stored entry leaves every card expanded', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  const errors: Error[] = [];
  page.on('pageerror', (error) => errors.push(error));
  await page.addInitScript((key) => localStorage.setItem(key, '{not json'), `factory.job-sections:${id}`);
  await page.goto(`/jobs/${id}`);

  await expectOnly(page, []);
  expect(errors).toEqual([]);
});

test('an opened MCP server group stays open across collapsing its card', async ({ page, factory, project }) => {
  const id = fullJob(factory, project);
  factory.fixture('mcp', '--job', id, '--server', 'github', '--tool', 'list_issues');
  await page.goto(`/jobs/${id}`);
  const group = card(page, 'mcp-calls').locator('details.mcp-call-group');
  await group.locator('summary').click();
  await expect(group).toHaveJSProperty('open', true);

  await toggle(page, 'mcp-calls').click();
  factory.fixture('mcp', '--job', id, '--server', 'github', '--tool', 'create_issue');
  await expect(card(page, 'mcp-calls').locator('.log-count')).toContainText('3 calls');
  await toggle(page, 'mcp-calls').click();

  await expect(group.locator('li.mcp-call')).toHaveCount(3);
  await expect(group).toHaveJSProperty('open', true);
});

test('the log keeps its place while its card is collapsed', async ({ page, factory, project }) => {
  const id = fullJob(factory, project, 80);
  await page.goto(`/jobs/${id}`);
  const log = page.locator('#job-log ol.log');
  const fromBottom = () => log.evaluate((el) => el.scrollHeight - el.clientHeight - el.scrollTop);
  await expect.poll(fromBottom).toBeLessThan(4);

  await log.evaluate((el) => {
    el.scrollTop = 120;
  });
  await toggle(page, 'log').click();
  factory.fixture('log', '--job', id, '--text', 'more', '--count', '1');
  await expect(card(page, 'log').locator('.log-count')).toHaveText('81 lines');
  await toggle(page, 'log').click();
  await expect(log.locator('li')).toHaveCount(81);
  await expect(log).toHaveJSProperty('scrollTop', 120);

  await log.evaluate((el) => {
    el.scrollTop = el.scrollHeight;
  });
  await toggle(page, 'log').click();
  factory.fixture('log', '--job', id, '--text', 'last', '--count', '1');
  await expect(card(page, 'log').locator('.log-count')).toHaveText('82 lines');
  await toggle(page, 'log').click();
  await expect(log.locator('li')).toHaveCount(82);
  await expect.poll(fromBottom).toBeLessThan(4);
});

test('a job page links its Renovate pull request through the forge', async ({ page, factory, project }) => {
  const id = factory.fixture(
    'renovate',
    '--project',
    project,
    '--number',
    '7',
    '--url',
    'https://stored.example/pull/7',
  );
  await page.goto(`/jobs/${id}`);

  const link = page.locator('p.meta a.renovate-pr');
  await expect(link).toHaveText('#7');
  await expect(link).toHaveAttribute('href', `https://github.com/e2e/${project}/pull/7`);
  await expect(link).toHaveAttribute('target', '_blank');
  await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
});

test('a stopped Renovate job starts a fix job and links to it', async ({ page, factory, project }) => {
  const id = factory.fixture(
    'renovate',
    '--project',
    project,
    '--number',
    '7',
    '--url',
    'https://stored.example/pull/7',
  );
  factory.fixture('apply', '--job', id, '--event', 'renovate-stopped');
  await page.goto(`/jobs/${id}`);

  await expect(page.getByText('Triage stopped')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible();
  await page.locator('textarea[name=instructions]').fill('Update the smoke test');
  await page.getByRole('button', { name: 'Fix in a separate pull request' }).click();

  const waiting = page.locator('.status-detail');
  await expect(waiting).toContainText('Waiting for fix job #');
  await expect(page.getByRole('button', { name: 'Retry' })).toHaveCount(0);
  await expect(page.locator('textarea[name=instructions]')).toHaveCount(0);

  const fix = (await waiting.getByRole('link').getAttribute('href'))!.split('/').pop()!;
  await waiting.getByRole('link').click();
  await expect(page).toHaveURL(/\/jobs\/\d+$/);
  await expect(page.getByText('Update the smoke test').first()).toBeVisible();
  const back = page.locator(`p.meta a[href="/jobs/${id}"]`);
  await expect(back).toBeVisible();
  await expect(back).not.toHaveAttribute('target', '_blank');
  await back.click();
  await expect(page).toHaveURL(new RegExp(`/jobs/${id}$`));

  factory.fixture('apply', '--job', id, '--fix', fix);
  await page.goto(`/jobs/${id}`);
  const stopped = page.locator('.status-detail');
  const rows = await stopped.evaluate((el) => {
    const range = document.createRange();
    const tops = new Set<number>();
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      range.selectNodeContents(n);
      for (const rect of Array.from(range.getClientRects())) tops.add(Math.round(rect.top));
    }
    return tops.size;
  });
  expect(rows).toBeGreaterThan(1);
  await expect(stopped).toContainText(`Fix job #${fix} was cancelled.`);
  await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible();
  await expect(page.locator('textarea[name=instructions]')).toBeVisible();
});

test('a job page on an unknown forge shows the notice and a plain Renovate number', async ({
  page,
  factory,
  project,
}) => {
  const odd = `${project}-odd`;
  factory.fixture('project', '--name', odd, '--login', ADMIN, '--forge', 'elsewhere');
  const id = factory.fixture('renovate', '--project', odd, '--number', '8', '--url', 'https://stored.example/pull/8');
  const waiting = newJob(factory, odd, 'plan');

  for (const job of [id, waiting]) {
    await page.goto(`/jobs/${job}`);
    await expect(page.locator('div.card.failed')).toContainText('elsewhere');
    await expect(page.locator('a.source-link, p.meta a[href^="https://"]')).toHaveCount(0);
  }
  await page.goto(`/jobs/${id}`);
  await expect(page.locator('p.meta .renovate-pr')).toHaveText('#8');
  await expect(page.locator('a.renovate-pr')).toHaveCount(0);
});
