# Codexify — Local MCP Bridge for ChatGPT Web Pro

**Date:** 2026-08-06
**Status:** Approved
**Stack:** Bun + TypeScript
**Entry:** `bun run main.ts --work-dir ../project`

## Problem

ChatGPT Web Pro supports MCP connectors (Settings > Connectors) but cannot access local filesystems or run local commands. A bridge server is needed to expose local development tools as MCP tools over SSE, allowing ChatGPT to read/write files, run commands, and interact with git — all within a user-specified project directory.

This works in regular ChatGPT chat mode (no need for "Work" mode). The user configures the MCP server URL in ChatGPT's connector settings, and tools become available in all conversations.

## Architecture

**Pattern:** Registry-based tool system. Each tool is a self-describing module. A registry auto-collects all tools and registers them with the MCP server.

### Project Structure

```
codexify/
├── main.ts                   # Entry: parse CLI args, boot server
├── package.json
├── tsconfig.json
├── codex.config.json          # Default config (allowlist, port, etc.)
├── README.md                  # English documentation
├── LICENSE                    # MIT
├── src/
│   ├── types.ts               # ToolDefinition, AppConfig, ToolResult
│   ├── config.ts              # Load & merge config (defaults → file → CLI)
│   ├── auth.ts                # Optional bearer token middleware
│   ├── server.ts              # MCP Server + SSE transport + CORS
│   ├── registry.ts            # Scan tools/, collect, validate, register
│   └── tools/
│       ├── read-file.ts       # read_file
│       ├── write-file.ts      # write_file
│       ├── run-command.ts     # run_command (allowlist enforced)
│       ├── git-status.ts      # git_status
│       ├── git-push.ts        # git_push
│       ├── glob.ts            # glob
│       ├── grep.ts            # grep
│       ├── list-directory.ts  # list_directory
│       └── tree.ts            # tree
```

### Boot Sequence

```
bun run main.ts --work-dir ../project [--api-key xxx] [--port 3000]
  │
  1. Parse CLI args (util.parseArgs — zero dependency)
  2. Load config: merge defaults → codex.config.json → CLI flags
  3. Validate work-dir exists and is a directory
  4. Registry: import all tool modules from src/tools/
  5. Create MCP Server, register tools from registry
  6. Create HTTP server with SSE transport
  │   ├─ If --api-key set → attach auth middleware
  │   └─ Bind to 0.0.0.0:{port}
  7. Log: "MCP Bridge running on http://localhost:{port}"
```

## Tool Definition Interface

Every tool module default-exports a `ToolDefinition`:

```typescript
interface ToolDefinition {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;  // JSON Schema
  handler: (args: Record<string, unknown>, config: AppConfig) => Promise<ToolResult>;
}

interface ToolResult {
  content: Array<{ type: "text"; text: string }>;
  isError?: boolean;
}

interface AppConfig {
  workDir: string;
  apiKey?: string;
  port: number;
  allowedCommands: string[];
  tree: { defaultDepth: number; ignore: string[] };
  command: { defaultTimeout: number; maxTimeout: number };
}
```

### Registry

`src/registry.ts` handles:
- Import all tool files from `src/tools/`
- Each file exports `default` as a `ToolDefinition`
- Validate no duplicate tool names
- Return array of tools for server registration

## Tools Specification

### File Operations

| Tool | Params | Description |
|------|--------|-------------|
| `read_file` | `path` (required), `offset?`, `limit?` | Read file contents with optional line-based pagination. Returns content with line numbers. |
| `write_file` | `path` (required), `content` (required) | Write full content to file. Creates parent directories if needed. |

### Search & Navigation

| Tool | Params | Description |
|------|--------|-------------|
| `glob` | `pattern` (required), `path?` | Find files matching glob pattern (e.g. `**/*.ts`). Returns list of relative paths. |
| `grep` | `pattern` (required), `path?`, `include?`, `context?` | Search file contents by regex. Returns file + line number + matching line. Uses spawned `grep` or `rg`. |
| `list_directory` | `path?` | List files/dirs in a directory. Returns name, type (file/dir), and size. Defaults to workDir root. |
| `tree` | `path?`, `depth?` | ASCII directory tree. Default depth=3. Auto-ignores `node_modules`, `.git`, etc. (configurable). |

### Command Execution

| Tool | Params | Description |
|------|--------|-------------|
| `run_command` | `command` (required), `args?`, `timeout?` | Execute a command in workDir. Only commands in the allowlist are permitted. Default timeout 30s. Returns stdout + stderr + exit code. |

**Allowlist mechanism:**
- `codex.config.json` contains `allowedCommands: ["bun", "npm", "node", "git", ...]`
- Only the binary name (first token) is checked, not arguments
- Rejected commands return a clear error listing all allowed commands

### Git Operations

| Tool | Params | Description |
|------|--------|-------------|
| `git_status` | _(none)_ | Run `git status --porcelain` in workDir. Returns parsed list of changed files with status codes. |
| `git_push` | `remote?`, `branch?` | Run `git push`. Defaults to `origin` + current branch. Returns command output. |

These are separate from `run_command` because:
- No allowlist check needed (they are dedicated, scoped tools)
- Output is parsed and formatted for LLM readability
- Reduced risk of dangerous git operations

## Server & Transport

### HTTP Endpoints

```
GET  /sse        → SSE connection (client subscribes here)
POST /messages   → JSON-RPC messages from client (tools/list, tools/call)
GET  /health     → Health check (always public, no auth)
```

### SSE Flow

1. ChatGPT opens `GET /sse` → server creates SSE stream, sends `endpoint` event with POST URL
2. ChatGPT sends `POST /messages?sessionId=xxx` with JSON-RPC payload
3. Server processes request, returns result via the open SSE stream

### CORS

Required because ChatGPT calls from `https://chatgpt.com`:

```
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET, POST, OPTIONS
Access-Control-Allow-Headers: Authorization, Content-Type
```

Preflight `OPTIONS` returns 204 immediately.

### Authentication

```
Request arrives
  │
  ├─ --api-key NOT set → pass through (no auth enforced)
  │
  └─ --api-key IS set
       ├─ "Authorization: Bearer <key>" matches → pass
       └─ Missing or mismatch → 401 Unauthorized
```

- Auth check runs before SSE/messages handlers
- `/health` is always public

## Security

### Path Traversal Prevention

Every tool that operates on file paths must:
1. Resolve the input path relative to `workDir`
2. Normalize the resolved path
3. Verify the final absolute path starts with `workDir`
4. Reject with "Path must be within work directory" if it escapes

### Command Allowlist

`run_command` extracts the binary name and checks against `allowedCommands`. This prevents arbitrary command execution while allowing the user to configure which tools are available.

## Configuration

### CLI Flags

```
bun run main.ts [options]

  --work-dir <path>     Required. Project directory for all tool operations.
  --port <number>       Listen port. Default: 3000.
  --api-key <string>    Bearer token for authentication. No auth if omitted.
  --config <path>       Path to config file. Default: ./codex.config.json
```

Parsed with `util.parseArgs` (Node-compatible, zero dependency in Bun).

### Config File: `codex.config.json`

```json
{
  "allowedCommands": ["bun", "npm", "npx", "node", "git", "python", "pip", "cargo", "make"],
  "port": 3000,
  "tree": {
    "defaultDepth": 3,
    "ignore": ["node_modules", ".git", "dist", ".next", "__pycache__"]
  },
  "command": {
    "defaultTimeout": 30000,
    "maxTimeout": 120000
  }
}
```

### Merge Order

`defaults → config file → CLI flags` (CLI wins)

## Error Handling

| Scenario | Response |
|----------|----------|
| Path traversal attempt | `isError: true`, "Path must be within work directory" |
| Command not in allowlist | `isError: true`, "Command not allowed. Allowed: bun, npm, ..." |
| File not found | `isError: true`, "File not found: <path>" |
| Command timeout | Kill process, `isError: true`, "Command timed out after {n}s" |
| Tool handler throws | MCP error response with `isError: true` + error message |

## Dependencies

| Package | Purpose |
|---------|---------|
| `@modelcontextprotocol/sdk` | MCP server, SSE transport, tool registration |
| `glob` (or `fast-glob`) | Glob pattern matching for the glob tool |

Bun built-ins cover: HTTP server, file I/O, child process spawning, `parseArgs`, path utilities.

## Deliverables

- All source files per project structure above
- `README.md` — English documentation covering setup, usage, tool descriptions, config
- `LICENSE` — MIT license
- `codex.config.json` — Default configuration
- `package.json` — with `"scripts": { "start": "bun run main.ts" }`
- `tsconfig.json` — Bun-compatible TypeScript config
