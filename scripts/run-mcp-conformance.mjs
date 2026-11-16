// MCP server smoke checks against an isolated Codexify process.
// This is a baseline, not a claim of full protocol conformance.
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const available = ["server-initialize", "tools-list"];
const requested = process.env.CODEXIFY_CONFORMANCE_SCENARIOS || "both";
const scenarios = requested === "both" ? available : [requested];
if (scenarios.some((scenario) => !available.includes(scenario))) {
  throw new Error("Unknown scenario: " + requested);
}
if (process.env.CI !== "true" && process.env.CODEXIFY_CONFORMANCE_ALLOW_LOCAL !== "1") {
  throw new Error(
    "Refusing to start an unauthenticated test server outside CI. " +
    "Set CODEXIFY_CONFORMANCE_ALLOW_LOCAL=1 only on a trusted, isolated machine."
  );
}

async function freePort() {
  const listener = createServer();
  await new Promise((ok, fail) => {
    listener.once("error", fail);
    listener.listen(0, "127.0.0.1", ok);
  });
  const address = listener.address();
  if (!address || typeof address === "string") {
    listener.close();
    throw new Error("Could not allocate a test port");
  }
  await new Promise((ok) => listener.close(ok));
  return address.port;
}

async function waitForHealth(url, server, getError) {
  for (let attempt = 0; attempt < 120; attempt++) {
    if (getError() || server.exitCode !== null) {
      throw new Error("Codexify exited during startup: " + (getError()?.message || server.exitCode));
    }
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(800) });
      if (response.ok) return;
    } catch {
      // A connection refused before the listener starts is expected.
    }
    await delay(250);
  }
  throw new Error("Isolated Codexify did not become healthy in 30 seconds");
}

async function runScenario(cwd, url, scenario) {
  const executable = process.platform === "win32" ? "npx.cmd" : "npx";
  const args = [
    "--yes", "@modelcontextprotocol/conformance@0.1.16",
    "server", "--url", url, "--scenario", scenario
  ];
  process.stdout.write("MCP conformance: " + scenario + "\n");
  await new Promise((ok, fail) => {
    const child = spawn(executable, args, {
      cwd,
      stdio: "inherit",
      windowsHide: true,
      shell: process.platform === "win32"
    });
    const timeout = setTimeout(() => {
      child.kill();
      fail(new Error("Conformance scenario timed out: " + scenario));
    }, 180000);
    child.once("error", (error) => {
      clearTimeout(timeout);
      fail(error);
    });
    child.once("close", (code) => {
      clearTimeout(timeout);
      if (code === 0) ok();
      else fail(new Error("Conformance scenario failed (" + scenario + "): exit " + code));
    });
  });
}

const scratch = await mkdtemp(join(tmpdir(), "codexify-mcp-conformance-"));
const workspace = join(scratch, "workspace");
const configPath = join(scratch, "codexify.config.json");
const port = await freePort();
const binary = resolve(
  process.env.CODEXIFY_CONFORMANCE_BIN ||
    join("target", "debug", process.platform === "win32" ? "codexify.exe" : "codexify")
);
const url = "http://127.0.0.1:" + port + "/mcp";
let server;
let serverError;
let output = "";

try {
  await mkdir(workspace);
  await writeFile(configPath, JSON.stringify({
    schemaVersion: 1,
    workDir: workspace,
    projectCloneDir: workspace,
    port,
    agentChat: { enabled: false },
    worktrees: { mode: "never", autoCleanupEnabled: false },
    codexMcp: { enabled: false, useCli: false },
    projectCatalog: { codexConfig: { enabled: false }, entries: [] },
    skills: { enabled: false },
    memory: { enabled: false },
    mcpServers: {}
  }, null, 2));
  server = spawn(binary, ["--config", configPath, "--work-dir", workspace, "--port", String(port)], {
    cwd: scratch,
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true
  });
  server.on("error", (error) => { serverError = error; });
  const capture = (chunk) => { output = (output + chunk.toString()).slice(-12000); };
  server.stdout.on("data", capture);
  server.stderr.on("data", capture);
  await waitForHealth("http://127.0.0.1:" + port + "/health", server, () => serverError);
  for (const scenario of scenarios) await runScenario(scratch, url, scenario);
  process.stdout.write("MCP smoke scenarios passed: " + scenarios.join(", ") + "\n");
} catch (error) {
  process.stderr.write(String(error) + "\nIsolated server log (tail):\n" + output + "\n");
  process.exitCode = 1;
} finally {
  if (server && server.exitCode === null) {
    server.kill();
    await Promise.race([
      new Promise((ok) => server.once("close", ok)),
      delay(5000)
    ]);
  }
  await rm(scratch, { recursive: true, force: true });
}
