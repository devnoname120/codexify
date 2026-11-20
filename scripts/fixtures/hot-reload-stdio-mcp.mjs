// No dependencies or network: isolated MCP stdio child for Rust hot-reload tests.
// Every invocation is a separate operating-system process.
import { createInterface } from 'node:readline';
import { writeFileSync } from 'node:fs';

const version = process.argv[2] ?? 'old';
const pid = process.pid;
const input = createInterface({ input: process.stdin, crlfDelay: Infinity });

function respond(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');
}
function tool(name, description) {
  return { name, description, inputSchema: { type: 'object', properties: {}, additionalProperties: false } };
}
function replyToTool(id) {
  respond(id, { content: [{ type: 'text', text: version + ':' + pid }], isError: false });
}

for await (const line of input) {
  let request;
  try { request = JSON.parse(line); } catch { continue; }
  if (!request || !Object.hasOwn(request, 'id')) continue;
  const { id, method } = request;
  if (method === 'initialize') {
    respond(id, {
      protocolVersion: request.params?.protocolVersion ?? '2025-03-26',
      capabilities: { tools: {} },
      serverInfo: { name: 'isolated-hot-reload-stdio-fixture', version },
    });
  } else if (method === 'tools/list') {
    const tools = [tool('probe', 'Version probe ' + version), tool('slow', 'Slow probe ' + version)];
    if (version === 'new') tools.push(tool('new_tool', 'New tool in replacement generation'));
    respond(id, { tools });
  } else if (method === 'tools/call') {
    if (request.params?.name === 'slow') {
      if (process.env.HOT_RELOAD_SLOW_MARKER_FILE) {
        writeFileSync(process.env.HOT_RELOAD_SLOW_MARKER_FILE, String(pid));
      }
      setTimeout(() => replyToTool(id), 300);
    } else {
      replyToTool(id);
    }
  } else if (method === 'ping') {
    respond(id, {});
  } else {
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, error: { code: -32601, message: 'Method not found' } }) + '\n');
  }
}
