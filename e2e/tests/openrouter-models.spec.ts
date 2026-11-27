import { ADMIN, expect, startFactory, test as base, type Factory } from '../fixtures';

const test = base.extend<{ own: Factory }>({
  own: async ({}, use) => {
    const { factory, stop } = await startFactory();
    await use(factory);
    await stop();
  },
  baseURL: async ({ own }, use) => {
    await use(own.url);
  },
  extraHTTPHeaders: async ({}, use) => {
    await use({ 'Tailscale-User-Login': ADMIN });
  },
});

function seed(own: Factory, count = 2) {
  const models = ['openai/gpt-5|OpenAI: GPT-5', 'x-ai/grok-4|xAI: Grok 4'];
  for (let i = 0; i < count - 2; i++) models.push(`acme/model-${i}|Acme Model ${i}`);
  own.fixture('openrouter-models', ...models.flatMap((model) => ['--model', model]));
}

test('typing in the model field lists matches from the stored models and picking one fills it', async ({
  page,
  own,
}) => {
  seed(own, 40);
  const requests: string[] = [];
  page.on('request', (request) => requests.push(new URL(request.url()).hostname));
  await page.goto('/settings/projects/maestro?stage=spec');
  const spec = page.locator('[data-panel="spec"]');
  const field = spec.getByLabel('Model');
  const results = spec.locator('[data-model-results] li');
  const label = page.locator('.pipeline [data-part="spec"] [data-model-label]');

  await spec.getByRole('radio', { name: 'OpenRouter' }).check();
  await expect(label).toHaveText('Model required');
  await expect(spec.locator('.model-chips .chip')).toHaveCount(0);
  await expect(spec.locator('[data-model-hint]')).toHaveText('Search 40 OpenRouter models.');

  await field.fill('GROK');
  await expect(results).toHaveText(['x-ai/grok-4 (xAI: Grok 4)']);
  await field.fill('acme');
  await expect(results).toHaveCount(20);
  await field.fill('openai: gpt');
  await results.first().click();
  await expect(field).toHaveValue('openai/gpt-5');
  await expect(label).toHaveText('openai/gpt-5');
  await expect(results).toHaveCount(0);

  await field.fill('grok');
  await field.press('ArrowDown');
  await field.press('Enter');
  await expect(field).toHaveValue('x-ai/grok-4');
  expect(requests.filter((host) => host.includes('openrouter'))).toEqual([]);
});

test('an id that is not in the list saves, and switching agents redraws the field without a reload', async ({
  page,
  own,
}) => {
  seed(own);
  await page.goto('/settings/projects/maestro?stage=spec');
  const spec = page.locator('[data-panel="spec"]');
  const field = spec.getByLabel('Model');
  const chips = spec.locator('.model-chips .chip');

  await spec.getByRole('radio', { name: 'OpenRouter' }).check();
  await field.fill('x-ai/grok-4');
  await spec.getByRole('radio', { name: 'Claude' }).check();
  await expect(chips).toHaveText(['CLI default', 'fable', 'opus', 'sonnet', 'haiku']);
  await expect(field).toHaveValue('');
  await expect(spec.locator('[data-model-results] li')).toHaveCount(0);
  await spec.getByRole('radio', { name: 'OpenRouter' }).check();
  await expect(chips).toHaveCount(0);

  await field.fill('openai/gpt-5:nitro');
  await page.getByRole('button', { name: 'Save maestro' }).click();
  await page.goto('/settings/projects/maestro?stage=spec');
  await expect(page.locator('input[name="spec_agent"][value="openrouter"]')).toBeChecked();
  await expect(spec.getByLabel('Model')).toHaveValue('openai/gpt-5:nitro');
});

test('the Renovate agent card searches the stored models too', async ({ page, own }) => {
  seed(own);
  await page.goto('/settings/projects/maestro');
  const card = page.locator('.renovate-agent-form');
  const field = card.getByLabel('Model');
  await card.getByRole('checkbox', { name: /separate agent for Renovate/ }).check();
  await card.getByRole('radio', { name: 'OpenRouter' }).check();
  await expect(card.locator('.model-chips .chip')).toHaveCount(0);
  await expect(card.locator('[data-model-hint]')).toHaveText('Search 2 OpenRouter models.');
  await field.fill('grok');
  await card.locator('[data-model-results] li').first().click();
  await expect(field).toHaveValue('x-ai/grok-4');

  await card.getByRole('radio', { name: 'Claude' }).check();
  await expect(card.locator('.model-chips .chip')).toHaveText(['CLI default', 'fable', 'opus', 'sonnet', 'haiku']);
  await expect(field).toHaveValue('');
  await card.getByRole('radio', { name: 'OpenRouter' }).check();
  await field.fill('custom/model:free');
  await card.getByRole('button', { name: 'Save Renovate agent' }).click();
  await expect(page.getByText('Saved.')).toBeVisible();
  await page.reload();
  await expect(field).toHaveValue('custom/model:free');
});

test('arrowing past the visible rows scrolls the active result into view and Escape closes the list', async ({
  page,
  own,
}) => {
  seed(own, 40);
  await page.goto('/settings/projects/maestro?stage=spec');
  const spec = page.locator('[data-panel="spec"]');
  const field = spec.getByLabel('Model');
  const list = spec.locator('[data-model-results]');
  await spec.getByRole('radio', { name: 'OpenRouter' }).check();
  await expect(field).toHaveAttribute('role', 'combobox');
  await expect(field).not.toHaveAttribute('list', /.*/);
  await field.fill('acme');
  await expect(field).toHaveAttribute('aria-expanded', 'true');
  for (let i = 0; i < 20; i++) await field.press('ArrowDown');
  const active = spec.locator('[data-model-results] li[aria-selected="true"]');
  await expect(active).toHaveText('acme/model-19 (Acme Model 19)');
  await expect(active).toBeInViewport();
  expect(await list.evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
  await field.press('Escape');
  await expect(list).toBeHidden();
  await expect(field).toHaveAttribute('aria-expanded', 'false');
  await expect(field).not.toHaveAttribute('aria-activedescendant', /.*/);

  await spec.getByRole('radio', { name: 'Claude' }).check();
  await expect(field).toHaveAttribute('list', 'model-suggestions-spec');
  await expect(field).not.toHaveAttribute('role', /.*/);
  await expect(spec.locator('#model-suggestions-spec option')).toHaveCount(4);
});
