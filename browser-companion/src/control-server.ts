import { randomBytes } from "node:crypto";
import { once } from "node:events";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  rename,
  rm,
} from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { basename, dirname, join } from "node:path";
import { z } from "zod";
import { SingleWindowCoordinator } from "./browser-window";
import {validateNewChatProjectUrl} from "./new-chat";
import { asCompanionError,type BrowserStage,type BrowserFailureCode } from "./errors";
import {
  COMPANION_KIND,
  chatGptUrlSchema,
  opaqueIdSchema,
  MAX_DESCRIPTOR_BYTES,
  MAX_EVENT_LINE_BYTES,
  MAX_JSON_BYTES,
  PROTOCOL_VERSION,
  cancelActionRequestSchema,
  descriptorSchema,
  executeActionRequestSchema,
  conversationApprovalRequestSchema,
  revealWindowRequestSchema,
  type CompanionDescriptor,
} from "./protocol";

const HEARTBEAT_MS = 30_000;

export interface ControlServer {
  endpoint: string;
  token: string;
  descriptorPath: string;
  close(): Promise<void>;
}

function loopback(remote: string | undefined): boolean {
  return remote === "127.0.0.1" || remote === "::1" || remote === "::ffff:127.0.0.1";
}

function noStore(response: ServerResponse): void {
  response.setHeader("Cache-Control", "no-store");
  response.setHeader("X-Content-Type-Options", "nosniff");
  response.setHeader("Referrer-Policy", "no-referrer");
}

function json(response: ServerResponse, status: number, value: unknown): void {
  noStore(response);
  response.statusCode = status;
  response.setHeader("Content-Type", "application/json; charset=utf-8");
  response.end(JSON.stringify(value));
}

function text(response: ServerResponse, status: number, message: string): void {
  noStore(response);
  response.statusCode = status;
  response.setHeader("Content-Type", "text/plain; charset=utf-8");
  response.end(message);
}

async function readJson(request: IncomingMessage): Promise<unknown> {
  const contentType = request.headers["content-type"]?.split(";", 1)[0]?.trim();
  if (contentType !== "application/json") throw new ClientError(415, "Expected application/json");
  const declared = Number(request.headers["content-length"] ?? 0);
  if (Number.isFinite(declared) && declared > MAX_JSON_BYTES) throw new ClientError(413, "Request body is too large");
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of request) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += bytes.length;
    if (total > MAX_JSON_BYTES) throw new ClientError(413, "Request body is too large");
    chunks.push(bytes);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8") || "null");
  } catch {
    throw new ClientError(400, "Request body is invalid JSON");
  }
}

class ClientError extends Error {
  constructor(readonly status: number, message: string,readonly detail?:{code:BrowserFailureCode;phase:BrowserStage;message:string}) {
    super(message);
    this.name = "ClientError";
  }
}

function httpError(error: unknown): ClientError {
  if (error instanceof ClientError) return error;
  if (error instanceof z.ZodError) return new ClientError(400, "Request does not match the browser companion protocol");
  if (error instanceof Error && error.message === "Browser action was not found") {
    return new ClientError(404, "Browser action was not found");
  }
  const failure=asCompanionError(error);
  return new ClientError(409,failure.message,failure.phase?{code:failure.code,phase:failure.phase,message:failure.message}:undefined);
}

async function ensurePrivateDirectory(path: string): Promise<void> {
  await mkdir(path, { recursive:true, mode:0o700 });
  const metadata = await lstat(path);
  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    throw new Error("Browser companion descriptor directory must be a real directory, not a symlink");
  }
  if (process.platform !== "win32") await chmod(path, 0o700);
}

async function publishDescriptor(path: string, descriptor: CompanionDescriptor): Promise<void> {
  const parent = dirname(path);
  await ensurePrivateDirectory(parent);
  const existing = await lstat(path).catch(error => {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw error;
  });
  if (existing && (!existing.isFile() || existing.isSymbolicLink())) {
    throw new Error("Browser companion descriptor must be a regular non-symlink file");
  }
  const bytes = Buffer.from(`${JSON.stringify(descriptor)}\n`, "utf8");
  if (bytes.length > MAX_DESCRIPTOR_BYTES) throw new Error("Browser companion descriptor is too large");
  const temporary = join(parent, `.${basename(path)}.${process.pid}.${randomBytes(8).toString("hex")}.tmp`);
  const handle = await open(temporary, "wx", 0o600);
  try {
    await handle.writeFile(bytes);
    await handle.sync();
  } finally {
    await handle.close();
  }
  if (process.platform !== "win32") await chmod(temporary, 0o600);
  await rename(temporary, path);
  if (process.platform !== "win32") await chmod(path, 0o600);
}

async function removeOwnedDescriptor(path: string, token: string): Promise<void> {
  try {
    const metadata = await lstat(path);
    if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > MAX_DESCRIPTOR_BYTES) return;
    const current = descriptorSchema.parse(JSON.parse(await readFile(path, "utf8")));
    if (current.pid === process.pid && current.token === token) await rm(path, { force:true });
  } catch {
    // A replaced or corrupt descriptor belongs to neither this process nor cleanup.
  }
}

async function closeServer(server: Server): Promise<void> {
  await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
}

async function writeEvent(response: ServerResponse, value: unknown): Promise<void> {
  const line = `${JSON.stringify(value)}\n`;
  if (Buffer.byteLength(line) > MAX_EVENT_LINE_BYTES) throw new Error("Browser companion event is too large");
  if (!response.write(line)) await once(response, "drain");
}

async function route(
  coordinator: SingleWindowCoordinator,
  token: string,
  request: IncomingMessage,
  response: ServerResponse,
): Promise<void> {
  if (!loopback(request.socket.remoteAddress)) throw new ClientError(403, "Loopback requests only");
  if (request.headers.authorization !== `Bearer ${token}`) throw new ClientError(401, "Unauthorized");
  const url = new URL(request.url ?? "/", "http://127.0.0.1");
  const operation = new AbortController();
  request.once("aborted",()=>operation.abort());
  response.once("close",()=>{ if(!response.writableEnded)operation.abort(); });
  if(request.method === "GET" && url.pathname === "/v1/liveness") {
    json(response,200,await coordinator.liveness());return;
  }
  if (request.method === "GET" && url.pathname === "/v1/health") {
    json(response, 200, await coordinator.health());
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/session/login") {
    z.strictObject({}).parse(await readJson(request));
    json(response, 200, await coordinator.login(operation.signal));
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/session/capabilities") {
    z.strictObject({}).parse(await readJson(request));
    json(response, 200, await coordinator.capabilities(operation.signal));
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/account/plan") {
    const body=z.strictObject({epoch:z.string().min(1).max(128)}).parse(await readJson(request));
    try {json(response,200,await coordinator.accountPlan(body.epoch,operation.signal));}
    catch(error){if(error instanceof Error&&error.message.toLowerCase().includes("busy"))throw new ClientError(423,"Browser companion is busy");throw error;}return;
  }
  if(request.method==="POST"&&url.pathname==="/v1/navigation/availability") {
    const body=z.strictObject({url:chatGptUrlSchema}).parse(await readJson(request));
    json(response,200,await coordinator.navigationAvailability(body.url));return;
  }
  if(request.method === "POST" && url.pathname === "/v1/turns/handoff") {
    const body=z.strictObject({url:chatGptUrlSchema,expectedAccountFingerprint:opaqueIdSchema,handoffId:z.string().regex(/^renew-[a-f0-9]{64}$/)}).parse(await readJson(request));
    json(response,200,await coordinator.turnHandoffComplete(body.url,body.expectedAccountFingerprint,body.handoffId,operation.signal));return;
  }
  if(request.method === "POST" && url.pathname === "/v1/approvals/conversation") {
    const body=conversationApprovalRequestSchema.parse(await readJson(request));
    json(response,200,await coordinator.approvePendingConversation(body,operation.signal));return;
  }
  if (request.method === "POST" && url.pathname === "/v1/connectors/refresh") {
    const body=z.strictObject({name:z.string().min(1).max(256)}).parse(await readJson(request));
    try {json(response,200,await coordinator.refreshConnector(body.name,operation.signal));}
    catch(error){if(error instanceof Error&&error.message.toLowerCase().includes("busy"))throw new ClientError(423,"Browser companion is busy");throw error;}return;
  }
  if (request.method === "POST" && url.pathname === "/v1/new-chat/capabilities") {
    const body = z.strictObject({projectUrl:z.string().nullable()}).parse(await readJson(request));
    json(response, 200, await coordinator.newChatCapabilities(validateNewChatProjectUrl(body.projectUrl),operation.signal));
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/window/reveal") {
    const body = revealWindowRequestSchema.parse(await readJson(request));
    json(response, 200, await coordinator.reveal(body.url,operation.signal));
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/actions/execute") {
    const body = executeActionRequestSchema.parse(await readJson(request));
    json(response, 200, await coordinator.execute(body,operation.signal));
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/actions/cancel") {
    const body = cancelActionRequestSchema.parse(await readJson(request));
    json(response, 200, await coordinator.cancel(body));
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/actions/release-pairing") {
    const body=z.strictObject({actionId:opaqueIdSchema}).parse(await readJson(request));
    json(response,200,{released:coordinator.releasePairingWindow(body.actionId)});
    return;
  }
  if (request.method === "POST" && url.pathname === "/v1/actions/refresh-pairing") {
    const body=z.strictObject({actionId:opaqueIdSchema}).parse(await readJson(request));
    json(response,200,{retained:coordinator.refreshPairingWindow(body.actionId)});
    return;
  }
  const match = url.pathname.match(/^\/v1\/actions\/([A-Za-z0-9_.-]{1,256})(?:\/(events))?$/);
  if (request.method === "GET" && match) {
    const actionId = match[1]!;
    if (!match[2]) {
      const snapshot = await coordinator.action(actionId);
      if (!snapshot) throw new ClientError(404, "Browser action was not found");
      json(response, 200, snapshot);
      return;
    }
    const afterText = url.searchParams.get("after") ?? "0";
    if (!/^(?:0|[1-9][0-9]{0,19})$/.test(afterText)) throw new ClientError(400, "Invalid event cursor");
    const after = Number(afterText);
    if (!Number.isSafeInteger(after) || after < 0) throw new ClientError(400, "Invalid event cursor");
    if (!await coordinator.action(actionId)) throw new ClientError(404, "Browser action was not found");
    noStore(response);
    response.statusCode = 200;
    response.setHeader("Content-Type", "application/x-ndjson; charset=utf-8");
    response.setHeader("Connection", "keep-alive");
    response.flushHeaders();
    const cancellation = new AbortController();
    request.once("close", () => cancellation.abort());
    const heartbeat = setInterval(() => { if (!response.destroyed) response.write("\n"); }, HEARTBEAT_MS);
    heartbeat.unref();
    try {
      for await (const event of coordinator.events(actionId, after, cancellation.signal)) {
        await writeEvent(response, event);
      }
      response.end();
    } finally {
      clearInterval(heartbeat);
    }
    return;
  }
  throw new ClientError(404, "Not found");
}

export async function startControlServer(
  coordinator: SingleWindowCoordinator,
  descriptorPath: string,
): Promise<ControlServer> {
  const token = randomBytes(32).toString("base64url");
  const server = createServer((request, response) => {
    void route(coordinator, token, request, response).catch(error => {
      if (response.headersSent) {
        response.destroy();
        return;
      }
      const failure = httpError(error);
      if(failure.detail)json(response,failure.status,{protocolVersion:PROTOCOL_VERSION,build:coordinator.build,error:failure.detail});
      else text(response, failure.status, failure.message);
    });
  });
  server.requestTimeout = 0;
  server.headersTimeout = 30_000;
  server.keepAliveTimeout = 5_000;
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") {
    await closeServer(server);
    throw new Error("Browser companion control listener has no TCP address");
  }
  const endpoint = `http://127.0.0.1:${address.port}`;
  const descriptor: CompanionDescriptor = {
    version:PROTOCOL_VERSION,
    kind:COMPANION_KIND,
    pid:process.pid,
    endpoint,
    token,
    build:coordinator.build,
    createdAt:new Date().toISOString(),
  };
  try {
    descriptorSchema.parse(descriptor);
    await publishDescriptor(descriptorPath, descriptor);
  } catch (error) {
    await closeServer(server);
    throw error;
  }
  let closed = false;
  return {
    endpoint,
    token,
    descriptorPath,
    async close() {
      if (closed) return;
      closed = true;
      await coordinator.close();
      await closeServer(server);
      await removeOwnedDescriptor(descriptorPath, token);
    },
  };
}
