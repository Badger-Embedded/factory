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

const SERVER = 'http://gfx1201:9931';

test('switching to Local offers the stored models and switching back restores the others', async ({
  page,
  own,
}) => {
  own.fixture('local-models', '--url', SERVER, '--model', 'gemma-4-26B', '--model', 'Llama-3.2-1B');
  await page.goto('/settings/projects/maestro?stage=spec');
  const spec = page.locator('[data-panel="spec"]');
  const chips = spec.locator('.model-chips .chip');
  const efforts = spec.locator('[data-effort-seg] label');
  await expect(chips).toHaveText(['CLI default', 'fable', 'opus', 'sonnet', 'haiku']);

  await spec.getByRole('button', { name: 'opus' }).click();
  const label = page.locator('.pipeline [data-part="spec"] [data-model-label]');
  await spec.getByRole('radio', { name: 'Local' }).check();
  await expect(label).toHaveText('Model required');
  await expect(chips).toHaveText(['gemma-4-26B', 'Llama-3.2-1B']);
  await expect(spec.locator('datalist option')).toHaveCount(2);
  await expect(spec.getByLabel('Model')).toHaveValue('');
  await expect(spec.locator('[data-model-hint]')).toHaveText(
    `Models served by ${SERVER}. A Local stage needs a model.`,
  );
  await expect(efforts).toHaveText(['low', 'medium', 'high', 'xhigh']);
  await expect(spec.getByRole('radio', { name: 'medium' })).toBeChecked();

  await chips.first().click();
  await expect(spec.getByLabel('Model')).toHaveValue('gemma-4-26B');
  await expect(label).toHaveText('gemma-4-26B');

  await spec.getByRole('radio', { name: 'Claude' }).check();
  await expect(chips).toHaveText(['CLI default', 'fable', 'opus', 'sonnet', 'haiku']);
  await expect(efforts).toHaveText(['low', 'medium', 'high', 'xhigh', 'max']);
  await expect(spec.getByLabel('Model')).toHaveValue('');
  await expect(label).toHaveText('CLI default');
  await expect(spec.locator('[data-model-hint]')).toContainText('installed claude CLI');
});

test('Local offers no chips when no list is stored', async ({ page, own }) => {
  own.fixture('local-models', '--url', SERVER);
  await page.goto('/settings/projects/maestro?stage=spec');
  const spec = page.locator('[data-panel="spec"]');
  await spec.getByRole('radio', { name: 'Local' }).check();
  await expect(page.locator('.pipeline [data-part="spec"] [data-model-label]')).toHaveText(
    'Model required',
  );
  await expect(spec.locator('.model-chips .chip')).toHaveCount(0);
  await expect(spec.locator('[data-model-hint]')).toContainText(`No model list is stored for ${SERVER}.`);
  await spec.getByLabel('Model').fill('any-model');
  await expect(spec.getByLabel('Model')).toHaveValue('any-model');
});
