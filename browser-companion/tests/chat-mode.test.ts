import { expect, test } from "bun:test";
import { runInNewContext } from "node:vm";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium } from "patchright";
import { installChatModeCompatibility, selectChatMode } from "../src/chat-mode";
import { privateChromeExecutable } from "../src/chrome-executable";

const layers: Record<string, Record<string, unknown>> = {
  "3040264819":Object.freeze({merged_picker:true, merged_picker_existing_chats:true, multi_model_default:true}),
  "1922887554":Object.freeze({chat_override_duration_hours:0, unrelated:"retained"}),
  other:Object.freeze({merged_picker:true, enabled:true}),
};

function environment(origin = "https://chatgpt.com") {
  const window: Record<string, any> = {};
  window.top = window;
  const context = {window, location:{origin}};
  return {window, install:() => runInNewContext(`(${installChatModeCompatibility.toString()})()`, context)};
}

function fakeClient(register: (value: any) => void = () => {}) {
  const client: Record<string, any> = {loadingStatus:"Ready", events:[], calls:[]};
  // Match the real SDK: registration precedes the getFeatureGate assignment.
  register(client);
  client.getFeatureGate = function(name: string, options?: unknown) {
    expect(this).toBe(client);
    client.calls.push({name, options});
    return Object.freeze({name, ruleID:"original-rule", value:true});
  };
  client.getLayer = function(name: string, options?: unknown) {
    expect(this).toBe(client);
    client.calls.push({name, options});
    return {
      name, ruleID:"original-rule", __value:layers[name],
      get(parameter: string, fallback: unknown) { return layers[name]?.[parameter] ?? fallback; },
    };
  };
  client.checkGate = (name: string) => client.getFeatureGate(name).value;
  client.getContext = () => ({values:"original-evaluations"});
  client.$emt = (event: unknown) => client.events.push(event);
  return client;
}

function assertChatOnly(client: Record<string, any>) {
  const options = {disableExposureLog:true};
  const gate = client.getFeatureGate("1625797156", options);
  expect(gate.value).toBeFalse();
  expect(gate.ruleID).toBe("original-rule");
  expect(client.calls.at(-1)).toEqual({name:"1625797156", options});
  expect(client.checkGate("1625797156")).toBeFalse();
  const picker = client.getLayer("3040264819", options);
  expect(picker.get("merged_picker", true)).toBeTrue();
  expect(picker.get("merged_picker_existing_chats", true)).toBeTrue();
  expect(picker.get("multi_model_default", false)).toBeTrue();
  expect(picker.ruleID).toBe("original-rule");
  expect(picker.__value).toBe(layers["3040264819"]);
  expect(client.calls.at(-1)).toEqual({name:"3040264819", options});
  expect(client.getLayer("1922887554").get("chat_override_duration_hours", 4)).toBe(0);
  expect(client.getLayer("1922887554").get("unrelated", null)).toBe("retained");
  expect(client.getLayer("other").get("merged_picker", false)).toBeTrue();
  expect(client.checkGate("account-access")).toBeTrue();
  expect(layers["3040264819"]!.merged_picker).toBeTrue();
  expect(layers["1922887554"]!.chat_override_duration_hours).toBe(0);
}

test("Chat-only compatibility hooks SDK registration before getFeatureGate is assigned", () => {
  const {window, install} = environment();
  install();
  window.__STATSIG__ = {};
  const client = fakeClient(value => { window.__STATSIG__.firstInstance = value; });
  assertChatOnly(client);
  const replacement = () => ({value:true});
  client.getFeatureGate = replacement;
  expect(client.checkGate("1625797156")).toBeFalse();
});

test("restored pages update once without changing account evaluations or other gates", () => {
  const {window, install} = environment();
  const client = fakeClient();
  window.__STATSIG__ = {firstInstance:client};
  install();
  assertChatOnly(client);
  expect(client.events).toEqual([{name:"values_updated", status:"Ready", values:"original-evaluations"}]);
  const wrapped = client.getFeatureGate;
  install();
  expect(client.getFeatureGate).toBe(wrapped);
  expect(client.events).toHaveLength(1);
});

test("SDK registry replacements and additional instances keep the same UI preference", () => {
  const {window, install} = environment();
  install();
  window.__STATSIG__ = {instances:{}};
  const first = fakeClient(value => { window.__STATSIG__.instances.first = value; });
  assertChatOnly(first);
  const next = fakeClient();
  window.__STATSIG__.instances = {next};
  assertChatOnly(next);
  const replacement = fakeClient();
  window.__STATSIG__ = {firstInstance:replacement};
  assertChatOnly(replacement);
});

test("other origins and embedded widgets receive no mode instrumentation", () => {
  for (const origin of ["https://example.test", "https://chatgpt.com.example.test", "https://auth.openai.com"]) {
    const {window, install} = environment(origin);
    install();
    expect(Object.hasOwn(window, "__STATSIG__")).toBeFalse();
  }
  const {window, install} = environment();
  window.top = {};
  install();
  expect(Object.hasOwn(window, "__STATSIG__")).toBeFalse();
});

test.skipIf(process.env.CODEXIFY_TEST_BROWSER_COMPANION_CHROME !== "1")(
  "Chrome installs the mode preference before application scripts and preserves it on reload",
  async () => {
    const profile = await mkdtemp(join(tmpdir(), "chat-mode-compatibility-"));
    const executablePath = await privateChromeExecutable(join(import.meta.dirname, "../.browsers", `${process.platform}-${process.arch}`));
    const context = await chromium.launchPersistentContext(profile, {channel:"chrome", executablePath, headless:true});
    try {
      await context.addInitScript(installChatModeCompatibility);
      await context.route("https://chatgpt.com/**", route => route.fulfill({contentType:"text/html", body:`<!doctype html>
        <button aria-pressed="false">Chat</button><button aria-pressed="true">Work</button>
        <script>
          window.__STATSIG__ = {instances:{}};
          const client = {};
          window.__STATSIG__.firstInstance = client;
          window.__STATSIG__.instances.fixture = client;
          client.getFeatureGate = name => ({value:true});
          client.checkGate = name => client.getFeatureGate(name).value;
          const work = client.checkGate('1625797156');
          document.body.dataset.ready = String(!work);
          if(work) document.querySelector('button').hidden = true;
          document.querySelectorAll('button')[1].disabled = !work;
          document.querySelectorAll('button').forEach(b => b.onclick = () => {
            document.querySelectorAll('button').forEach(x => x.setAttribute('aria-pressed', String(x === b)));
          });
        </script>`}));
      const page = context.pages()[0]!;
      await page.goto("https://chatgpt.com/");
      expect(await page.locator("body").getAttribute("data-ready")).toBe("true");
      await selectChatMode(page);
      expect(await page.getByRole("button", {name:"Chat", exact:true}).getAttribute("aria-pressed")).toBe("true");
      await page.reload();
      expect(await page.locator("body").getAttribute("data-ready")).toBe("true");
      await selectChatMode(page);
      expect(await page.getByRole("button", {name:"Chat", exact:true}).getAttribute("aria-pressed")).toBe("true");
      expect(context.pages()).toHaveLength(1);
    } finally {
      await context.close();
      await rm(profile, {recursive:true, force:true});
    }
  }, 30_000,
);
