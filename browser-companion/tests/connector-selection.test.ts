import { expect, test } from "bun:test";
import { createDocument } from "@mixmark-io/domino";
import { connectorSelectionSelector, SELECTED_CONNECTOR_SELECTOR, normalizeConnectorRowTitles, selectConnector } from "../src/connector-selection";
import { CHATGPT_COMPOSER_SELECTOR } from "../src/session";

test("selected connector selector matches only one exact configured identity", () => {
  expect(connectorSelectionSelector("Codexify")).toBe(
    '[data-id^="plugin:"][data-keyword="Codexify"], [app-mention-path^="app://"][app-mention-display-name="Codexify"][contenteditable="false"]',
  );
  expect(connectorSelectionSelector('Codexify "DEV"')).toContain('Codexify \\"DEV\\"');
  for (const value of ["", " ", "x\n", "x".repeat(257)]) {
    expect(() => connectorSelectionSelector(value)).toThrow("invalid");
  }
});

test("connector menu diagnostics retain titles but discard descriptions and blanks", () => {
  expect(normalizeConnectorRowTitles([
    "Codexify\nUse tools from Codexify",
    "  Codexify DEV  \nOther description",
    "\nDescription only",
    "",
  ])).toEqual(["Codexify", "Codexify DEV"]);
});

test("connector selection searches for the configured name and verifies its exact identity", async () => {
  for (const [name,draft] of ["Custom app", 'Custom app "DEV"'].flatMap(name=>["","An old draft"].map(draft=>[name,draft] as const))) {
    const document = createDocument('<div id="prompt-textarea" contenteditable="true"></div>');
    const editor = document.getElementById("prompt-textarea")!;
    editor.textContent=draft;
    const pill = document.createElement("span");
    pill.setAttribute("data-id", "plugin:test-connector");
    pill.setAttribute("data-keyword", name);
    pill.setAttribute("contenteditable", "false");
    pill.textContent = name;
    const names = ["Codexify", name];
    const queries: string[] = [];
    const pressed:string[]=[];
    const visibleNames = () => names.filter(candidate => candidate.toLowerCase()
      .includes((editor.textContent ?? "").slice(1).toLowerCase()));
    const selected = {
      filter() { return this; },
      count:async()=>editor.contains(pill)?1:0,
      evaluateAll: async (read: (elements: Element[], useIdentity?: boolean) => unknown, useIdentity?: boolean) => read(editor.contains(pill) ? [pill] : [], useIdentity),
      waitFor: async () => { expect(editor.contains(pill)).toBeTrue(); },
    };
    const composer = {
      filter() { return this; }, first() { return this; },
      count: async () => 1, isEditable: async () => true,
      evaluate: async (read: (element: Element) => unknown) => read(editor),
      locator: (selector: string) => {
        expect([connectorSelectionSelector(name), SELECTED_CONNECTOR_SELECTOR, '[data-id="plugin:test-connector"], [app-mention-path="plugin:test-connector"][contenteditable="false"]']).toContain(selector);
        return selected;
      },
      focus: async () => {},
      fill: async (value: string) => { if(value.startsWith('@'))queries.push(value); editor.textContent = value; },
      pressSequentially: async () => { throw new Error("App search must use a single input event"); },
      press: async (key: string) => {
        pressed.push(key);
        if (key === "Backspace") editor.textContent = "";
        if (key === "Enter") throw new Error("Selecting an app must not press Enter in the composer");
      },
    };
    const rows = {
      filter(options: { has?: string; visible?: boolean }) {
        if (options.has === undefined) return this;
        const target = options.has;
        return {
          waitFor: async () => {
            if (!visibleNames().includes(target)) throw new Error("The mention search excludes the configured connector");
          },
          count: async () => visibleNames().filter(candidate => candidate === target).length,
          click:async()=>{expect(target).toBe(name);editor.textContent="";editor.appendChild(pill);},
        };
      },
      count: async () => visibleNames().length,
      allInnerTexts: async () => visibleNames(),
    };
    const page = {
      locator: (selector: string) => selector === CHATGPT_COMPOSER_SELECTOR ? composer : rows,
      getByText: (text: string) => text,
      keyboard: { press: async () => {} },
    };
    expect(await selectConnector(page as never, name)).toEqual({composer:composer as never,identity:"plugin:test-connector"});
    expect(queries).toEqual([`@${name}`]);
    expect(pressed).not.toContain("Enter");
    expect(editor.querySelector('[data-keyword]')?.getAttribute("data-keyword")).toBe(name);
  }
});
