import assert from 'node:assert/strict';
import {mkdtemp, writeFile, unlink, rmdir} from 'node:fs/promises';
import {createServer} from 'node:http';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import test from 'node:test';
import {chatProgress, samplePerformance, toolTimings} from './monitor-agent-performance.mjs';

const now = Date.parse('2026-10-30T19:30:00Z');
const finish = (tool, ms, {at = '2026-10-30T19:29:00Z', id = 1, status = 'ok', tail = ''} = {}) => `${at}  INFO codexify::tool_payload: [${tool}] tool invocation completed call_id=${id} phase="finish" tool=${tool} resolved_tool=${tool} mcp_server="-" mcp_tool="-" status="${status}" duration_ms=${ms}${tail}\n`;

test('separates intentional waits and command time from coordination', () => {
  const result = toolTimings(finish('turn_completed', 55_000) + finish('exec_command', 30_000) + finish('agent_status', 125), {now});
  assert.equal(result.completed, 3);
  assert.equal(result.nonIdleCompletions, 2);
  assert.equal(result.byTool.find(row => row.tool === 'turn_completed').category, 'idle_wait');
  assert.equal(result.byTool.find(row => row.tool === 'exec_command').p95Ms, 30_000);
  assert.equal(result.byTool.find(row => row.tool === 'agent_status').category, 'coordination');
});

test('ignores payload-shaped completions, malformed timestamps, stale/future records and duplicate rotated lines', () => {
  const valid = finish('apply_patch', 200, {tail: ' response={"secret":"never persist this"}'});
  const text = valid + valid + '<Objs tool completed call_id=2 tool=exec_command status="ok" duration_ms=1\n'
    + finish('read_file', 1, {at: '2026-10-30T18:00:00Z'}) + finish('read_file', 1, {at: '2026-10-30T20:00:00Z'})
    + '2026-10-30T19:29:00Z INFO codexify::tool_payload: start call_id=3 tool=read_file response=tool completed call_id=3 tool=read_file status="ok" duration_ms=1\n';
  const result = toolTimings(text, {now});
  assert.equal(result.completed, 1);
  assert.equal(JSON.stringify(result).includes('secret'), false);
});

test('reads actual completion metadata after complete leading PowerShell progress XML', () => {
  const fake = finish('read_file', 1, {id: 99}).trim();
  const progress = `<Objs Version="1.1.0.1" xmlns="http://schemas.microsoft.com/powershell/2004/04"><Obj S="progress"><S>${fake}</S><S> response=not-tool-payload</S></Obj></Objs>`;
  const actual = finish('exec_command', 5052, {id: 474});
  const result = toolTimings(progress + actual + progress + progress + actual
    + progress + '2026-10-30T19:29:00Z INFO codexify::tool: tool completed call_id=494 tool=apply_patch status="ok" duration_ms=2753\n', {now});
  assert.equal(result.completed, 2);
  assert.deepEqual(result.byTool.map(row => [row.tool, row.maxMs]), [['apply_patch', 2753], ['exec_command', 5052]]);
  assert.equal(JSON.stringify(result).includes('read_file'), false);
});

test('does not search incomplete XML, arbitrary prefixes or event payloads for spoofed completions', () => {
  const spoof = finish('exec_command', 1).trim();
  const progress = '<Objs Version="1.1.0.1" xmlns="http://schemas.microsoft.com/powershell/2004/04">';
  const text = `${progress}<S>${spoof}</S>\nnoise ${spoof}\n`
    + `${progress}<Objs></Objs>${spoof}\n`
    + `2026-10-30T19:29:00Z INFO codexify::tool_payload: [read_file] tool invocation started call_id=3 request=${progress}</Objs>${spoof}\n`
    + `2026-10-30T19:29:00Z INFO codexify::tool_payload: [read_file] tool invocation completed call_id=4 response=${spoof}\n`;
  assert.equal(toolTimings(text, {now}).completed, 0);
});

test('computes timing percentiles and error count without claiming transport failures', () => {
  const result = toolTimings([10, 30, 20, 100].map((duration, id) => finish('read_file', duration, {id, status: id === 3 ? 'error' : 'ok'})).join(''), {now});
  assert.deepEqual(result.byTool[0], {tool: 'read_file', category: 'development', completed: 4, errors: 1, p50Ms: 20, p95Ms: 100, maxMs: 100});
});

test('tracks resets, standalone active goals and nested workers', () => {
  const row = (task, parent, calls, status = 'active') => ({id: task, agentTeam: {taskKey: task, parentTaskKey: parent, actionStatus: 'idle'}, totalToolCalls: calls, goalStatus: status});
  const previous = {checkedAtUtc: new Date(now - 60_000).toISOString(), chats: [{taskKey: 'root', toolCalls: 10}, {taskKey: 'worker', toolCalls: 90}, {ownerId: 'solo', taskKey: null, toolCalls: 3}]};
  const rows = chatProgress([row('root', null, 20), row('worker', 'root', 4, 'completed'), row('nested', 'worker', 5), {id: 'solo', goalStatus: 'active', totalToolCalls: 7}, row('historical', null, 300, 'paused')], previous, now);
  assert.equal(rows.length, 4);
  assert.equal(rows[0].callsPerMinute, 10);
  assert.equal(rows[1].toolCallDelta, null);
  assert.equal(rows[1].counterReset, true);
  assert.equal(rows[2].taskKey, 'nested');
  assert.equal(rows[3].taskKey, null);
  assert.equal(rows[3].toolCallDelta, 4);
});

test('active workers remain visible after their coordinator completes or disappears', () => {
  const row = (task, parent, status) => ({id: task, agentTeam: {taskKey: task, parentTaskKey: parent, actionStatus: 'idle'}, totalToolCalls: 12, goalStatus: status});
  const chats = [row('finished-root', null, 'completed'), row('active-worker', 'finished-root', 'active'),
    row('nested-report', 'active-worker', 'completed'), row('paused-sibling', 'finished-root', 'paused'),
    row('orphan-worker', 'absent-root', 'active'), row('unrelated-complete', null, 'completed')];
  const result = chatProgress(chats, null, now);
  assert.deepEqual(result.map(chat => chat.taskKey), ['active-worker', 'nested-report', 'orphan-worker']);
  assert.equal(result[0].parentTaskKey, 'finished-root');
  assert.equal(result[2].parentTaskKey, 'absent-root');
});

async function localOwner(t, handler) {
  const server = createServer(handler);
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const directory = await mkdtemp(join(tmpdir(), 'codexify-performance-fixture-'));
  const descriptor = join(directory, 'owner-chat.json');
  await writeFile(descriptor, JSON.stringify({port: server.address().port, token: 'private-fixture-token'}));
  t.after(async () => {await unlink(descriptor); await rmdir(directory);});
  return directory;
}

test('owner sampling uses direct loopback HTTP independently of the global fetch dispatcher', async t => {
  const original = globalThis.fetch;
  globalThis.fetch = () => {throw new Error('Global fetch dispatcher must not receive owner credentials');};
  t.after(() => {globalThis.fetch = original;});
  const appRoot = await localOwner(t, (req, res) => {
    assert.equal(req.url, '/api/chats');
    assert.equal(req.headers.authorization, 'Bearer private-fixture-token');
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({chats: []}));
  });
  const result = await samplePerformance({appRoot});
  assert.deepEqual(result.chats, []);
  assert.equal(result.toolExecution.completed, 0);
  assert.equal(JSON.stringify(result).includes('private-fixture-token'), false);
});

test('owner redirects never forward the bearer token to another endpoint', async t => {
  let forwarded = 0;
  const destination = createServer((req, res) => {++forwarded; res.end('{}');});
  await new Promise(resolve => destination.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => destination.close(resolve)));
  const appRoot = await localOwner(t, (req, res) => {res.writeHead(307, {Location: `http://127.0.0.1:${destination.address().port}/elsewhere`}).end();});
  await assert.rejects(samplePerformance({appRoot}), error => error.code === 'HTTP_307');
  assert.equal(forwarded, 0);
});
