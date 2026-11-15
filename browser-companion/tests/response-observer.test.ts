import { expect, test } from "bun:test";
import { ChatGptVisibleTraceTracker, isChatGptTraceControl, stripChatGptTraceControlSuffix } from "../src/response-observer";
import {responseDomSnapshot} from "../src/response-observer";
import type {Locator} from "patchright";

test("a hung response projection propagates cancellation instead of proving an absent final",async()=>{
  let reads=0;
  const locator={evaluate:()=>{reads++;return new Promise(()=>{});}} as unknown as Locator;
  const controller=new AbortController(),pending=responseDomSnapshot(locator,{},controller.signal);
  controller.abort(new Error("cancelled final inspection"));
  await expect(pending).rejects.toThrow("cancelled final inspection");
  await expect(responseDomSnapshot(locator,{},controller.signal)).rejects.toThrow("cancelled final inspection");
  expect(reads).toBe(1);
});

test("stable visible status and commentary stream without hidden reasoning", () => {
  const tracker = new ChatGptVisibleTraceTracker(100);
  const blocks = [
    { kind:"status" as const, text:"Searching 3 websites", key:"search" },
    { kind:"commentary" as const, text:"I found a stable public result.", key:"commentary", complete:true },
  ];
  expect(tracker.observe(blocks, false, 1_000)).toEqual([]);
  expect(tracker.observe(blocks, false, 1_100)).toEqual([
    { kind:"reasoning", text:"Searching 3 websites" },
    { kind:"commentary", text:"I found a stable public result." },
  ]);
  expect(tracker.observe(blocks, false, 1_200)).toEqual([]);
  expect(tracker.observe([
    { ...blocks[0], text:"Searching 4 websites" },
    { ...blocks[1], text:"I found a stable public result. More context." },
  ], true, 1_201)).toEqual([
    { kind:"reasoning", text:"Searching 4 websites" },
    { kind:"commentary", text:" More context.", continuation:true },
  ]);
});

test("ChatGPT response controls are removed from status output", () => {
  expect(isChatGptTraceControl({ kind:"status", text:"Answer now" })).toBeTrue();
  expect(isChatGptTraceControl({ kind:"status", text:"Searching", uiControl:true })).toBeTrue();
  expect(isChatGptTraceControl({ kind:"commentary", text:"Answer now" })).toBeFalse();
  expect(stripChatGptTraceControlSuffix({ kind:"status", text:"Searching websites Answer now" }).text)
    .toBe("Searching websites");
});
