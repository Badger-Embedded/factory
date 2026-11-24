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

  test('p opens the panel in normal mode and Esc returns focus to the button', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('p');
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(page.getByLabel('Find a project')).not.toBeFocused();
    await expect(page.getByLabel('Find a project')).toHaveValue('');
    await expect(page.locator('#picker-list li').first()).toHaveClass(/active/);
    await page.keyboard.press('Escape');
    await expect(page.locator('.picker-panel')).toBeHidden();
    await expect(button(page)).toBeFocused();
    await page.keyboard.press('p');
    await page.keyboard.press('i');
    await expect(page.getByLabel('Find a project')).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(page.locator('.picker-panel')).toBeHidden();
    await expect(button(page)).toBeFocused();
  });

  test('j and k move the highlight in normal mode and stop at the ends', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: null, c: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    const rows = page.locator('#picker-list li');
    const url = page.url();
    await page.keyboard.press('p');
    await page.keyboard.press('k');
    await expect(rows.nth(0)).toHaveClass(/active/);
    await page.keyboard.press('j');
    await page.keyboard.press('j');
    await page.keyboard.press('k');
    await expect(rows.nth(1)).toHaveClass(/active/);
    await expect(page.getByLabel('Find a project')).toHaveValue('');
    await page.keyboard.press('j');
    await page.keyboard.press('j');
    await expect(rows.nth(2)).toHaveClass(/active/);
    expect(page.url()).toBe(url);
  });

  test('ArrowDown and Enter in normal mode open the second row', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: null, c: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('p');
    await page.keyboard.press('ArrowDown');
    await expect(page.locator('#picker-list li').nth(1)).toHaveClass(/active/);
    const id = await page.locator('#picker-list li').nth(1).getAttribute('data-id');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(new RegExp(`[?&]project=${id}$`));
  });

  test('i focuses the search box and j k then type into the filter', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: null, c: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('p');
    await page.keyboard.press('i');
    const box = page.getByLabel('Find a project');
    await expect(box).toBeFocused();
    await expect(box).toHaveValue('');
    await page.keyboard.type('jk');
    await expect(box).toHaveValue('jk');
    await expect(page.locator('#picker-list li:not([hidden])')).toHaveCount(0);
  });

  test('j, k and i do nothing with the panel closed', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    const url = page.url();
    await page.keyboard.press('j');
    await page.keyboard.press('k');
    await page.keyboard.press('i');
    await expect(page.locator('.picker-panel')).toBeHidden();
    expect(page.url()).toBe(url);
  });

  test('p highlights the first row after a poll reordered the list while the panel was closed', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: null, c: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('p');
    await expect(page.locator('#picker-list li.active')).toHaveAttribute('data-id', ids.a);
    await page.keyboard.press('Escape');
    await expect(page.locator('.picker-panel')).toBeHidden();
    factory.fixture('job', '--project', ids.c, '--stage', 'spec', '--request', 'A spec job');
    await expect(page.locator('#picker-list li').first()).toHaveAttribute('data-id', ids.c, { timeout: 15_000 });
    await page.keyboard.press('p');
    await expect(page.locator('#picker-list li.active')).toHaveAttribute('data-id', ids.c);
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(new RegExp(`[?&]project=${ids.c}$`));
  });

  test('a poll keeps normal mode, the highlight and an unfocused box', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: null, c: null });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await page.keyboard.press('p');
    await page.keyboard.press('j');
    const active = page.locator('#picker-list li.active');
    const id = await active.getAttribute('data-id');
    factory.fixture('job', '--project', ids.c, '--stage', 'spec', '--request', 'A spec job');
    await expect(page.locator(`#picker-list li[data-id="${ids.c}"]`)).toHaveAttribute('data-attention', /you|failed/, { timeout: 15_000 });
    await expect(page.locator('.picker-panel')).toBeVisible();
    await expect(page.locator('#picker-list li.active')).toHaveAttribute('data-id', id!);
    await expect(page.getByLabel('Find a project')).not.toBeFocused();
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
    await page.keyboard.type('pnPNjkip');
    await expect(page.locator('#request')).toHaveValue('pnPNjkip');
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
    await expect(footer.locator('kbd')).toHaveText(['↑', '↓', 'J', 'K', 'Enter', 'Esc', 'I', 'N', 'Shift', 'N']);
    for (const text of ['move', 'open', 'close', 'search', 'next that needs you', 'previous']) {
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

    factory.fixture('job', '--project', ids.c, '--stage', 'spec', '--request', 'Another spec job');
    await expect(page.locator('.picker-hint-n.dimmed')).toHaveCount(0, { timeout: 15_000 });
  });
});

test.describe('at an intermediate width', () => {
  test.use({ viewport: { width: 600, height: 800 } });

  test('the open panel stays inside the viewport', async ({ page, factory }, testInfo) => {
    const { login, ids } = setup(factory, testInfo.workerIndex, { a: null, b: 'spec' });
    await page.context().setExtraHTTPHeaders({ 'Tailscale-User-Login': login });
    await page.goto(`/?project=${ids.a}`);
    await button(page).click();
    await expect
      .poll(async () => {
        const box = await page.locator('.picker-panel').boundingBox();
        return box!.x + box!.width;
      })
      .toBeLessThanOrEqual(600);
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth))
      .toBeLessThanOrEqual(0);
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
