import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium } from "patchright";
import { installMinimalChatGptPage } from "../src/minimal-page";
import { privateChromeExecutable } from "../src/chrome-executable";
import { captureSubmissionBaseline, readTurnDomState } from "../src/turn-binding";
import {responseDomSnapshot, type ChatGptResponseDomCache} from "../src/response-observer";

// Runs under Node, not Bun: Patchright's Windows Chrome pipe can hang under Bun.
const profile = await mkdtemp(join(tmpdir(), "codexify-long-chat-"));
const executablePath = process.env.CODEXIFY_TEST_CHROME_PATH
  ?? await privateChromeExecutable(resolve(".browsers", `${process.platform}-${process.arch}`));
let context: Awaited<ReturnType<typeof chromium.launchPersistentContext>> | undefined;
try {
  context = await chromium.launchPersistentContext(profile, {
    executablePath, headless: true, timeout: 20_000,
  });
  await installMinimalChatGptPage(context);
  const page = context.pages()[0] ?? await context.newPage();
  await page.route("https://chatgpt.com/**", route => route.fulfill({
    contentType: "text/html",
    body: `<!doctype html><html><head></head><body>
      <nav aria-label="Chat history">History</nav>
      <main><div id="conversation"></div></main>
      <form><div id="prompt-textarea" contenteditable="true"></div>
        <button type="button" data-testid="end-turn-button">End turn</button>
        <button type="button" data-testid="stop-button">Stop</button>
        <button type="button" data-testid="copy-turn-action-button">Copy</button>
      </form></body></html>`,
  }));
  await page.goto("https://chatgpt.com/c/long-fixture", { timeout: 10_000 });
  await page.evaluate(() => {
    const host = document.querySelector("#conversation")!;
    for (let i = 0; i < 650; i++) {
      const group = document.createElement("div");
      group.setAttribute("data-turn-key", `old-${i}`);
      group.innerHTML = '<div data-user-message-bubble>Question</div><div data-conversation-role="assistant"><p class="markdown">Answer</p></div>';
      host.appendChild(group);
    }
  });
  const start = performance.now();
  const baseline = await readTurnDomState(page);
  const firstScanMs = performance.now() - start;
  assert.equal(baseline.responseIdentities.length, 650);
  assert.equal(baseline.generationRunning, true);
  const scans = async () => page.evaluate(() => (globalThis as any).__CODEXIFY_TURN_IDENTITIES__.scans as number);
  assert.equal(await scans(), 1);
  const repeatStart = performance.now();
  for (let i = 0; i < 20; i++) {
    await page.evaluate(value => {
      document.querySelector(".markdown")!.textContent = `Streaming answer ${value}`;
    }, i);
    const state = await readTurnDomState(page);
    assert.deepEqual(state.responseIdentities, baseline.responseIdentities);
  }
  const repeatPollMs = performance.now() - repeatStart;
  assert.equal(await scans(), 1);
  for (let i = 0; i < 20; i++) {
    await page.evaluate(value => document.querySelector("#prompt-textarea")!.setAttribute("data-testid", `composer-status-${value}`), i);
    await readTurnDomState(page);
  }
  assert.equal(await scans(), 1, "Composer status changes must not scan 650 previous turns");
  const submission = await captureSubmissionBaseline(page, "Follow-up message");
  assert.equal((await readTurnDomState(page, submission.browserBaselineId!)).newResponseIdentities.length, 0);
  await page.locator('[data-testid="stop-button"]').evaluate(button => button.remove());
  assert.equal((await readTurnDomState(page)).generationRunning, true, "End turn remains a live Stop control");
  assert.equal(await scans(), 1);
  await page.evaluate(() => {
    const node = document.createElement("div");
    node.setAttribute("data-turn-key", "new");
    node.innerHTML = '<div data-user-message-bubble>New question</div><div data-conversation-role="assistant">Answer</div>';
    document.querySelector("#conversation")!.appendChild(node);
  });
  assert.equal((await readTurnDomState(page)).responseIdentities.at(-1), "group:assistant:new");
  assert.deepEqual((await readTurnDomState(page, submission.browserBaselineId!)).newResponseIdentities, ["group:assistant:new"]);
  assert.equal(await scans(), 2);
  await page.evaluate(() => history.pushState({}, "", "/c/different"));
  assert.equal((await readTurnDomState(page)).responseIdentities.length, 651);
  assert.equal(await scans(), 3);
  const display = async (selector: string) => page.locator(selector).evaluate(n => getComputedStyle(n).display);
  assert.equal(await display('nav[aria-label="Chat history"]'), "none");
  assert.notEqual(await display('[data-testid="end-turn-button"]'), "none");
  await page.locator('[data-testid="end-turn-button"]').evaluate(button => button.remove());
  assert.equal((await readTurnDomState(page)).generationRunning, false);
  assert.notEqual(await display('[data-testid="copy-turn-action-button"]'), "none");
  assert.notEqual(await display("#prompt-textarea"), "none");
  const fullScans = await scans();
  const forcedStart = performance.now();
  for (let i = 0; i < 20; i++) {
    await page.evaluate(() => { (globalThis as any).__CODEXIFY_TURN_IDENTITIES__.dirty = true; });
    await readTurnDomState(page);
  }
  const forcedPollMs = performance.now() - forcedStart;
  assert.equal(await scans(), fullScans + 20);
  // ChatGPT sometimes inserts a grouped user shell before attaching its
  // assistant response. The response must still be considered a new turn.
  await page.evaluate(() => {
    const group = document.createElement("div");
    group.setAttribute("data-turn-key", "optimistic-shell");
    group.innerHTML = '<div data-user-message-bubble>Pending follow-up</div>';
    document.querySelector("#conversation")!.appendChild(group);
  });
  const optimisticBaseline = await captureSubmissionBaseline(page, "New follow-up");
  assert.equal(optimisticBaseline.initialTurnIdentities.includes("group:assistant:optimistic-shell"), false);
  await page.evaluate(() => {
    const answer = document.createElement("div");
    answer.setAttribute("data-conversation-role", "assistant");
    answer.textContent = "Response appeared";
    document.querySelector('[data-turn-key="optimistic-shell"]')!.appendChild(answer);
  });
  assert.deepEqual((await readTurnDomState(page, optimisticBaseline.browserBaselineId!)).newResponseIdentities, ["group:assistant:optimistic-shell"]);
  await page.evaluate(() => {
    const group = document.createElement("div");
    group.setAttribute("data-turn-key", "empty-shell");
    document.querySelector("#conversation")!.appendChild(group);
  });
  const emptyBaseline = await captureSubmissionBaseline(page, "Another follow-up");
  assert.equal(emptyBaseline.initialTurnIdentities.includes("group:user:empty-shell"), false);
  await page.evaluate(() => {
    const bubble = document.createElement("div");
    bubble.setAttribute("data-user-message-bubble", "");
    bubble.textContent = "Now actually sent";
    document.querySelector('[data-turn-key="empty-shell"]')!.appendChild(bubble);
  });
  assert.deepEqual((await readTurnDomState(page, emptyBaseline.browserBaselineId!)).newUserIdentities, ["group:user:empty-shell"]);
  await page.evaluate(() => {
    const answer = document.createElement("section");
    answer.id = "performance-answer";
    answer.innerHTML = Array.from({length:200}, (_, i) => `<div class="markdown"><p>Visible chunk ${i}</p></div>`).join("");
    document.querySelector("main")!.appendChild(answer);
    const original = getComputedStyle;
    (globalThis as any).__styleReads = 0;
    globalThis.getComputedStyle = (...args: Parameters<typeof getComputedStyle>) => {
      (globalThis as any).__styleReads++;
      return original(...args);
    };
  });
  const answer = page.locator("#performance-answer");
  const answerCache: ChatGptResponseDomCache = {};
  assert.ok((await responseDomSnapshot(answer, answerCache)).visibleText.includes("Visible chunk 199"));
  await page.evaluate(() => { (globalThis as any).__styleReads = 0; });
  assert.ok((await responseDomSnapshot(answer, answerCache)).visibleText.includes("Visible chunk 199"));
  const cachedStyleReads = await page.evaluate(() => (globalThis as any).__styleReads as number);
  assert.ok(cachedStyleReads <= 210, `Shared ancestors were resolved repeatedly: ${cachedStyleReads}`);
  assert.equal(answerCache.fullScans, 1);
  assert.equal(answerCache.cacheHits, 1);
  await answer.evaluate(element => { element.style.display = "none"; });
  assert.equal((await responseDomSnapshot(answer, answerCache)).visibleText, "");
  await answer.evaluate(element => { element.style.display = ""; });
  assert.ok((await responseDomSnapshot(answer, answerCache)).visibleText.includes("Visible chunk 199"));
  console.log(JSON.stringify({
    ok:true, turns:650, firstScanMs:Math.round(firstScanMs),
    cached20PollsMs:Math.round(repeatPollMs),
    forced20PollsMs:Math.round(forcedPollMs),
    fullScansBeforeBenchmark:fullScans,
    fullScansAfterBenchmark:await scans(),
    cachedStyleReads,
  }));
} finally {
  if (context) await context.close();
  await rm(profile, { recursive:true, force:true });
}
