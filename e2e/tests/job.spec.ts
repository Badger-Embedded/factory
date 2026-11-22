// Job page (/jobs/{id}) use cases, by test name:
//
// - The page renders for a job in each stage, from requested to cancelled:
//   "the page renders a job in the $stage stage"
// - Answering the questions, approving a spec, approving a plan, requesting
//   changes, cancelling, retrying a failed job and dismissing a failed job each
//   move the job and the page shows it: "answering the questions hands the job
//   to the agent", "approving the spec hands the job to the agent", "approving
//   the plan starts the implementation", "requesting changes sends the job back
//   to the agent", "cancelling a job stops it", "retrying a failed job queues it
//   again", "dismissing a failed job takes it off the Failed section"
// - Editing the request of a queued job saves the new text:
//   "editing a queued job's request saves the new text"
// - A state change on the server shows without a reload:
//   "a state change on the server appears without a reload"
// - Typed text in a form field survives that update, and the log keeps its
//   scroll position: "text typed in an answer field survives an update",
//   "the log keeps its scroll position when a poll swaps it"
// - Times render in the browser's time zone on load and after a poll:
//   "times render in the browser's time zone on load and after a poll"
// - GitHub links open in a new tab and factory links do not:
//   "GitHub links open in a new tab and factory links stay in the tab"
import type { Page } from '@playwright/test';
import { expect, test } from '../fixtures';

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
  await expect(page.getByRole('button', { name: 'Send answers' })).toHaveCount(0);
});

test('approving the spec hands the job to the agent', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'spec');
  await page.goto(`/jobs/${id}`);
  await expect(page.locator('#job-spec')).toContainText('Goal: reboot the device.');

  await page.getByRole('button', { name: 'Approve spec' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(status(page)).toContainText('The agent is');
  await expect(page.getByRole('button', { name: 'Approve spec' })).toHaveCount(0);
});

test('approving the plan starts the implementation', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'plan');
  await page.goto(`/jobs/${id}`);
  await expect(page.locator('#job-plan')).toContainText('maestrod: add a reboot endpoint');

  await page.getByRole('button', { name: 'Approve plan' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
  await expect(page.getByRole('button', { name: 'Approve plan' })).toHaveCount(0);
});

test('requesting changes sends the job back to the agent', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'spec');
  await page.goto(`/jobs/${id}`);

  await page.getByPlaceholder('Or say what should change').fill('Also cover the second hub');
  await page.getByRole('button', { name: 'Request changes' }).click();

  await expect(status(page).locator('.badge')).toHaveText('agent working');
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

test('text typed in an answer field survives an update', async ({ page, factory, project }) => {
  const id = newJob(factory, project, 'questions');
  await page.goto(`/jobs/${id}`);
  const answer = page.locator('#answer_0');
  await answer.click();
  await answer.pressSequentially('half an answer');

  factory.fixture('log', '--job', id, '--text', 'update while typing');
  await expect(page.locator('#job-log .log-text')).toHaveText('update while typing 1');
  await expect(answer).toHaveValue('half an answer');
  await expect(answer).toBeFocused();

  factory.fixture('apply', '--job', id, '--event', 'cancel');
  await answer.blur();
  await expect(status(page).locator('.badge')).toHaveText('cancelled');
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
