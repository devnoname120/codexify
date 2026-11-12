import {open, readFile, writeFile, rename, unlink} from 'node:fs/promises';
import {homedir} from 'node:os';
import {Agent, request} from 'node:http';
import {dirname, join, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';

const waiting = new Set(['chat_await', 'turn_completed']);
const coordination = new Set([
  'chat_read', 'chat_write', 'goal_read', 'goal_finish', 'agent_status',
  'agent_message', 'agent_control', 'agent_report', 'rollout_read',
  'rollout_update', 'update_plan', 'get_agent_brief', 'continue_task',
]);
const percentile = (values, fraction) => {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted.length ? sorted[Math.ceil(sorted.length * fraction) - 1] : null;
};

function toolCompletionHeader(line) {
  // PowerShell can write a complete progress document without a trailing newline
  // to the shared service log. Only skip that known leading envelope; searching
  // for timestamps anywhere would also accept log-shaped text inside payloads.
  const progress = '<Objs Version="1.1.0.1" xmlns="http://schemas.microsoft.com/powershell/2004/04">';
  while (line.startsWith(progress)) {
    const end = line.indexOf('</Objs>', progress.length);
    if (end < 0 || line.slice(progress.length, end).includes('<Objs')) return null;
    line = line.slice(end + '</Objs>'.length);
  }
  const prefix = /^(\d{4}-\d{2}-\d{2}T\S+)\s+(?:TRACE|DEBUG|INFO|WARN|ERROR)\s+codexify::(?:tool_payload|tool): (?:\[[^\]\r\n]+\] tool invocation completed|tool completed)\s+/.exec(line);
  if (!prefix) return null;
  // Only fields belonging to the actual completion event are eligible.
  const fields = line.slice(prefix[0].length).split(/ (?:request|response|output_summary|argument_summary)=/, 1)[0];
  return {timestamp: prefix[1], fields};
}

// Parse only tracing metadata. Never retain requests, results, commands or tokens.
export function toolTimings(text, {now = Date.now(), windowMs = 900_000} = {}) {
  const groups = new Map();
  const seen = new Set();
  let first = null, last = null;
  for (const line of text.split('\n')) {
    const header = toolCompletionHeader(line);
    if (!header) continue;
    const at = Date.parse(header.timestamp);
    if (!Number.isFinite(at) || at < now - windowMs || at > now) continue;
    const fields = /^call_id=(\d+)\b.*?\btool=(\S+)\b.*?\bstatus="(ok|error)"\s+duration_ms=(\d+)\b/.exec(header.fields);
    if (!fields) continue;
    const [, id, tool, status, duration] = fields;
    const key = `${header.timestamp}:${id}:${tool}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const group = groups.get(tool) ?? {durations: [], errors: 0};
    group.durations.push(Number(duration));
    group.errors += Number(status === 'error');
    groups.set(tool, group);
    first = Math.min(first ?? at, at);
    last = Math.max(last ?? at, at);
  }
  const byTool = [...groups].map(([tool, group]) => ({
    tool,
    category: waiting.has(tool) ? 'idle_wait' : tool === 'write_stdin' ? 'process_wait' : coordination.has(tool) ? 'coordination' : 'development',
    completed: group.durations.length,
    errors: group.errors,
    p50Ms: percentile(group.durations, 0.5),
    p95Ms: percentile(group.durations, 0.95),
    maxMs: Math.max(...group.durations),
  })).sort((a, b) => b.completed - a.completed || a.tool.localeCompare(b.tool));
  return {
    windowStartUtc: new Date(now - windowMs).toISOString(),
    windowEndUtc: new Date(now).toISOString(),
    firstObservedUtc: first === null ? null : new Date(first).toISOString(),
    lastObservedUtc: last === null ? null : new Date(last).toISOString(),
    completed: byTool.reduce((n, row) => n + row.completed, 0),
    nonIdleCompletions: byTool.filter(row => row.category !== 'idle_wait').reduce((n, row) => n + row.completed, 0),
    errors: byTool.reduce((n, row) => n + row.errors, 0),
    byTool,
    limits: 'Native execution durations, not model thinking or end-to-end MCP latency. Commands include requested test/build waits. Non-idle calls include coordination and do not establish productive output. Errors include command failures and message guards; inspect before treating them as transport failures. Bounded log tails can cover less than the requested window.',
  };
}

export function chatProgress(chats, previous, now = Date.now()) {
  // A coordinator can finish before its workers. Every active goal must seed
  // monitoring, even when its parent is completed, configured or absent.
  const roots = chats.filter(chat => chat.goalStatus === 'active');
  const rootIds = new Set(roots.map(chat => chat.id));
  const teamTasks = new Set(roots.map(chat => chat.agentTeam?.taskKey).filter(Boolean));
  // Include delegated descendants and retain standalone active goals before
  // their first worker exists. Neither case should disappear from monitoring.
  let added;
  do {
    added = false;
    for (const chat of chats) {
      const team = chat.agentTeam;
      if (team?.taskKey && teamTasks.has(team.parentTaskKey) && !teamTasks.has(team.taskKey)) {teamTasks.add(team.taskKey); added = true;}
    }
  } while (added);
  const old = new Map((previous?.chats ?? []).map(chat => [chat.taskKey || chat.ownerId, chat]));
  const elapsedMs = previous?.checkedAtUtc ? now - Date.parse(previous.checkedAtUtc) : null;
  return chats.filter(chat => rootIds.has(chat.id) || teamTasks.has(chat.agentTeam?.taskKey)).map(chat => {
    const taskKey = chat.agentTeam?.taskKey ?? null;
    const before = old.get(taskKey || chat.id);
    const toolCalls = chat.totalToolCalls ?? 0;
    const counterReset = !!before && toolCalls < before.toolCalls;
    const comparable = !!before && !counterReset && elapsedMs > 0;
    const delta = comparable ? toolCalls - before.toolCalls : null;
    return {
      ownerId: chat.id, taskKey, parentTaskKey: chat.agentTeam?.parentTaskKey ?? null,
      role: chat.agentTeam?.role ?? 'coordinator', goalStatus: chat.goalStatus,
      goalRevision: chat.goal?.revision ?? null, toolCalls, toolCallDelta: delta,
      callsPerMinute: comparable ? Math.round(delta * 60_000 / elapsedMs * 10) / 10 : null,
      counterReset,
      lastCallAtMs: chat.lastAgentCallAtMs ?? null,
      lastCallAgeMs: chat.lastAgentCallAtMs ? Math.max(0, now - chat.lastAgentCallAtMs) : null,
      actionStatus: chat.agentTeam?.actionStatus ?? null,
      hasAutomationError: !!chat.automationError,
    };
  });
}

async function logTail(path, maxBytes) {
  let handle;
  try {
    handle = await open(path, 'r');
    const {size} = await handle.stat();
    const start = Math.max(0, size - maxBytes);
    const buffer = Buffer.alloc(Math.min(size, maxBytes));
    const {bytesRead} = await handle.read(buffer, 0, buffer.length, start);
    let text = buffer.subarray(0, bytesRead).toString('utf8');
    if (start) text = text.slice(text.indexOf('\n') + 1);
    // A concurrent write may leave an incomplete final record. Read it next time.
    return {text: text.slice(0, text.lastIndexOf('\n') + 1), bytesRead, truncated: start > 0};
  } catch (error) {
    if (error.code === 'ENOENT') return {text: '', bytesRead: 0, truncated: false};
    throw error;
  } finally {
    await handle?.close();
  }
}

export async function samplePerformance({appRoot = join(homedir(), '.codexify'), previous = null, windowMs = 900_000} = {}) {
  const credentials = JSON.parse(await readFile(join(appRoot, 'owner-chat.json'), 'utf8'));
  if (!Number.isInteger(credentials.port) || credentials.port < 1 || credentials.port > 65535 || typeof credentials.token !== 'string' || !credentials.token) throw new Error('Invalid local owner descriptor');
  const started = performance.now();
  // Use an explicitly direct local connection. A process-wide fetch dispatcher
  // or environment proxy must never receive private owner credentials.
  const agent = new Agent({keepAlive: false, proxyEnv: {}});
  let payload;
  try {
    payload = await new Promise((resolve, reject) => {
      const req = request({hostname: '127.0.0.1', port: credentials.port, path: '/api/chats', agent,
        headers: {Authorization: `Bearer ${credentials.token}`}, signal: AbortSignal.timeout(25_000)}, response => {
        if (response.statusCode !== 200) {
          response.resume(); reject(Object.assign(new Error('Local owner status rejected'), {code: `HTTP_${response.statusCode}`})); return;
        }
        const chunks = []; let size = 0;
        response.on('data', chunk => {
          size += chunk.length;
          if (size > 8 * 1024 * 1024) req.destroy(Object.assign(new Error('Owner snapshot exceeds budget'), {code: 'OWNER_SNAPSHOT_TOO_LARGE'}));
          else chunks.push(chunk);
        });
        response.on('end', () => {try {resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));} catch (error) {reject(error);}});
        response.on('error', reject);
      });
      req.on('error', reject); req.end();
    });
  } finally {agent.destroy();}
  const {chats} = payload;
  if (!Array.isArray(chats)) throw new Error('Invalid owner chat snapshot');
  const ownerSnapshotMs = Math.round(performance.now() - started);
  const tails = await Promise.all(['codexify.log.1', 'codexify.log'].map(name => logTail(join(appRoot, 'logs', name), 4 * 1024 * 1024)));
  const now = Date.now();
  return {
    version: 1, checkedAtUtc: new Date(now).toISOString(),
    previousCheckedAtUtc: previous?.checkedAtUtc ?? null, ownerSnapshotMs,
    chats: chatProgress(chats, previous, now),
    toolExecution: {...toolTimings(tails.map(tail => tail.text).join('\n'), {now, windowMs}), bytesRead: tails.reduce((n, tail) => n + tail.bytesRead, 0), boundedTail: tails.some(tail => tail.truncated)},
    limits: 'Per-chat counters include waits and coordination. Log timing aggregates cover this local server and cannot reliably attribute durations to individual chats. Inspect exact queue/deadlines and changed worker reports only when needed; a call gap alone is not a stall.',
  };
}

async function main() {
  const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const output = process.argv[2] ? resolve(process.argv[2]) : join(repo, 'browser-companion/artifacts/ready-20261030/tool-speed-state.json');
  let previous;
  try {previous = JSON.parse(await readFile(output, 'utf8'));} catch (error) {if (error.code !== 'ENOENT') throw error;}
  const snapshot = await samplePerformance({previous});
  const temporary = `${output}.${process.pid}.tmp`;
  try {
    await writeFile(temporary, `${JSON.stringify(snapshot, null, 2)}\n`, {flag: 'wx'});
    await rename(temporary, output);
  } finally {
    await unlink(temporary).catch(error => {if (error.code !== 'ENOENT') throw error;});
  }
  console.log(JSON.stringify({state: 'sampled', output, checkedAtUtc: snapshot.checkedAtUtc, chats: snapshot.chats.length, completed: snapshot.toolExecution.completed, nonIdleCompletions: snapshot.toolExecution.nonIdleCompletions, ownerSnapshotMs: snapshot.ownerSnapshotMs}));
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => {
    // Error messages can contain descriptor JSON. Emit safe diagnostics only.
    console.error(JSON.stringify({state: 'failed_previous_state_preserved', error: error.name, code: error.code ?? error.cause?.code ?? null, frames: error.stack?.split('\n').filter(line => line.trim().startsWith('at ')).slice(0, 2)}));
    process.exitCode = 1;
  });
}
