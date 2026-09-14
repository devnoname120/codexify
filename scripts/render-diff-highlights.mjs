import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { highlightFixtures, wrappedFixture, reportedFixture, diffPayload } from "./diff-highlight-fixtures.mjs";

const { chromium } = createRequire(import.meta.url)("playwright");
const root = fileURLToPath(new URL("../", import.meta.url));
const out = fileURLToPath(new URL("../target/diff-highlight-preview/", import.meta.url));
mkdirSync(`${out}/screenshots`, { recursive: true });
const baseline = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim();
const prism = readFileSync(`${root}/src/diff_prism.js`, "utf8");
function widget(source) {
  const fragments = [...source.matchAll(/r##"([\s\S]*?)"##/g)].map(match => match[1]);
  assert.equal(fragments.length, 2);
  return fragments[0] + prism + fragments[1];
}
const data = {
  baseline,
  before: widget(execFileSync("git", ["show", `${baseline}:src/diff_ui.rs`], { cwd: root, encoding: "utf8" })),
  after: widget(readFileSync(`${root}/src/diff_ui.rs`, "utf8")),
  fixtures: [...highlightFixtures, wrappedFixture, reportedFixture].map(fixture => ({ ...fixture, payload: diffPayload(fixture) }))
};
const gallery = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Codexify inline highlights: before / after</title><style>
:root { color-scheme: light; --bg:#fff; --ink:#1c2329; --muted:#5b636c; --line:#d9dde2; }
:root[data-theme="dark"] { color-scheme:dark; --bg:#171717; --ink:#e8ebef; --muted:#a9b0b8; --line:#3d4145; }
* { box-sizing:border-box; } body { margin:0; padding:18px; background:var(--bg); color:var(--ink); font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }
nav { display:flex; flex-wrap:wrap; gap:12px; padding-bottom:18px; margin-bottom:22px; border-bottom:1px solid var(--line); }
label { display:grid; gap:4px; font-size:12px; color:var(--muted); } select { font:inherit; color:var(--ink); background:var(--bg); border:1px solid var(--line); border-radius:6px; padding:6px; max-width:100%; }
section { max-width:1300px; } .eyebrow { margin:0 0 7px; font-size:11px; letter-spacing:.06em; color:var(--muted); }
h1 { font-size:20px; line-height:1.3; margin:0 0 8px; } .description { margin:0 0 22px; color:var(--muted); max-width:760px; }
.views { display:flex; gap:20px; flex-wrap:wrap; align-items:flex-start; } figure { margin:0; width:var(--size); max-width:100%; }
figcaption { font-size:11px; letter-spacing:.06em; font-weight:650; margin-bottom:8px; }
iframe { display:block; width:100%; border:0; min-height:130px; }
footer { color:var(--muted); font-size:11px; border-top:1px solid var(--line); padding-top:10px; margin-top:16px; }
</style></head><body>
<nav><label>Example<select id="fixture"></select></label><label>Theme<select id="theme"><option value="light">Light</option><option value="dark">Dark</option></select></label><label>Widget viewport<select id="width"><option value="390">390 px - mobile</option><option value="640">640 px - desktop</option></select></label></nav>
<section id="comparison"><p class="eyebrow" id="eyebrow"></p><h1 id="title"></h1><p class="description" id="description"></p><div class="views" id="views"></div><footer>Real Codexify widget source with sample diff data. No font, color, or layout overrides inside the widget.</footer></section>
<script id="fixture-data" type="application/json">${JSON.stringify(data).replace(/</g, "\\u003c")}</script>
<script>
const data = JSON.parse(document.getElementById("fixture-data").textContent);
const picker = document.getElementById("fixture"), theme = document.getElementById("theme"), width = document.getElementById("width");
for (const [index, fixture] of data.fixtures.entries()) { const option = document.createElement("option"); option.value = fixture.id; option.textContent = (index + 1) + ". " + fixture.title; picker.append(option); }
let generation = 0;
function render() {
  const current = ++generation, fixture = data.fixtures.find(fixture => fixture.id === picker.value);
  window.previewReady = null;
  document.documentElement.dataset.theme = theme.value;
  document.getElementById("eyebrow").textContent = "INLINE DIFF / " + width.value + " PX / " + theme.value.toUpperCase();
  document.getElementById("title").textContent = fixture.title;
  document.getElementById("description").textContent = fixture.description;
  const views = document.getElementById("views"); views.replaceChildren(); views.style.setProperty("--size", width.value + "px");
  let loaded = 0;
  for (const side of ["before", "after"]) {
    const figure = document.createElement("figure"), label = document.createElement("figcaption"), frame = document.createElement("iframe");
    label.textContent = side === "before" ? "BEFORE / HEAD " + data.baseline.slice(0,7) : "AFTER / UNCOMMITTED CHANGE";
    frame.title = side + " diff";
    const globals = { toolResponseMetadata:{ "io.github.devnoname120/codexify/diff":fixture.payload }, widgetState:{ privateContent:{ diffOpen:true, expandedFiles:["0:" + String.fromCharCode(8594) + fixture.payload.files[0].path] } } };
    const bootstrap = "<script>window.openai=" + JSON.stringify(globals).replace(/</g,"\\u003c") + ";<" + "/script>";
    frame.srcdoc = data[side].replace('<html lang="en">', '<html lang="en" data-theme="' + theme.value + '">').replace("<head>","<head>" + bootstrap);
    frame.addEventListener("load", () => { requestAnimationFrame(() => {
      if (generation !== current) return;
      frame.style.height = Math.ceil(frame.contentDocument.getElementById("root").getBoundingClientRect().height) + "px";
      if (++loaded === 2) window.previewReady = fixture.id + "/" + theme.value + "/" + width.value;
    }); });
    figure.append(label, frame); views.append(figure);
  }
}
window.addEventListener("message", event => {
  const frame = [...document.querySelectorAll("iframe")].find(frame => frame.contentWindow === event.source);
  if (!frame || event.data?.method !== "ui/initialize") return;
  event.source.postMessage({jsonrpc:"2.0",id:event.data.id,result:{protocolVersion:"2026-01-26",hostContext:{theme:theme.value},hostCapabilities:{}}},"*");
});
for (const control of [picker, theme, width]) control.addEventListener("change", render);
render();
</script></body></html>`;
writeFileSync(`${out}/index.html`, gallery);
writeFileSync(`${out}/fixtures.json`, JSON.stringify(data.fixtures, null, 2) + "\n");
const browser = await chromium.launch();
const captures = [];
try {
  const page = await browser.newPage({ viewport: { width: 426, height: 1200 }, deviceScaleFactor: 2 });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(pathToFileURL(`${out}/index.html`).href);
  for (const viewport of [390, 640]) {
    await page.setViewportSize({ width: viewport === 390 ? 426 : 1336, height: 1400 });
    await page.selectOption("#width", String(viewport));
    for (const theme of ["light", "dark"]) {
      await page.selectOption("#theme", theme);
      for (const fixture of data.fixtures) {
        await page.selectOption("#fixture", fixture.id);
        await page.waitForFunction(expected => window.previewReady === expected, `${fixture.id}/${theme}/${viewport}`);
        for (const side of ["before", "after"]) {
          const frame = page.frameLocator(`iframe[title="${side} diff"]`);
          assert.equal((await frame.locator(".diff-row.deleted .code").allTextContents()).join("\n"), fixture.before);
          assert.equal((await frame.locator(".diff-row.added .code").allTextContents()).join("\n"), fixture.after);
        }
        const filename = `${fixture.id}-${viewport}-${theme}.png`;
        await page.locator("#comparison").screenshot({ path: `${out}/screenshots/${filename}` });
        captures.push(filename);
      }
    }
  }
  assert.deepEqual(errors, []);
} finally { await browser.close(); }
writeFileSync(`${out}/README.txt`, `Codexify inline highlight review\n\nBefore: HEAD ${baseline}\nAfter: current uncommitted src/diff_ui.rs\n\nOpen index.html locally for the interactive gallery. It needs no server or network.\nChoose among ${data.fixtures.length} examples, light/dark themes, and 390/640 px viewports.\nScreenshots include the unchanged widget UI in both comparison panels.\nNo installed executable or repository history was changed.\n\n${captures.length} PNG screenshots in screenshots/.\n`);
console.log(JSON.stringify({ directory: out, baseline, screenshots: captures.length, examples: data.fixtures.length }, null, 2));
