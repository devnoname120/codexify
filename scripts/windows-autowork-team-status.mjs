import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';

const message = error => String(error?.message ?? error).slice(0, 1000);

// Independent, authenticated team observations. These never replace the owner
// inventory or authorize filesystem discovery, setup, control or Send retries.
export async function monitorTeams({helperPath, timeoutMs = 20_000}) {
  const started = Date.now();
  const result = {
    available:false, discoveryComplete:false, allStatusesAvailable:false,
    checkedAtUtc:new Date().toISOString(), elapsedMs:0, teams:null, error:null,
  };
  try {
    const signal = AbortSignal.timeout(timeoutMs);
    const {agent} = await import(pathToFileURL(resolve(helperPath)).href);
    const teams = [];
    const identities = new Set(), cursors = new Set();
    let afterTeamId;
    try {
      for (;;) {
        signal.throwIfAborted();
        const page = await agent({action:'list', ...(afterTeamId ? {afterTeamId} : {})}, {signal, timeoutMs});
        if (!Array.isArray(page.teams)) throw new Error('Invalid published team inventory');
        result.available = true;
        result.teams = teams;
        for (const item of page.teams) {
          if (typeof item.teamId !== 'string' || !item.teamId || identities.has(item.teamId)) {
            throw new Error('Invalid or repeated published team identity');
          }
          identities.add(item.teamId);
          teams.push({teamId:item.teamId, clientName:item.clientName, ready:item.ready,
            statusAvailable:false, capacity:null, allowDelegation:null, workers:null, error:null});
        }
        if (!page.hasMore) { result.discoveryComplete = true; break; }
        if (typeof page.nextAfterTeamId !== 'string' || !page.nextAfterTeamId || cursors.has(page.nextAfterTeamId)) {
          throw new Error('Published team pagination did not advance');
        }
        afterTeamId = page.nextAfterTeamId;
        cursors.add(afterTeamId);
      }
    } catch (error) { result.error = message(error); }

    let next = 0;
    // Bound monitoring I/O, independently of the runtime's worker capacity.
    await Promise.all(Array.from({length:Math.min(4, teams.length)}, async () => {
      for (;;) {
        const item = teams[next++];
        if (!item) return;
        try {
          signal.throwIfAborted();
          const {team} = await agent({action:'status', teamId:item.teamId}, {signal, timeoutMs});
          if (!Array.isArray(team?.workers)) throw new Error('Invalid published team status');
          item.capacity = team.capacity;
          item.allowDelegation = team.allowDelegation;
          item.workers = team.workers.map(worker => ({
            taskKey:worker.taskKey, role:worker.role, scope:worker.scope, goal:worker.goal?.status ?? null,
            goalRevision:worker.goal?.revision ?? null, stopped:worker.stopped,
            retired:worker.retired, action:worker.action, error:worker.lastError ?? null,
            calls:worker.toolCallCount, lastCallAtMs:worker.lastCallAtMs ?? null,
            queue:worker.queuePosition ?? null,
            blockedReason:worker.goal?.blockedReason ? String(worker.goal.blockedReason).slice(0, 1500) : null,
          }));
          item.statusAvailable = true;
        } catch (error) { item.error = message(error); }
      }
    }));
    result.allStatusesAvailable = result.discoveryComplete && teams.every(team => team.statusAvailable);
  } catch (error) { result.error = message(error); }
  result.elapsedMs = Date.now() - started;
  return result;
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  const result = await monitorTeams({helperPath:process.argv[2]});
  console.log(JSON.stringify(result));
  if (!result.allStatusesAvailable) process.exitCode = 1;
}
