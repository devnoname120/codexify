import {expect,test} from 'bun:test';
import {mkdtemp,mkdir,writeFile} from 'node:fs/promises';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import type {Page} from 'patchright';
import {readAutoApprovalPolicy,startAutoApprovalMonitor} from '../src/approval-monitor';
import {SingleWindowCoordinator,PASSIVE_HEALTH_BUDGET_MS} from '../src/browser-window';
import {protocolBuild} from '../src/protocol';

const policy={connectorName:'Codexify',accountFingerprint:'a'.repeat(64)};
function makeSurface(cards=1){return {page:async()=>({url:()=> 'https://chatgpt.com/c/fixture',isClosed:()=>false,
  locator:()=>({filter:()=>({count:async()=>cards})})}) as unknown as Page};}
const surface=makeSurface();
test('approval policy respects disabled settings, account proof and current changes',async()=>{
  const root=await mkdtemp(join(tmpdir(),'approval-policy-')),browser=join(root,'browser');await mkdir(browser);
  await writeFile(join(browser,'account.json'),JSON.stringify({fingerprint:policy.accountFingerprint}));
  const config={agentChat:{enabled:true,automation:{enabled:true,autoApproveToolCalls:true,connectorName:'Codexify'}}};
  await writeFile(join(root,'codexify.config.json'),JSON.stringify(config));expect(readAutoApprovalPolicy(browser)).toEqual(policy);
  config.agentChat.automation.autoApproveToolCalls=false;await writeFile(join(root,'codexify.config.json'),JSON.stringify(config));expect(readAutoApprovalPolicy(browser)).toBeNull();
});
test('visible approval monitoring uses once scope and never overlaps another poll',async()=>{
  let calls=0,release!:()=>void,entered!:()=>void;
  const started=new Promise<void>(resolve=>entered=resolve),held=new Promise<void>(resolve=>release=resolve);
  const coordinator={approvePendingConversation:async(_request:unknown,_signal?:AbortSignal,navigate?:boolean,once?:boolean)=>{
    calls++;expect(navigate).toBe(false);expect(once).toBe(true);entered();await held;return {protocolVersion:1,build:'test',approved:false};}};
  const stop=startAutoApprovalMonitor(coordinator,surface,'unused',{policy:()=>policy,intervalMs:5});
  await started;await Bun.sleep(25);expect(calls).toBe(1);stop();release();await Bun.sleep(15);expect(calls).toBe(1);
});
test('disabled policy and non-conversation pages never approve',async()=>{
  let calls=0;const coordinator={approvePendingConversation:async()=>{calls++;return {protocolVersion:1,build:'test',approved:false};}};
  const stop=startAutoApprovalMonitor(coordinator,surface,'unused',{policy:()=>null,intervalMs:5});await Bun.sleep(15);stop();expect(calls).toBe(0);
  const home={page:async()=>({url:()=> 'https://chatgpt.com/',isClosed:()=>false}) as Page};
  const stopHome=startAutoApprovalMonitor(coordinator,home,'unused',{policy:()=>policy,intervalMs:5});await Bun.sleep(15);stopHome();expect(calls).toBe(0);
});

test('empty polling preserves shared health proof for queued native work',async()=>{
  let reads=0,approvals=0;
  const coordinator=new SingleWindowCoordinator({
    health:async()=>{reads++;await Bun.sleep(PASSIVE_HEALTH_BUDGET_MS+30);return {authenticated:true,accountFingerprint:policy.accountFingerprint,status:'idle',detail:null};},
    execute:async()=>{},reveal:async()=>{},
    login:async()=>{throw Error('Unexpected login');},capabilities:async()=>{throw Error('Unexpected capabilities');},
    approvePendingConversation:async()=>{approvals++;return false;},
  },protocolBuild('approval-health-test'));
  const stop=startAutoApprovalMonitor(coordinator,makeSurface(0),'unused',{policy:()=>policy,intervalMs:5});
  try {
    expect((await coordinator.health()).status).toBe('busy');await Bun.sleep(90);
    // Both UI and native preflight must still see the same completed proof,
    // despite dozens of empty approval checks in between.
    expect((await coordinator.health()).status).toBe('idle');await Bun.sleep(30);
    expect((await coordinator.health()).status).toBe('idle');
    expect(reads).toBe(1);expect(approvals).toBe(0);
  }finally{stop();await coordinator.close();}
});

test('active controller work is skipped without reading its page',async()=>{
  let pages=0,approvals=0;
  const coordinator={busy:true,approvePendingConversation:async()=>{approvals++;return {protocolVersion:1,build:'test',approved:false};}};
  const stop=startAutoApprovalMonitor(coordinator,{page:async()=>{pages++;return surface.page();}},'unused',{policy:()=>policy,intervalMs:5});
  await Bun.sleep(20);stop();expect(pages).toBe(0);expect(approvals).toBe(0);
});
