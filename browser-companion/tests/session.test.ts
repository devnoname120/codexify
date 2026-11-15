import { expect, test } from "bun:test";
import { createDocument } from "@mixmark-io/domino";
import {
  CHATGPT_COMPOSER_SELECTOR,
  CHATGPT_EFFORT_CONTROL_SELECTOR,
  activateChatGptEffortMenu,
  parseChatGptEffortSliderState,
  parseChatGptModelAnnouncement,
  chatGptEffortLabelFromAnnouncement,
} from "../src/session";

test("composer and effort selectors exclude unrelated editors and menus", () => {
  const document = createDocument(`<body><form>
    <div contenteditable="true" id="unrelated-editor"></div>
    <button aria-haspopup="menu" id="attachments"></button>
    <div data-testid="prompt-textarea" id="composer-testid"></div>
    <div id="prompt-textarea"></div>
    <div contenteditable="true" data-lexical-editor="true" id="composer-lexical"></div>
    <button aria-haspopup="menu" data-tone="neutral" id="effort"></button>
    <button aria-haspopup="menu" data-testid="model-switcher-dropdown-button" id="model"></button>
  </form>
  <form data-chatgpt-composer>
    <div contenteditable="true" data-composer-markdown role="textbox" id="power-editor"></div>
    <button data-codex-intelligence-trigger="true" data-composer-navigation-target="reasoning" aria-haspopup="menu" id="power-effort"></button>
    <button data-composer-navigation-target="reasoning" aria-haspopup="menu" id="current-effort"></button>
    <button data-composer-navigation-target="add-context" aria-haspopup="menu" id="current-apps"></button>
  </form></body>`);
  const matches = (selector: string) => Array.from(document.querySelectorAll(selector)).map(element => element.id);
  expect(matches(CHATGPT_COMPOSER_SELECTOR)).toEqual(["composer-testid", "prompt-textarea", "composer-lexical", "power-editor"]);
  expect(matches(CHATGPT_EFFORT_CONTROL_SELECTOR)).toEqual(["effort", "model", "power-effort", "current-effort"]);
});

test("model announcements tolerate localized punctuation without weakening identity", () => {
  expect(parseChatGptModelAnnouncement("\u2068６ Pro\u2069、5 of 5")).toEqual({ version:"6", mode:"Pro" });
  expect(parseChatGptModelAnnouncement("GPT-5.6 Sol Extra High, 4 of 5")).toEqual({ version:"5.6", name:"sol", mode:"Extra High" });
  expect(parseChatGptModelAnnouncement("Try Pro for more reasoning")).toBeUndefined();
});

test("effort slider state is bounded and internally consistent", () => {
  expect(parseChatGptEffortSliderState("0", "4", "2")).toEqual({ min:0, max:4, value:2 });
  expect(parseChatGptEffortSliderState("0", "5", "2")).toEqual({ min:0, max:5, value:2 });
  expect(parseChatGptEffortSliderState("0", "16", "2")).toBeUndefined();
  expect(parseChatGptEffortSliderState("0", "4", "5")).toBeUndefined();
  expect(parseChatGptEffortSliderState("x", "4", "2")).toBeUndefined();
});

test("effort activation retries a ghost click with primary pointerdown", async () => {
  let ghostOpen = false;
  let pointerOpened = false;
  const events: unknown[] = [];
  const ownedSlider = {
    filter() { return this; }, locator() { return this; }, isVisible:async () => pointerOpened,
  };
  const ownedMenu = { isVisible: async () => pointerOpened, locator:() => ownedSlider };
  const hiddenSurface = {
    filter() { return this; }, last() { return this; }, locator() { return this; }, isVisible: async () => false,
  };
  const control = {
    evaluate:async () => undefined,
    getAttribute: async (name: string) => {
      if (name === "aria-controls") return pointerOpened ? "radix-effort-menu" : null;
      if (name === "aria-expanded") return ghostOpen ? "true" : "false";
      if (name === "data-state") return ghostOpen ? "open" : "closed";
      return null;
    },
    click: async (options: unknown) => { events.push(["click", options]); ghostOpen = true; },
    press: async (key:string) => {events.push(["keyboard",key]);ghostOpen=false;},
    dispatchEvent: async (name: string, detail: unknown) => {
      events.push([name, detail]); ghostOpen = true; pointerOpened = true;
    },
  };
  const page = {
    locator: (selector: string) => selector === '[id="radix-effort-menu"]' ? ownedMenu : hiddenSurface,
    keyboard: { press: async (key: string) => { events.push(["keyboard", key]); ghostOpen = false; } },
  };
  const activation = await activateChatGptEffortMenu(page as never, control as never, { settleMs:0 });
  expect(activation.method).toBe("pointerdown");
  expect(events).toEqual([
    ["click", { force:true, timeout:1 }],
    ["keyboard", "Escape"],
    ["pointerdown", { button:0, buttons:1, pointerType:"mouse", isPrimary:true }],
  ]);
});

test("an outgoing global slider cannot stand in for the control's new menu", async () => {
  let currentMenuVisible = false;
  let clicks = 0;
  const slider = { filter() { return this; }, locator() { return this; }, isVisible:async () => true };
  const menu = { isVisible:async () => currentMenuVisible, locator:() => slider };
  const control = {
    evaluate:async () => undefined,
    getAttribute:async (name: string) => name === "aria-controls" ? "current-menu" : "true",
    click:async () => { clicks += 1; currentMenuVisible = true; },
    press:async () => {},
  };
  const page = {
    locator:(selector: string) => selector === '[id="current-menu"]' ? menu : slider,
    keyboard:{ press:async () => {} },
  };
  const result = await activateChatGptEffortMenu(page as never, control as never, { settleMs:0 });
  expect(result.method).toBe("click");
  expect(clicks).toBe(1);
});


test("native effort labels are read without a fixed vocabulary and must match the slider position", () => {
  expect(chatGptEffortLabelFromAnnouncement("Balanced, 2 of 3.", {min:0,max:2,value:1})).toBe("Balanced");
  expect(chatGptEffortLabelFromAnnouncement("GPT-5.5 Sol Deep, 3 of 3.", {min:0,max:2,value:2})).toBe("Deep");
  expect(chatGptEffortLabelFromAnnouncement("Pro, 5 of 5.", {min:0,max:4,value:0})).toBeUndefined();
  expect(chatGptEffortLabelFromAnnouncement("Deep, 3 of 4.", {min:0,max:2,value:2})).toBeUndefined();
  expect(chatGptEffortLabelFromAnnouncement("Use Left and Right arrow keys to adjust power", {min:0,max:2,value:2})).toBeUndefined();
});
