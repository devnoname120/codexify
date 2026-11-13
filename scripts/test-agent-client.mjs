import assert from 'node:assert/strict';
import {copyFile, mkdtemp, rm, writeFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {tmpdir} from 'node:os';
import {basename, dirname, join, resolve} from 'node:path';
import {pathToFileURL} from 'node:url';
import test from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {monitorTeams} from './windows-autowork-team-status.mjs';

const fixtureToken = `Bearer codexify_agents_${'a'.repeat(64)}`;
async function fixture(t, handler) {
  const directory = await mkdtemp(join(tmpdir(), 'codexify-agent-client-'));
  await copyFile(new URL('../src/owner_chat/agent_client.mjs', import.meta.url), join(directory, 'client.mjs'));
  const server = createServer(async (request, response) => {
    try {
      assert.equal(request.method, 'POST');
      assert.equal(request.url, '/api/agent-client/request');
      assert.equal(request.headers.authorization, fixtureToken);
      let body = ''; for await (const chunk of request) body += chunk;
      await handler(JSON.parse(body), response);
    } catch (error) { response.writeHead(500).end(error.message); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const value = {version:1, enabled:true, requestUrl:`http://127.0.0.1:${server.address().port}/api/agent-client/request`, headers:{Authorization:fixtureToken}};
  const publish = next => writeFile(join(directory, 'connection.json'), JSON.stringify(next));
  await publish(value);
  t.after(async () => {
    server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
    assert.equal(dirname(resolve(directory)), resolve(tmpdir()));
    assert.ok(basename(directory).startsWith('codexify-agent-client-'));
    await rm(directory, {recursive:true, force:true});
  });
  const client = await import(pathToFileURL(join(directory, 'client.mjs')).href);
  return {client, publish, value, helperPath:join(directory, 'client.mjs')};
}

test('one JS helper sends all action arguments and follows published connection changes', async t => {
  const seen = [];
  const first = await fixture(t, async (request, response) => {
    seen.push(request); response.setHeader('Content-Type','application/json'); response.end(JSON.stringify({teamId:'team', source:1}));
  });
  const setup = {action:'setup', requestId:'team-01', clientName:'Any AI', projectPath:'repo', model:'model', effort:'high', workers:[]};
  assert.equal((await first.client.agent(setup)).source, 1);
  assert.deepEqual(seen, [setup]);
  const second = await fixture(t, async (request, response) => response.end(JSON.stringify({source:2, action:request.action})));
  await first.publish(second.value);
  assert.deepEqual(await first.client.agent({action:'list'}), {source:2, action:'list'});
  await first.publish({version:1, enabled:false});
  await assert.rejects(first.client.agent({action:'list'}), /disabled/);
});

test('JS requests preserve errors and obey caller cancellation and deadlines', async t => {
  const {client} = await fixture(t, async (request, response) => {
    if (request.action === 'status') response.writeHead(422).end('Unknown teamId');
    // Other requests intentionally remain pending until client cancellation.
  });
  await assert.rejects(client.agent({action:'status'}), /422.*Unknown teamId/);
  await assert.rejects(client.agent({action:'capabilities'}, {timeoutMs:30}), error => error.name === 'TimeoutError');
  const cancellation = new AbortController();
  const request = client.agent({action:'capabilities'}, {signal:cancellation.signal});
  cancellation.abort(); await assert.rejects(request, error => error.name === 'AbortError');
});

test('JS credentials stay on the exact loopback endpoint and never follow redirects', async t => {
  const {client, publish, value} = await fixture(t, async (_, response) => response.writeHead(307, {Location:'http://127.0.0.1:9/stolen'}).end());
  await assert.rejects(client.agent({action:'list'}), /fetch failed/);
  for (const requestUrl of ['https://example.invalid/api/agent-client/request', 'http://127.0.0.1:3120/api/chats', 'http://user@127.0.0.1:3120/api/agent-client/request']) {
    await publish({...value, requestUrl}); await assert.rejects(client.agent({action:'list'}), /Invalid Codexify agent connection/);
  }
});

test('read retry follows a changed connection with byte-identical arguments', async t => {
  const seen = [];
  const request = {action:'status', teamId:'same-team'};
  const expected = {...request};
  const second = await fixture(t, async (body, response) => {
    seen.push(body); response.end(JSON.stringify({source:2}));
  });
  const first = await fixture(t, async (body, response) => {
    seen.push(body);
    request.action = 'control';
    await first.publish(second.value);
    response.destroy();
  });
  assert.deepEqual(await first.client.agent(request), {source:2});
  assert.deepEqual(seen, [expected, expected]);
});

test('read transport failures stop after one retry', async t => {
  let attempts = 0;
  const {client} = await fixture(t, async (_, response) => { attempts++; response.destroy(); });
  await assert.rejects(client.agent({action:'list'}, {timeoutMs:2000}), /fetch failed/);
  assert.equal(attempts, 2);
});

test('lost mutation responses are never replayed automatically', async t => {
  const attempts = new Map();
  const {client} = await fixture(t, async (body, response) => {
    attempts.set(body.action, (attempts.get(body.action) ?? 0) + 1);
    response.destroy();
  });
  for (const action of ['setup', 'spawn', 'control']) {
    await assert.rejects(client.agent({action, requestId:'original-id'}, {timeoutMs:2000}), /fetch failed/);
    assert.equal(attempts.get(action), 1);
  }
});

test('read retries share the original caller deadline', async t => {
  let attempts = 0;
  const {client} = await fixture(t, async (_, response) => { attempts++; response.destroy(); });
  await assert.rejects(client.agent({action:'list'}, {timeoutMs:50}), error => error.name === 'TimeoutError');
  await delay(250);
  assert.ok(attempts <= 1, 'a second attempt must not outlive the original deadline');
});

test('caller cancellation interrupts the retry delay', async t => {
  let attempts = 0, entered;
  const ready = new Promise(resolve => { entered = resolve; });
  const {client} = await fixture(t, async (_, response) => { attempts++; entered(); response.destroy(); });
  const cancellation = new AbortController();
  const pending = client.agent({action:'capabilities'}, {signal:cancellation.signal});
  await ready; await delay(30); cancellation.abort();
  await assert.rejects(pending, error => error.name === 'AbortError');
  await delay(250);
  assert.equal(attempts, 1);
});

test('HTTP read errors are surfaced without a transport retry', async t => {
  let attempts = 0;
  const {client} = await fixture(t, async (_, response) => { attempts++; response.writeHead(503).end('Try later'); });
  await assert.rejects(client.agent({action:'list'}), /503.*Try later/);
  assert.equal(attempts, 1);
});

test('team monitoring paginates and reports scopes, stops and blocked goals using only reads', async t => {
  const seen = [];
  const {helperPath} = await fixture(t, async (request, response) => {
    seen.push(request);
    if (request.action === 'list') response.end(JSON.stringify(request.afterTeamId
      ? {teams:[{teamId:'second',ready:false}],hasMore:false}
      : {teams:[{teamId:'first',ready:true}],hasMore:true,nextAfterTeamId:'first'}));
    else {
      assert.equal(request.action, 'status');
      response.end(JSON.stringify({team:{capacity:{active:1,limit:null},allowDelegation:true,workers:[{
        taskKey:request.teamId,role:'owned',scope:'private scope',goal:{status:'blocked',revision:3,blockedReason:'Root proof needed'},
        stopped:true,retired:false,action:'idle',toolCallCount:5,lastCallAtMs:123,queuePosition:null,
      }]}}));
    }
  });
  const result = await monitorTeams({helperPath});
  assert.equal(result.discoveryComplete, true);
  assert.equal(result.allStatusesAvailable, true);
  assert.deepEqual(seen.slice(0,2), [{action:'list'},{action:'list',afterTeamId:'first'}]);
  assert.equal(result.teams.length, 2);
  assert.equal(result.teams[1].ready, false, 'unfinished setups must also be observed');
  assert.equal(result.teams[0].capacity.limit, null);
  assert.equal(result.teams[0].workers[0].stopped, true);
  assert.equal(result.teams[0].workers[0].scope, 'private scope');
  assert.equal(result.teams[0].workers[0].blockedReason, 'Root proof needed');
  assert.deepEqual(new Set(seen.map(r=>r.action)), new Set(['list','status']));
});

test('a failed team status keeps successful team observations and explicit unknown workers', async t => {
  const {helperPath} = await fixture(t, async (request, response) => {
    if(request.action === 'list') response.end(JSON.stringify({teams:[{teamId:'good'},{teamId:'bad'}],hasMore:false}));
    else if(request.teamId === 'bad') response.writeHead(503).end('Unavailable');
    else response.end(JSON.stringify({team:{workers:[]}}));
  });
  const result=await monitorTeams({helperPath});
  assert.equal(result.available,true);
  assert.equal(result.discoveryComplete,true);
  assert.equal(result.allStatusesAvailable,false);
  assert.deepEqual(result.teams[0].workers,[]);
  assert.equal(result.teams[1].workers,null);
  assert.match(result.teams[1].error,/503.*Unavailable/);
});

test('all team reads share a single monitoring deadline', async t => {
  const seen=[];
  const {helperPath}=await fixture(t,async(request,response)=>{
    seen.push(request.action);
    if(request.action==='list') response.end(JSON.stringify({teams:[{teamId:'slow'}],hasMore:false}));
  });
  const result=await monitorTeams({helperPath,timeoutMs:100});
  assert.equal(result.discoveryComplete,true);
  assert.equal(result.allStatusesAvailable,false);
  assert.equal(result.teams[0].workers,null);
  assert.ok(result.teams[0].error);
  assert.deepEqual(seen,['list','status']);
});

test('repeating pagination fails visibly without a loop or losing discovered teams', async t => {
  let lists=0;
  const {helperPath}=await fixture(t,async(request,response)=>{
    if(request.action==='list') response.end(JSON.stringify({teams:[{teamId:`team-${++lists}`}],hasMore:true,nextAfterTeamId:'same'}));
    else response.end(JSON.stringify({team:{workers:[]}}));
  });
  const result=await monitorTeams({helperPath});
  assert.equal(lists,2);
  assert.equal(result.discoveryComplete,false);
  assert.equal(result.allStatusesAvailable,false);
  assert.equal(result.teams.length,2);
  assert.match(result.error,/pagination did not advance/);
});

test('missing or malformed team discovery stays unknown instead of an empty successful inventory', async t => {
  const missing=await monitorTeams({helperPath:join(tmpdir(),'codexify-no-such-helper','client.mjs')});
  assert.equal(missing.available,false);
  assert.equal(missing.teams,null);
  const {helperPath}=await fixture(t,async(_,response)=>response.end('{}'));
  const malformed=await monitorTeams({helperPath});
  assert.equal(malformed.discoveryComplete,false);
  assert.equal(malformed.allStatusesAvailable,false);
  assert.equal(malformed.teams,null);
  const empty=await fixture(t,async(_,response)=>response.end('{"teams":[],"hasMore":false}'));
  const knownEmpty=await monitorTeams({helperPath:empty.helperPath});
  assert.equal(knownEmpty.available,true);
  assert.equal(knownEmpty.allStatusesAvailable,true);
  assert.deepEqual(knownEmpty.teams,[]);
});
