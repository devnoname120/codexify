# One agent connection for any AI

Codexify exposes the same agent controls through MCP, HTTP, and a JavaScript
helper. Any AI client with access to those interfaces can create and manage
teams. There are no app-specific adapters or connection profiles.

## Start once with the tunnel

Enable **Agent teams** and **AI agent control** in Settings, with browser
automation and the ChatGPT connector configured. These added features remain
off by default. Restart Codexify once.

The normal service starts the agent endpoint before its tunnel and automatically
publishes these files in `~/.codexify/agents/`:

- `connection.json`: the live MCP URL, HTTP request/schema URLs, authentication
  headers, helper path, and short instructions.
- `client.mjs`: a dependency-free JavaScript client and command-line helper.

The connection is available whenever the service is running. A restart updates
the published port automatically, and the JavaScript helper rereads the
connection before each request. No separate agent server needs to be launched.

```sh
codexify agents connection
codexify agents instructions
```

The first command prints the same universal connection as JSON. The second
prints short instructions suitable for an AI's prompt. Keep the connection file
private: its credential permits agent management. Disabling AI agent control
and restarting removes the endpoints and clears the published credential.

## Use whichever interface your AI has

| Interface | Connection |
| --- | --- |
| MCP | Use the published `url` and `headers`; call `agent_client` |
| HTTP | POST action JSON to `requestUrl` with the published `headers` |
| JavaScript | Import `agent` from the published `javascript` path |
| Shell | Run `node <javascript-path> '<action JSON>'` |

The authenticated `schemaUrl` returns the input schema and instructions.
All interfaces use identical action arguments, team IDs, reports, and controls.
The localhost endpoint starts with native tunnel or direct MCP installations;
the ordinary tunnel connector also advertises `agent_client` when enabled.

JavaScript, using Node.js 20.3 or newer:

```js
import {execFileSync} from 'node:child_process';
import {pathToFileURL} from 'node:url';

const connection = JSON.parse(execFileSync('codexify', ['agents', 'connection'], {encoding:'utf8'}));
const {agent} = await import(pathToFileURL(connection.javascript).href);
const catalog = await agent({action:'capabilities'});
```

The helper needs no npm packages and never follows HTTP redirects with the
credential. The connection is authenticated on every request. Its scoped
credential does not authorize the owner UI or the tunnel's private MCP hop.

## Short AI instructions

> Discover the connection with `codexify agents connection`. Call capabilities,
> then setup with a stable requestId, clientName, projectPath, model and effort;
> save teamId. Give workers concrete goals and distinct scopes. Read status and
> inbox, verify reports, and use control for messages, goals, pause/resume, stop,
> or retirement. Read fresh expectedRevision values before lifecycle changes.
> Retry with the same requestId and arguments. Workers implement authorized
> assignments by default; use readOnly:true for audits. A read-only parent
> cannot create editing workers.
> Respect stops and coordinate through tools.

## Actions

| Action | Arguments and result |
| --- | --- |
| `capabilities` | Returns current model and effort choices from the managed browser |
| `setup` | Requires `requestId`, `clientName`, `projectPath`, `model`, `effort`; returns persistent `teamId` |
| `list` | Recovers teams after reconnect; paginate with `nextAfterTeamId` as `afterTeamId` |
| `status` | Requires `teamId`; includes workers, scopes, goal revisions, and compact reports; use `includeReports:true` for full evidence |
| `inbox` | Requires `teamId`; reads messages without consuming them; supports `knownRevision` and older-page `before` |
| `spawn` | Requires `teamId`, stable `requestId`, and up to eight `workers` |
| `control` | Requires `teamId` and a nested `control` object with `requestId`, `workerTaskKey`, and `action` |
| `stop_team` | Requires `teamId` and stable `requestId`; stops the coordinator and its workers |

Setup optionally accepts `workers` and `createWorktree` (default false). Each
worker has `role`, `goal`, `scope`, and `readOnly`. Workers share the team's
workspace; assign separate scopes and use `readOnly:true` for an audit.

Control actions are `message`, `set_goal`, `pause`, `resume`, `stop`, and `retire`.
Supply `content` for messages and new goals; use `expectedRevision` for lifecycle
and goal changes. `set_goal` can also update `scope`. Retirement requires reviewed,
completed work. Messages do not resume stopped, paused, or completed assignments.

Teams, reports, and inboxes survive reconnects and service restarts. Retrying an
interrupted setup with identical arguments recovers its team. `spawnError` means
the team was saved but the worker batch needs attention. Replaying setup never
reverses a stop; use individual worker pause/resume for temporary pauses, or a
new setup request ID for work after `stop_team`.

Connected AIs share the local operator's agent-management authority. Controls
stay within the selected team, project, and account. Worker messages and reports
are delivered through tools; no project coordination file is used. Read-only
assignments and owned scopes are cooperative rules, so verify edits and evidence
before integration.
