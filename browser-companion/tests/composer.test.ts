import { afterEach, expect, test } from "bun:test";
import { activeComposer, insertPlainTextIntoComposer, isPairingVerificationDraft, verifyOwnedComposerText, uniqueSendButton } from "../src/composer";
import { renderActionPrompt } from "../src/protocol";
import type {Page,Locator} from "patchright";
import {BrowserCompanionError} from "../src/errors";

test("short readiness probes expire as missing editors and can retry the same page",async()=>{
  let ready=false;
  const composer={isEditable:async()=>true};
  const candidates={
    count:()=>ready?Promise.resolve(1):Bun.sleep(100).then(()=>0),
    first:()=>composer,
  };
  const page={locator:()=>({filter:()=>candidates})} as unknown as Page;
  let failure:unknown;
  try {await activeComposer(page,15);}catch(error){failure=error;}
  expect(failure).toBeInstanceOf(Error);
  expect(failure).not.toBeInstanceOf(BrowserCompanionError);
  expect((failure as Error).message).toContain("composer is unavailable");
  ready=true;
  expect(await activeComposer(page,1000)).toBe(composer as never);
});

test("short readiness probes preserve cancellation and immediate observation failures",async()=>{
  const failed={locator:()=>({filter:()=>({count:()=>Promise.reject(new BrowserCompanionError("real failure","editor_unavailable"))})})} as unknown as Page;
  await expect(activeComposer(failed,1000)).rejects.toBeInstanceOf(BrowserCompanionError);
  const controller=new AbortController();
  const pending={locator:()=>({filter:()=>({count:()=>Bun.sleep(200).then(()=>0)})})} as unknown as Page;
  setTimeout(()=>controller.abort(),5);
  await expect(activeComposer(pending,1000,controller.signal)).rejects.toMatchObject({name:"AbortError"});
});

test("a short composer probe also bounds the editable-state read",async()=>{
  const page={locator:()=>({filter:()=>({count:async()=>1,first:()=>({isEditable:()=>new Promise(()=>{})})})})} as unknown as Page;
  const started=performance.now();
  await expect(activeComposer(page,20)).rejects.toThrow("composer is unavailable");
  expect(performance.now()-started).toBeLessThan(500);
});

test("Send readiness cancels hung form and enabled-state reads without returning a submitter",async()=>{
  for(const hungForm of [true,false]) {
    const controller=new AbortController();
    let enabledRead=false;
    const send={count:async()=>1,first:()=>({isEnabled:()=>{enabledRead=true;return new Promise(()=>{});}})};
    const form={count:()=>hungForm?new Promise(()=>{}):Promise.resolve(1),locator:()=>({filter:()=>send})};
    const composer={locator:()=>form} as unknown as Locator;
    const pending=uniqueSendButton(composer,1_000,controller.signal);
    await Bun.sleep(5);
    controller.abort(new Error("cancelled Send readiness"));
    await expect(pending).rejects.toThrow("cancelled Send readiness");
    expect(enabledRead).toBe(!hungForm);
  }
});

interface FakeSelection {
  isCollapsed: boolean;
  anchorNode: object | null;
  removeAllRanges(): void;
  addRange(range: object): void;
}

const originals = { document: globalThis.document, window: globalThis.window };
afterEach(() => {
  (globalThis as Record<string, unknown>).document = originals.document;
  (globalThis as Record<string, unknown>).window = originals.window;
});

function harness(options: {
  focusable: boolean;
  caretInsideComposer: boolean;
  collapsed?: boolean;
  execCommandResult?: boolean;
}) {
  const inside = { name:"inside" };
  const composer = {
    focus() { if (options.focusable) fakeDocument.activeElement = composer; },
    contains: (node: object | null) => node === inside || node === composer,
  };
  const calls: Array<{ command:string; value:string }> = [];
  const selection: FakeSelection = {
    isCollapsed: options.collapsed ?? true,
    anchorNode: options.caretInsideComposer ? inside : { name:"outside" },
    removeAllRanges() { selection.anchorNode = null; },
    addRange() { selection.anchorNode = inside; selection.isCollapsed = true; },
  };
  const fakeDocument = {
    activeElement: null as unknown,
    createRange: () => ({ selectNodeContents() {}, collapse() {} }),
    execCommand(command: string, _ui: boolean, value: string) {
      calls.push({ command, value });
      return options.execCommandResult ?? true;
    },
  };
  (globalThis as Record<string, unknown>).document = fakeDocument;
  (globalThis as Record<string, unknown>).window = { getSelection: () => selection };
  return { composer:composer as unknown as HTMLElement, calls, selection };
}

test("exact plaintext insertion establishes a caret only inside the owned composer", () => {
  const { composer, calls, selection } = harness({ focusable:true, caretInsideComposer:false });
  expect(insertPlainTextIntoComposer(composer, "line one\nline two")).toBeTrue();
  expect(calls).toEqual([{ command:"insertText", value:"line one\nline two" }]);
  expect(selection.isCollapsed).toBeTrue();
});

test("insertion refuses a detached or covered composer and reports rejected edits", () => {
  expect(insertPlainTextIntoComposer(harness({ focusable:false, caretInsideComposer:false }).composer, "x")).toBeFalse();
  expect(insertPlainTextIntoComposer(harness({ focusable:true, caretInsideComposer:true, execCommandResult:false }).composer, "x")).toBeFalse();
});

test("composer verification preserves exact whitespace and rejects connector-only or stale drafts", () => {
  expect(verifyOwnedComposerText("Prompt\nwith  two spaces", "Prompt\nwith  two spaces")).toBeTrue();
  expect(verifyOwnedComposerText("Prompt with two spaces", "Prompt with  two spaces")).toBeFalse();
  expect(verifyOwnedComposerText("Codexify", "")).toBeFalse();
  expect(verifyOwnedComposerText("old request", "new request")).toBeFalse();
});

test("pairing replacement recognizes only the complete unchanged verification template", () => {
  const draft = renderActionPrompt({kind:"pair_verify",reference:"A".repeat(43)});
  expect(isPairingVerificationDraft(draft)).toBeTrue();
  const legacy = [
    "Verify that this ChatGPT conversation is paired with the intended Codexify task by",
    `calling \`automation_verify_pairing\` with the non-secret: ${"A".repeat(43)}`,
    "Do not inspect project files or perform task work in this verification turn.",
    "",
    draft.split("\n\n").at(-1),
  ].join("\n");
  expect(isPairingVerificationDraft(legacy)).toBeTrue();
  for (const text of ["my own draft", draft + " edited", legacy + " edited", draft.replace("Do not inspect", "Please inspect"), draft.replace("A".repeat(43), "short"), draft.replaceAll("\n", " ")]) {
    expect(isPairingVerificationDraft(text)).toBeFalse();
  }
});
