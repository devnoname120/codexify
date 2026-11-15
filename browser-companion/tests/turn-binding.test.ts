import { expect, test } from "bun:test";
import type {Page} from "patchright";
import {
  newTurnIdentity,
  reboundTurnIdentity,
  submissionEvidence,
  turnIdentityFromAttributes,
  readTurnDomState,
  waitForSubmissionAcceptance,
  waitForAssistantTurn,
  captureSubmissionBaseline,
} from "../src/turn-binding";

test("a hung turn read cancels without waiting for Chrome or rewriting the submission baseline", async () => {
  let release!: (value:unknown)=>void, reads=0;
  const page={evaluate:()=>{reads++;return new Promise(resolve=>{release=resolve;});}} as unknown as Page;
  const controller=new AbortController();
  const pending=captureSubmissionBaseline(page,"Owned prompt",controller.signal);
  controller.abort(new Error("cancelled turn read"));
  await expect(pending).rejects.toThrow("cancelled turn read");
  release({turnIdentities:[],userIdentities:[],responseIdentities:[],generationRunning:false});
  await Promise.resolve();
  expect(reads).toBe(1);
  await expect(readTurnDomState(page,undefined,controller.signal)).rejects.toThrow("cancelled turn read");
  expect(reads).toBe(1);
});

test("acceptance and assistant binding deadlines include a hung browser read",async()=>{
  const page={evaluate:()=>new Promise(()=>{})} as unknown as Page;
  const baseline={initialTurnIdentities:[],submittedText:"Owned prompt"};
  for(const poll of [waitForSubmissionAcceptance,waitForAssistantTurn]) {
    const start=performance.now();
    await expect(poll(page,baseline,Date.now()+20)).rejects.toMatchObject({code:"editor_unavailable"});
    expect(performance.now()-start).toBeLessThan(500);
  }
});

test("turn identity uses stable turn keys and never display indexes", () => {
  expect(turnIdentityFromAttributes({ role:"user", turnKey:"group-a", turnId:null })).toBe("group:user:group-a");
  expect(turnIdentityFromAttributes({ role:"assistant", turnKey:"group-a", turnId:null })).toBe("group:assistant:group-a");
  expect(turnIdentityFromAttributes({ role:"assistant", turnKey:null, turnId:"turn-123" })).toBe("turn-123");
  expect(() => turnIdentityFromAttributes({ role:"assistant", turnKey:null, turnId:null })).toThrow("stable identity");
});

test("one physical Send may create only one new logical turn", () => {
  expect(newTurnIdentity(["old"], ["old", "new"])).toBe("new");
  expect(newTurnIdentity(["old"], ["old", "old", "new", "new"])).toBe("new");
  expect(newTurnIdentity(["old"], ["old"])).toBeUndefined();
  expect(() => newTurnIdentity(["old"], ["old", "new-a", "new-b"])).toThrow("2 new");
});

test("semantic submission evidence prefers the accepted user turn", () => {
  const baseline = ["group:user:old", "group:assistant:old"];
  expect(submissionEvidence({
    initialTurnIdentities:baseline,
    userIdentities:[...baseline, "group:user:new"],
    responseIdentities:["group:assistant:old"],
    generationRunning:true,
  })).toBe("user_turn");
  expect(submissionEvidence({
    initialTurnIdentities:baseline,
    userIdentities:["group:user:old"],
    responseIdentities:["group:assistant:old", "group:assistant:new"],
    generationRunning:true,
  })).toBe("assistant_turn");
  expect(submissionEvidence({
    initialTurnIdentities:baseline,
    userIdentities:["group:user:old"],
    responseIdentities:["group:assistant:old"],
    generationRunning:true,
  })).toBe("generation_running");
});

test("rebound assistant identity never chooses among competing turns", () => {
  const initial = ["group:assistant:old"];
  expect(reboundTurnIdentity(initial, "group:assistant:optimistic", ["group:assistant:optimistic"]))
    .toBe("group:assistant:optimistic");
  expect(reboundTurnIdentity(initial, "group:assistant:optimistic", ["group:assistant:old", "group:assistant:persisted"]))
    .toBe("group:assistant:persisted");
  expect(() => reboundTurnIdentity(initial, "group:assistant:optimistic", [
    "group:assistant:old", "group:assistant:persisted", "group:assistant:foreign",
  ])).toThrow("2 new");
});
