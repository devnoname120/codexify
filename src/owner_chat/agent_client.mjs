import {readFile} from 'node:fs/promises';
import {resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {setTimeout as delay} from 'node:timers/promises';

const readActions = new Set(['capabilities', 'list', 'status']);
const transientConnections = new Set(['ECONNREFUSED', 'ECONNRESET', 'ETIMEDOUT', 'UND_ERR_SOCKET']);

// Published beside connection.json by Codexify. No packages or app adapters.
export async function connection() {
  const value = JSON.parse(await readFile(new URL('./connection.json', import.meta.url), 'utf8'));
  if (value.version !== 1 || value.enabled !== true) throw new Error('Codexify agent control is disabled.');
  const url = new URL(value.requestUrl);
  if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || url.pathname !== '/api/agent-client/request' || url.username || url.password || url.search || url.hash) {
    throw new Error('Invalid Codexify agent connection.');
  }
  if (!/^Bearer codexify_agents_[a-f0-9]{64}$/.test(value.headers?.Authorization ?? '')) throw new Error('Invalid Codexify agent credential.');
  return value;
}

export async function agent(request, {signal, timeoutMs = 180_000} = {}) {
  // Freeze arguments before any await, including for a retried read.
  const body = JSON.stringify(request);
  const readOnly = readActions.has(typeof body === 'string' ? JSON.parse(body)?.action : undefined);
  const deadline = AbortSignal.timeout(timeoutMs);
  const cancellation = signal ? AbortSignal.any([signal, deadline]) : deadline;
  for (let attempt = 0; ; attempt++) {
    try {
      cancellation.throwIfAborted();
      const value = await connection(); // Follow port changes on each attempt.
      cancellation.throwIfAborted();
      const response = await fetch(value.requestUrl, {
        method: 'POST', redirect: 'error',
        headers: {'Content-Type': 'application/json', 'Authorization': value.headers.Authorization},
        body, signal: cancellation,
      });
      const reply = await response.text();
      if (!response.ok) throw new Error(`Codexify agent request failed (${response.status}): ${reply.slice(0, 2000)}`);
      return JSON.parse(reply);
    } catch (error) {
      // Reads get one bounded transport retry. Mutation outcomes still require
      // status inspection and the caller's original request ID and arguments.
      if (!readOnly || attempt !== 0 || cancellation.aborted
        || !transientConnections.has(error.cause?.code ?? error.code)) throw error;
      try { await delay(200, undefined, {signal:cancellation}); }
      catch (error) { cancellation.throwIfAborted(); throw error; }
    }
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try {
    if (!process.argv[2] || process.argv[2] === '--help') {
      console.log(`Use: node "${fileURLToPath(import.meta.url)}" '{"action":"capabilities"}'\nImport: const {agent} = await import(pathToFileURL(helperPath).href); await agent({action:'list'});`);
    } else {
      console.log(JSON.stringify(await agent(JSON.parse(process.argv[2])), null, 2));
    }
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
