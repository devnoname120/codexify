import {afterAll, beforeAll, expect, test} from "bun:test";
import {mkdtemp,rm} from "node:fs/promises";
import {tmpdir} from "node:os";
import {join} from "node:path";
import {chromium,type BrowserContext,type Page} from "patchright";
import {privateChromeExecutable} from "../src/chrome-executable";
import {deliveryTimeoutText,previousResponseDeliveryTimedOut,throwIfSessionAttention} from "../src/browser-session";

const localizedTimeouts=[
  "Cas za dostavo sporocila se je iztekel. Poskusite znova.", // ASCII is not the real locale; must not match
  "\u010cas za dostavo sporo\u010dila se je iztekel. Poskusite znova.",
  "Message delivery timed out. Please try again.",
  "Message delivery has timed out. Retry.",
] as const;

test("recognized response-delivery timeout text is narrowly localized",()=>{
  expect(deliveryTimeoutText.test(localizedTimeouts[0])).toBe(false);
  for(const text of localizedTimeouts.slice(1))expect(deliveryTimeoutText.test(text)).toBe(true);
  for(const text of [
    "Delivery will take longer than expected",
    "Your message was not sent. Please retry.",
    "The connection timed out while reading the repository.",
    "You have reached a conversation limit.",
  ])expect(deliveryTimeoutText.test(text)).toBe(false);
});

const native=process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME==="1";
let context:BrowserContext;
let page:Page;
let profile:string;

beforeAll(async()=>{
  if(!native)return;
  profile=await mkdtemp(join(tmpdir(),"codexify-delivery-timeout-"));
  const executablePath=await privateChromeExecutable(join(import.meta.dirname,"../.browsers",`${process.platform}-${process.arch}`));
  context=await chromium.launchPersistentContext(profile,{channel:"chrome",executablePath,headless:true});
  page=context.pages()[0]!;
  await context.route("https://chatgpt.com/c/timeout-fixture",route=>route.fulfill({contentType:"text/html",body:"<main><form><div contenteditable=true id=prompt-textarea></div></form></main>"}));
  await page.goto("https://chatgpt.com/c/timeout-fixture");
},60_000);

afterAll(async()=>{
  await context?.close();
  if(profile)await rm(profile,{recursive:true,force:true,maxRetries:6,retryDelay:500});
});

test.skipIf(!native)("localized timeout in app-owned alert permits new continuation, never clicks Retry",async()=>{
  for(const message of localizedTimeouts.slice(1)){
    await page.setContent(`<main><form><div id="prompt-textarea" contenteditable="true"></div></form>
      <article data-message-author-role="user">Existing accepted user message</article>
      <div role="alert">${message}<button data-testid="regenerate-thread-error-button">Try again</button></div></main>`);
    expect(await previousResponseDeliveryTimedOut(page)).toBe(true);
    await throwIfSessionAttention(page);
    expect(await page.getByTestId("regenerate-thread-error-button").count()).toBe(1);
  }
},60_000);

test.skipIf(!native)("quoted user content cannot masquerade as an app-owned delivery timeout",async()=>{
  await page.setContent(`<main><form><div id="prompt-textarea" contenteditable="true"></div></form>
    <article data-message-author-role="user"><div role="alert">${localizedTimeouts[1]}</div></article>
    <button data-testid="regenerate-thread-error-button">Try again</button></main>`);
  expect(await previousResponseDeliveryTimedOut(page)).toBe(false);
  await expect(throwIfSessionAttention(page)).rejects.toMatchObject({code:"failed"});
},60_000);

test.skipIf(!native)("visible app Retry button and adjacent localized message work without alert role",async()=>{
  await page.setContent(`<main><form><div id="prompt-textarea" contenteditable="true"></div></form>
    <div class="rounded border"><p>\u010cas za dostavo sporo\u010dila se je iztekel. Poskusite znova.</p>
    <button data-testid="regenerate-thread-error-button" onclick="document.body.dataset.clicked='true'">Poskusi znova</button></div></main>`);
  expect(await previousResponseDeliveryTimedOut(page)).toBe(true);
  await throwIfSessionAttention(page);
  expect(await page.locator("body").getAttribute("data-clicked")).toBeNull();
},60_000);

test.skipIf(!native)("unrelated terminal failure remains a blocker",async()=>{
  await page.setContent(`<main><form><div id="prompt-textarea" contenteditable="true"></div></form>
    <div role="alert">The response failed.</div><button data-testid="regenerate-thread-error-button">Try again</button></main>`);
  expect(await previousResponseDeliveryTimedOut(page)).toBe(false);
  await expect(throwIfSessionAttention(page)).rejects.toMatchObject({code:"failed"});
},60_000);

const timeoutCard=`<div role="alert">${localizedTimeouts[2]}<button data-testid="regenerate-thread-error-button">Retry old response</button></div>`;
const terminalCard='<div role="alert">Something went wrong. See help.openai.com.<button data-testid="regenerate-thread-error-button">Retry current response</button></div>';
for(const [name,cards] of [
  ['timeout before terminal',timeoutCard+terminalCard],
  ['terminal before timeout',terminalCard+timeoutCard],
  ['separate Retry in a shared wrapper',`<section><div role="alert">${localizedTimeouts[2]}</div><div><button data-testid="regenerate-thread-error-button">Retry unrelated response</button></div></section>`],
  ['timeout without ARIA alert',`<div><p>${localizedTimeouts[2]}</p><button data-testid="regenerate-thread-error-button">Retry old response</button></div>`+terminalCard],
] as const)test.skipIf(!native)(`a previous timeout never exempts a different error: ${name}`,async()=>{
    await page.setContent(`<main><form><div id="prompt-textarea" contenteditable="true"></div></form>${cards}</main>`);
    expect(await previousResponseDeliveryTimedOut(page)).toBe(true);
    await expect(throwIfSessionAttention(page)).rejects.toMatchObject({code:"failed"});
},30_000);
