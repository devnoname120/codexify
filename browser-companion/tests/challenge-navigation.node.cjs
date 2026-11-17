// Offline regression coverage for the actual TypeScript navigation module
// on Windows machines without Bun. No Chrome, HTTP server, or account needed.
const {test} = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {join} = require("node:path");
const ts = require("typescript");

class BrowserCompanionError extends Error {
  constructor(message,code="failed") {
    super(message);
    this.code=code;
    this.sendAccepted=false;
  }
}
class ConversationCooldownError extends BrowserCompanionError {
  constructor(availableAtMs) {
    super("Conversation load cooldown","conversation_cooldown");
    this.availableAtMs=availableAtMs;
  }
}
function loadNavigation(){
  const source=readFileSync(join(__dirname,"../src/navigation.ts"),"utf8");
  const output=ts.transpileModule(source,{compilerOptions:{
    target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS,esModuleInterop:true,
  }}).outputText;
  const mocks={
    "./errors":{BrowserCompanionError,ConversationCooldownError},
    "./protocol":{canonicalChatGptConversationUrl:url=>{
      const parsed=new URL(url);
      return parsed.origin+parsed.pathname;
    }},
  };
  const result={};
  new Function("require","exports","module",output)(
    name=>mocks[name]??require(name),result,{exports:result},
  );
  return result;
}
class FakePage {
  constructor(owner={},code=200,challenge=null) {
    this.owner=owner;
    this.code=code;
    this.challenge=challenge;
    this.current="https://chatgpt.com/";
    this.loads=[];
  }
  context(){return this.owner;}
  url(){return this.current;}
  async goto(url){
    this.current=url;
    this.loads.push(url);
    return {
      status:()=>this.code,
      headerValue:async header=>header==="cf-mitigated"?this.challenge:null,
    };
  }
  async reload(){return this.goto(this.current);}
}

test("ChatGPT Cloudflare challenges fail closed before repeating a conversation load",async()=>{
  const nav=loadNavigation(),owner={},page=new FakePage(owner,200,"challenge");
  await assert.rejects(nav.navigatePage(page,"https://chatgpt.com/c/a"),error=>
    error.code==="external_interference"&&error.sendAccepted===false
      &&error.message.includes("Cloudflare verification challenge"));
  assert.deepEqual(page.loads,["https://chatgpt.com/c/a"]);
  page.challenge=null;
  await assert.rejects(nav.navigatePage(page,"https://chatgpt.com/c/b"),error=>
    error.code==="conversation_cooldown");
  assert.equal(page.loads.length,1);
  const another=new FakePage({},200);
  await nav.navigatePage(another,"https://chatgpt.com/c/other-profile");
  assert.equal(another.loads.length,1);
});

test("403 alone does not mean Cloudflare; unrelated origins do not acquire ChatGPT holds",async()=>{
  const nav=loadNavigation();
  const ordinary=new FakePage({},403);
  await nav.navigatePage(ordinary,"https://chatgpt.com/c/ordinary");
  const elsewhere=new FakePage({},403,"challenge");
  await nav.navigatePage(elsewhere,"https://example.com/protected");
  assert.equal(elsewhere.loads.length,1);
  assert.equal(nav.isCloudflareChallengeHeader(" challenge "),true);
  assert.equal(nav.isCloudflareChallengeHeader("bad-value"),false);
});
