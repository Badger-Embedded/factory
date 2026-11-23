// Keyboard shortcuts of the project picker and the key caps that show them.
//
// - p opens the picker with the search box focused and Esc closes it:
//   "p opens the panel with the search box focused and Esc returns focus to the button"
// - n and Shift+N load the next and previous project that needs attention, wrapping:
//   "n and Shift+N jump between projects that need attention and wrap around"
// - Typing p or n in the request box only types the letter:
//   "typing p or n in the request box does nothing else"
// - The caps are <kbd> elements, the button keeps its name, and the N hints dim
//   when no other project needs attention:
//   "the button and the panel footer show key caps", "the N hints dim with no other project that needs attention"
// - A touch device and a page without JavaScript show no caps:
//   "a touch device shows no key caps", "without JavaScript no key cap is visible"
import type { Page } from '@playwright/test';
import { expect, test, type Factory } from '../fixtures';

let counter = 0;

function setup(factory: Factory, workerIndex: number, stages: Record<string, string | null>) {
  const login = `keys-${workerIndex}-${counter++}@e2e.test`;
  const tag = login.split('@')[0];
  const ids: Record<string, string> = {};
  for (const [name, stage] of Object.entries(stages)) {
    const id = `${name}${tag}`;
    ids[name] = id;
    factory.fixture('project', '--name', id, '--login', login);
    if (stage) factory.fixture('job', '--project', id, '--stage', stage, '--request', `A ${stage} job`);
  }
  return { login, ids };
}

const button = (page: Page) => page.getByRole('button', { name: 'Project', exact: true });

test.describe('with a desktop pointer', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('p opens the panel with the search box focused and Esc returns focus to the button', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('p');
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(page.getByLabel('Find a project')).toBeFocused();
    await expect(page.getByLabel('Find a project')).toHaveValue('');
    await page.keyboard.press('Escape');
    await expect(page.locator('.picker-panel')).toBeHidden();
    await expect(button(page)).toBeFocused();
  });

  test('n and Shift+N jump between projects that need attention and wrap around', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec', c: null, d: 'failed' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    const at = (name: string) => new RegExp(`[?&]project=${ids[name]}$`);
    // Picker order: b (needs you), d (failed), a, c.
    await page.goto(`/?project=${ids.b}`);
    await page.keyboard.press('n');
    await expect(page).toHaveURL(at('d'));
    await page.keyboard.press('n');
    await expect(page).toHaveURL(at('b'));
    await page.keyboard.press('Shift+N');
    await expect(page).toHaveURL(at('d'));
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('n');
    await expect(page).toHaveURL(at('b'));
    await page.goto(`/?project=${ids.c}`);
    await page.keyboard.press('Shift+N');
    await expect(page).toHaveURL(at('d'));
  });

  test('typing p or n in the request box does nothing else', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.locator('#request').click();
    await page.keyboard.type('pnPN');
    await expect(page.locator('#request')).toHaveValue('pnPN');
    await expect(page.locator('.picker-panel')).toBeHidden();
    await expect(page).toHaveURL(new RegExp(`[?&]project=${ids.a}$`));
  });

  test('the button and the panel footer show key caps', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await expect(button(page)).toHaveAttribute('aria-keyshortcuts', 'p');
    const cap = button(page).locator('kbd');
    await expect(cap).toBeVisible();
    await expect(cap).toHaveText('P');

    await button(page).click();
    const footer = page.locator('.picker-keys');
    await expect(footer.locator('kbd')).toHaveText(['↑', '↓', 'Enter', 'Esc', 'N', 'Shift', 'N']);
    for (const text of ['move', 'open', 'close', 'next that needs you', 'previous']) {
      await expect(footer).toContainText(text);
    }
    const style = await footer.locator('kbd').first().evaluate((el) => {
      const own = getComputedStyle(el);
      const around = getComputedStyle(el.parentElement!);
      return {
        border: own.borderTopWidth,
        background: own.backgroundColor,
        around: around.backgroundColor,
        font: own.fontFamily,
        aroundFont: around.fontFamily,
      };
    });
    expect(parseFloat(style.border)).toBeGreaterThan(0);
    expect(style.background).not.toBe(style.around);
    expect(style.font).not.toBe(style.aroundFont);
    const oneLine = await footer.evaluate((el) => ({ scroll: el.scrollWidth, client: el.clientWidth, height: el.clientHeight }));
    expect(oneLine.scroll).toBeLessThanOrEqual(oneLine.client);
    expect(oneLine.height).toBeLessThan(40);
  });

  test('the N hints dim with no other project that needs attention', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec', c: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await button(page).click();
    await expect(page.locator('.picker-hint-n.dimmed')).toHaveCount(0);

    await page.goto(`/?project=${ids.b}`);
    await button(page).click();
    await expect(page.locator('.picker-hint-n.dimmed')).toHaveCount(2);
    await expect(page.locator('.picker-hint:not(.picker-hint-n).dimmed')).toHaveCount(0);

    // The dimming follows the five-second refresh.
    factory.fixture('job', '--project', ids.c, '--stage', 'spec', '--request', 'Another spec job');
    await expect(page.locator('.picker-hint-n.dimmed')).toHaveCount(0, { timeout: 15_000 });
  });
});

test.describe('on a touch device', () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test('a touch device shows no key caps', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    expect(await page.evaluate(() => matchMedia('(hover: none)').matches)).toBe(true);
    await expect(button(page)).toBeVisible();
    await expect(page.locator('kbd:visible')).toHaveCount(0);
    await button(page).click();
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(page.locator('kbd:visible')).toHaveCount(0);
    await expect(page.locator('.picker-keys')).toBeHidden();
  });
});

test.describe('without JavaScript', () => {
  test.use({ viewport: { width: 1280, height: 800 }, javaScriptEnabled: false });

  test('without JavaScript no key cap is visible', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await button(page).click();
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(page.locator('kbd:visible')).toHaveCount(0);
  });
});
