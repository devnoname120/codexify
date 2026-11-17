"use strict";
const test=require("node:test");
const assert=require("node:assert/strict");
const path=require("node:path");
const {archiveTool}=require("../scripts/archive-tool.cjs");

test("non-Windows packaging continues to use PATH tar",()=>{
  assert.equal(archiveTool({platform:"linux",exists:()=>{throw new Error("must not probe")}}),"tar");
});
test("Windows resolves the OS-owned tar.exe even if PATH omits System32",()=>{
  let checked;
  const expected=path.win32.join("C:\\Windows","System32","tar.exe");
  assert.equal(archiveTool({platform:"win32",systemRoot:"C:\\Windows",exists:p=>{checked=p;return true}}),expected);
  assert.equal(checked,expected);
});
test("Windows system root can contain spaces",()=>{
  const expected=path.win32.join("C:\\Windows Dir","System32","tar.exe");
  assert.equal(archiveTool({platform:"win32",systemRoot:"C:\\Windows Dir",exists:()=>true}),expected);
});
test("Windows rejects a missing native tar.exe instead of searching arbitrary PATH",()=>{
  assert.throws(()=>archiveTool({platform:"win32",systemRoot:"C:\\Windows",exists:()=>false}),/unavailable/);
});
test("Windows rejects a relative system directory",()=>{
  assert.throws(()=>archiveTool({platform:"win32",systemRoot:"relative",exists:()=>true}),/not absolute/);
});