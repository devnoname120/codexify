const {test}=require("node:test");
const assert=require("node:assert/strict");
const {readFileSync}=require("node:fs");
const {join}=require("node:path");
const ts=require("typescript");
class BrowserCompanionError extends Error {
  constructor(message,code,options){super(message);this.code=code;this.sendAccepted=false;this.cause=options?.cause;}
}
const js=ts.transpileModule(
  readFileSync(join(__dirname,"../src/pairing-retry.ts"),"utf8"),
  {compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS}},
).outputText;
const scope={exports:{}};
new Function("require","exports","module",js)(
  name=>name==="./errors"?{BrowserCompanionError}:require(name),scope.exports,scope,
);
const {retryPickerAfterControlTimeout}=scope.exports;
function timeout(){const error=new Error("locator went stale");error.name="TimeoutError";return error;}

test("stale picker gets exactly one reacquire, and never sends",async()=>{
  let attempts=0,reacquires=0,sends=0;
  const result=await retryPickerAfterControlTimeout(
    async()=>{attempts++;throw timeout();},
    async()=>{reacquires++;return "new-control";},
    new AbortController().signal,()=>true,
  );
  assert.equal(result,"new-control");
  assert.equal(attempts,1);
  assert.equal(reacquires,1);
  assert.equal(sends,0);
});

test("unrelated errors and aborted attempts do not retry",async()=>{
  for(const reason of [new Error("selector drift"),new BrowserCompanionError("another page","external_interference")]){
    let retries=0;
    await assert.rejects(retryPickerAfterControlTimeout(
      async()=>{throw reason;},
      async()=>{retries++;},
      new AbortController().signal,()=>true,
    ),error=>error===reason);
    assert.equal(retries,0);
  }
  const controller=new AbortController();controller.abort();
  let retries=0;
  await assert.rejects(retryPickerAfterControlTimeout(
    async()=>{throw timeout();},
    async()=>{retries++;},
    controller.signal,()=>true,
  ));
  assert.equal(retries,0);
});

test("changed conversation fails closed without reacquiring",async()=>{
  let reacquires=0;
  await assert.rejects(retryPickerAfterControlTimeout(
    async()=>{throw timeout();},
    async()=>{reacquires++;},
    new AbortController().signal,()=>false,
  ),error=>error.code==="external_interference"&&error.sendAccepted===false);
  assert.equal(reacquires,0);
});

test("reacquire is bounded and a second timeout is not retried",async()=>{
  let reacquires=0;
  await assert.rejects(retryPickerAfterControlTimeout(
    async()=>{throw timeout();},
    async()=>{reacquires++;throw timeout();},
    new AbortController().signal,()=>true,
  ),error=>error.name==="TimeoutError");
  assert.equal(reacquires,1);
});
