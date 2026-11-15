import {expect,test} from 'bun:test';
import {SingleWindowCoordinator,type BrowserDriver} from '../src/browser-window';
import {protocolBuild,type ExecuteActionRequest} from '../src/protocol';

const build=protocolBuild('cleanup-regression');
const budget={operationMs:20,cleanupMs:5,hardStopMs:5,retainedCleanupMs:30};
function driverFixture(){
  let closed=0,executed=0;
  const driver={health:async()=>({authenticated:true,accountFingerprint:'account',status:'idle',detail:null}),
    liveness:async()=>true,close:async()=>{closed++;},
    capabilities:async()=>{executed++;return {protocolVersion:1,build,accountFingerprint:'account',catalogRevision:'catalog',models:[]};},
    refreshConnector:async()=>new Promise(()=>{}),execute:async()=>{executed++;}} as unknown as BrowserDriver;
  return {driver,closed:()=>closed,executed:()=>executed};
}
test('a cancelled read-only presentation probe preserves the browser',async()=>{
  const fixture=driverFixture();
  let beginRead!:()=>void;
  const reading=new Promise<void>(resolve=>{beginRead=resolve;});
  fixture.driver.presentation=async()=>{
    beginRead();
    return new Promise(()=>{});
  };
  const coordinator=new SingleWindowCoordinator(fixture.driver,build,budget);
  const cancellation=new AbortController();
  const diagnostic=coordinator.presentation(cancellation.signal);
  await reading;
  cancellation.abort();
  await expect(diagnostic).rejects.toThrow('cancelled');
  await Bun.sleep(60);
  expect(fixture.closed()).toBe(0);
  expect((await coordinator.capabilities()).catalogRevision).toBe('catalog');
  await coordinator.close();
});
test('a forever-pending maintenance read cannot keep a responsive browser busy forever',async()=>{
  const f=driverFixture(),coordinator=new SingleWindowCoordinator(f.driver,build,budget);
  await expect(coordinator.refreshConnector('fixture')).rejects.toThrow('timed out');
  expect(f.closed()).toBe(0);
  await Bun.sleep(60);
  expect(f.closed()).toBe(1);expect((await coordinator.health()).status).toBe('unavailable');
  expect((await coordinator.liveness()).available).toBe(false);
  await expect(coordinator.capabilities()).rejects.toThrow('recovery is required');
  expect(f.executed()).toBe(0);await coordinator.close();
});
test('cleanup that settles within its grace period preserves the browser',async()=>{
  const f=driverFixture();let release!:()=>void;
  f.driver.refreshConnector=async()=>{await new Promise<void>(resolve=>release=resolve);return {appId:'fixture',refreshObserved:true,permissionChanged:false,schemaVerified:false};};
  const coordinator=new SingleWindowCoordinator(f.driver,build,{...budget,retainedCleanupMs:100});
  await expect(coordinator.refreshConnector('fixture')).rejects.toThrow('timed out');release();
  await Bun.sleep(120);expect(f.closed()).toBe(0);expect((await coordinator.health()).status).toBe('idle');await coordinator.close();
});

test('even a hung close cannot retain the old browser lease indefinitely',async()=>{
  const f=driverFixture();f.driver.close=async()=>new Promise(()=>{});
  const coordinator=new SingleWindowCoordinator(f.driver,build,budget);
  await expect(coordinator.refreshConnector('fixture')).rejects.toThrow('timed out');
  await Bun.sleep(80);
  expect((await coordinator.liveness()).available).toBe(false);
  await expect(coordinator.capabilities()).rejects.toThrow('recovery is required');
  expect(f.executed()).toBe(0);await coordinator.close();
});
test('an unsent load that ignores cancellation retires without replaying the action',async()=>{
  const f=driverFixture();f.driver.execute=async e=>{e.emit({type:'navigating'});await new Promise(()=>{});};
  const coordinator=new SingleWindowCoordinator(f.driver,build,budget);
  const request:ExecuteActionRequest={actionId:'held-load',idempotencyKey:'held-load-key',url:'https://chatgpt.com/c/held',expectedUrl:'https://chatgpt.com/c/held',
    expectedAccountFingerprint:'account',pairingGeneration:1,connectorName:'Codexify',model:null,effort:null,prompt:{kind:'offline_goal',goalRevision:1},autoApproveToolCalls:false};
  await coordinator.execute(request);for await(const _ of coordinator.events(request.actionId,0)){}
  expect((await coordinator.action(request.actionId))?.sendAccepted).toBe(false);
  await Bun.sleep(60);expect(f.closed()).toBe(1);expect((await coordinator.health()).status).toBe('unavailable');
  await expect(coordinator.execute({...request,actionId:'next',idempotencyKey:'next'})).rejects.toThrow('recovery is required');
  await coordinator.close();
});

test('approval on an existing turn reports a pre-Send failure without claiming message acceptance',async()=>{
  const f=driverFixture();f.driver.execute=async e=>{e.emit({type:'navigating'});e.emit({type:'approval_required',message:'Approve the existing connector turn'});};
  const coordinator=new SingleWindowCoordinator(f.driver,build,budget);
  const request:ExecuteActionRequest={actionId:'approval-before-send',idempotencyKey:'approval-before-send-key',url:'https://chatgpt.com/c/held',expectedUrl:'https://chatgpt.com/c/held',
    expectedAccountFingerprint:'account',pairingGeneration:1,connectorName:'Codexify',model:null,effort:null,prompt:{kind:'offline_goal',goalRevision:1},autoApproveToolCalls:false};
  await coordinator.execute(request);const events=[];for await(const event of coordinator.events(request.actionId,0))events.push(event);
  expect(events.at(-1)?.type).toBe('approval_required');
  expect(await coordinator.action(request.actionId)).toMatchObject({state:'failed',sendAccepted:false,attention:{code:'approval_required'}});
  await coordinator.close();
});

test('explicit approval recovery holds one lease across navigation and verified approval',async()=>{
  const f=driverFixture(),order:string[]=[];
  f.driver.reveal=async url=>{order.push('navigate:'+url);};
  f.driver.approvePendingConversation=async()=>{order.push('approve');return true;};
  const coordinator=new SingleWindowCoordinator(f.driver,build,{...budget,operationMs:200});
  const request={url:'https://chatgpt.com/c/existing',expectedAccountFingerprint:'account',connectorName:'Codexify'};
  expect((await coordinator.approvePendingConversation(request,undefined,true)).approved).toBe(true);
  expect(order).toEqual(['navigate:'+request.url,'approve']);expect(f.executed()).toBe(0);
  await expect(coordinator.approvePendingConversation({...request,expectedAccountFingerprint:'different'},undefined,true)).rejects.toThrow('account changed');
  expect(order).toHaveLength(2);await coordinator.close();
});
