import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";
import vm from "node:vm";
import { highlightFixtures, diffPayload } from "./diff-highlight-fixtures.mjs";

const { chromium, webkit } = createRequire(import.meta.url)("playwright");
const source = readFileSync(new URL("../src/diff_ui.rs", import.meta.url), "utf8");
const fragments = [...source.matchAll(/r##"([\s\S]*?)"##/g)].map(match => match[1]);
assert.equal(fragments.length, 2);
const html = fragments[0]
  + readFileSync(new URL("../src/diff_prism.js", import.meta.url), "utf8")
  + fragments[1];
const intraline = vm.runInNewContext(
  source.match(/  const MAX_INTRALINE_\w+ = \d+;/g).join("\n")
  + source.slice(source.indexOf("  function intralineTokens("), source.indexOf("  function annotateIntraline("))
  + "\n({ intralineSegments, mergeTokenSegments })"
);

for (const fixture of highlightFixtures) {
  test(`intraline: ${fixture.title}`, () => {
    const result = intraline.intralineSegments(fixture.before, fixture.after);
    for (const side of ["before", "after"]) {
      assert.equal(result[side].map(segment => segment.text).join(""), fixture[side]);
      assert.deepEqual(Array.from(result[side].filter(segment => segment.changed), segment => segment.text), fixture[`${side}Changes`]);
    }
  });
}

test("intraline: never bridge physical line boundaries or extend a single-sided change", () => {
  for (const gap of ["\n", "\r\n", "\u2028", "\u2029"]) {
    const result = intraline.mergeTokenSegments(["old", gap, "worker"], [true, false, true]);
    assert.equal(result[1].changed, false);
    assert.equal(result[1].text, gap);
  }
  for (const flags of [[true, false, false], [false, false, true], [false, false, false]]) {
    const original = flags.slice();
    const result = intraline.mergeTokenSegments(["old", " ", "worker"], flags);
    assert.deepEqual(flags, original, "Presentation must not alter the original change flags");
    assert.ok(result.filter(segment => segment.changed).every(segment => !segment.text.includes(" ")));
  }
  assert.equal(intraline.intralineSegments("unchanged", "unchanged"), null);
});

test("intraline: the long-line fallback preserves unchanged edges", () => {
  const prefix = "    " + "prefix ".repeat(1200);
  const result = intraline.intralineSegments(prefix + "old worker    ", prefix + "new runner    ");
  assert.equal(result.before[0].text, prefix);
  assert.equal(result.before[0].changed, false);
  assert.equal(result.before.at(-1).text, "er    ");
  assert.equal(result.before.at(-1).changed, false);
});
const lines = [
  [" ", '            subprocess.run([str(hook_test), paf_text, offsets["PAF_EVICT_OFFSET"],'],
  [" ", '                            offsets["PAF_SCAN_OFFSET"], shell_text,'],
  ["-", '                            offsets["SHELL_POOL_INIT_OFFSET"]], check=True)'],
  ["+", '                            offsets["SHELL_POOL_INIT_OFFSET"], offsets["PAF_APPLY_OFFSET"]], check=True)'],
  [" ", "    " + "long_identifier_".repeat(20)],
  [" ", "\t\t\t\t" + "tab_indented_identifier_".repeat(4)],
  [" ", ""],
  [" ", "    return value   "]
];

async function mount(browser, width, theme, extension = "py", fixture = null) {
  const page = await browser.newPage({
    viewport: { width, height: 900 },
    deviceScaleFactor: 2,
    isMobile: width <= 520,
    hasTouch: width <= 520,
    colorScheme: theme
  });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  const path = `test.${extension}`;
  const patch = [
    `diff --git a/${path} b/${path}`, `--- a/${path}`, `+++ b/${path}`,
    "@@ -70,7 +70,7 @@", ...lines.map(([kind, text]) => kind + text), ""
  ].join("\n");
  const payload = fixture ? diffPayload(fixture) : {
    summary: { files: 1, additions: 1, deletions: 1 },
    files: [{ path, status: "modified", additions: 1, deletions: 1 }],
    patch, patchIncluded: true
  };
  const globals = JSON.stringify({
    toolResponseMetadata: { "io.github.devnoname120/codexify/diff": payload }
  });
  await page.setContent(html.replace("<script>", `<script>window.openai = ${globals};</script><script>`));
  await page.locator(".file-summary").click();
  await page.locator(".diff-table").waitFor({ state: "visible" });
  return { page, errors };
}

async function assertRows(page) {
  const rows = await page.locator(".diff-row:not(.hunk) .code").evaluateAll(cells => cells.map(cell => {
    const bounds = cell.getBoundingClientRect();
    const style = getComputedStyle(cell);
    const walker = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT);
    let firstGlyphTop = null;
    let text;
    while ((text = walker.nextNode())) {
      const offset = text.textContent.search(/\S/);
      if (offset < 0) continue;
      const range = document.createRange();
      range.setStart(text, offset);
      range.setEnd(text, offset + 1);
      firstGlyphTop = range.getBoundingClientRect().top - bounds.top;
      break;
    }
    return {
      text: cell.textContent,
      firstGlyphTop,
      height: bounds.height,
      lineHeight: parseFloat(style.lineHeight),
      padding: parseFloat(style.paddingTop) + parseFloat(style.paddingBottom),
      overflow: cell.scrollWidth - cell.clientWidth,
      gutterHeights: [...cell.parentElement.querySelectorAll(".line-number")]
        .map(gutter => gutter.getBoundingClientRect().height)
    };
  }));
  assert.deepEqual(rows.map(row => row.text), lines.map(([, text]) => text));
  for (const row of rows) {
    assert.ok(row.overflow <= 1, `Horizontal overflow: ${JSON.stringify(row)}`);
    assert.ok(row.gutterHeights.every(height => Math.abs(height - row.height) < 1));
    if (row.text.trim()) {
      assert.ok(row.firstGlyphTop !== null && row.firstGlyphTop < row.lineHeight / 2,
        `Whitespace-only first visual line: ${JSON.stringify(row)}`);
    } else {
      assert.ok(row.height <= row.lineHeight + row.padding + 1, "Blank source line gained extra height");
    }
  }
  assert.ok(rows[4].height > rows[4].lineHeight * 2, "Long identifier should still wrap");
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
}

for (const engine of [chromium, webkit]) {
  test(`${engine.name()} diff layout`, async t => {
    const browser = await engine.launch();
    t.after(() => browser.close());
    for (const width of [320, 390, 520, 521, 1024]) {
      for (const theme of ["light", "dark"]) {
        await t.test(`${width}px ${theme}`, async t => {
          const { page, errors } = await mount(browser, width, theme);
          t.after(() => page.close());
          assert.equal(await page.locator(".diff-table").evaluate(node => getComputedStyle(node).fontSize),
            width <= 520 ? "10px" : "12px");
          await assertRows(page);
          assert.ok(await page.locator(".syntax-string").count() > 0);
          assert.ok(await page.locator(".added .word-change").count() > 0);
          assert.deepEqual(errors, []);
        });
      }
    }
    for (const extension of ["py", "txt"]) {
      await t.test(`wrapping at 13px with ${extension} source`, async t => {
        const { page, errors } = await mount(browser, 390, "light", extension);
        t.after(() => page.close());
        await page.addStyleTag({ content: ":root { --diff-font-size: 13px; }" });
        await assertRows(page);
        assert.deepEqual(errors, []);
      });
    }
    for (const fixture of highlightFixtures) {
      await t.test(`continuous highlights: ${fixture.title}`, async t => {
        const { page, errors } = await mount(browser, 390, "light", fixture.extension, fixture);
        t.after(() => page.close());
        const rows = await page.locator(".diff-row:is(.deleted,.added) .code").evaluateAll(cells => cells.map(cell => {
          const walker = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT);
          const runs = [];
          let text;
          while ((text = walker.nextNode())) {
            const changed = Boolean(text.parentElement.closest(".word-change"));
            const last = runs.at(-1);
            if (last && last.changed === changed) last.text += text.textContent;
            else runs.push({ text: text.textContent, changed });
          }
          return { text: cell.textContent, highlighted: runs.filter(run => run.changed).map(run => run.text) };
        }));
        assert.deepEqual(rows, [
          { text: fixture.before, highlighted: fixture.beforeChanges },
          { text: fixture.after, highlighted: fixture.afterChanges }
        ]);
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
        if (fixture.extension === "js") assert.ok(await page.locator(".syntax-string").count() > 0);
        assert.deepEqual(errors, []);
      });
    }
  });
}
