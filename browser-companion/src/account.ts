// Adapted from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/limits.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
import { createHash } from "node:crypto";
import { z } from "zod";
import type { Frame, Page } from "patchright";
import { BrowserCompanionError } from "./errors";
import {observeReadOnly} from "./observation";

const sessionProjectionSchema = z.strictObject({
  userId:z.string().min(1).max(256),
  accountId:z.string().min(1).max(256),
  planType:z.string().min(1).max(256),
  structure:z.string().min(1).max(256),
  needsAttention:z.boolean(),
});

export type AccountSessionProjection = z.infer<typeof sessionProjectionSchema>;

export function validateAccountSessionProjection(value: unknown): AccountSessionProjection {
  return sessionProjectionSchema.parse(value);
}

export function fingerprintAccountIdentity(userId: string, accountId: string): string {
  const identity = sessionProjectionSchema.pick({ userId:true, accountId:true }).parse({ userId, accountId });
  return createHash("sha256")
    .update("codexify-browser-account-v1\0")
    .update(identity.userId)
    .update("\0")
    .update(identity.accountId)
    .digest("hex");
}

export async function readChatGptAccount(page: Page, signal?:AbortSignal): Promise<{
  fingerprint:string;
  planType:string;
  personal:boolean;
  needsAttention:boolean;
}> {
  if (new URL(page.url()).origin !== "https://chatgpt.com") {
    throw new Error("Open ChatGPT and sign in before inspecting account capabilities");
  }
  const projection = validateAccountSessionProjection(await observeReadOnly(page.evaluate(async () => {
    const response = await fetch("/api/auth/session", {
      credentials:"same-origin",
      cache:"no-store",
      redirect:"error",
      signal:AbortSignal.timeout(5_000),
    });
    const url = new URL(response.url);
    if (!response.ok || url.origin !== "https://chatgpt.com" || url.pathname !== "/api/auth/session") {
      throw new Error("ChatGPT account session could not be verified");
    }
    const session = await response.json();
    if ((session?.error != null && session.error !== "")
      || (session?.expires != null && (
        typeof session.expires !== "string"
        || !Number.isFinite(Date.parse(session.expires))
        || Date.parse(session.expires) <= Date.now()
      ))) {
      throw new Error("ChatGPT session is expired");
    }
    return {
      userId:session?.user?.id,
      accountId:session?.account?.id,
      planType:session?.account?.planType,
      structure:session?.account?.structure,
      needsAttention:session?.account?.isDelinquent === true,
    };
  }),signal));
  return {
    fingerprint:fingerprintAccountIdentity(projection.userId, projection.accountId),
    planType:projection.planType,
    personal:projection.structure === "personal",
    needsAttention:projection.needsAttention,
  };
}


export type ChatGptAccount = Awaited<ReturnType<typeof readChatGptAccount>>;
export const HEALTH_ACCOUNT_TTL_MS = 60_000;
export const HEALTH_ACCOUNT_FAILURE_TTL_MS = 5_000;

/** Only passive health polling uses this cache; action account checks stay fresh. */
export class HealthAccountCache {
  private page?: Page;
  private generation = 0;
  private navigation?: (frame: Frame) => void;
  private readonly closed = () => this.invalidate();
  private cached?: {page:Page;url:string;generation:number;until:number;result:{account:ChatGptAccount}|{error:unknown}};
  private pending?: {page:Page;url:string;generation:number;promise:Promise<ChatGptAccount>};

  constructor(private readonly now = () => performance.now()) {}

  invalidate(): void {
    this.generation++;
    this.cached = undefined;
    this.pending = undefined;
  }

  dispose(): void {
    if (this.page) {
      if (this.navigation) this.page.off("framenavigated", this.navigation);
      this.page.off("close", this.closed);
    }
    this.page = undefined;
    this.navigation = undefined;
    this.invalidate();
  }

  async read(page: Page): Promise<ChatGptAccount> {
    if (page !== this.page) {
      this.dispose();
      this.page = page;
      this.navigation = frame => { if (frame === page.mainFrame()) this.invalidate(); };
      page.on("framenavigated", this.navigation);
      page.on("close", this.closed);
    }
    if (page.isClosed()) { this.invalidate(); throw new BrowserCompanionError("The shared Chrome ChatGPT window was closed", "external_interference"); }
    const url = page.url(), generation = this.generation;
    const matches = (entry: {page:Page;url:string;generation:number}) => entry.page === page && entry.url === url && entry.generation === generation;
    const current = () => this.page === page && this.generation === generation && !page.isClosed() && page.url() === url;
    if (this.cached && matches(this.cached) && this.now() < this.cached.until) {
      if ("error" in this.cached.result) throw this.cached.result.error;
      return this.cached.result.account;
    }
    if (this.pending && matches(this.pending)) return this.pending.promise;
    const promise = readChatGptAccount(page).then(account => {
      if (!current()) throw new BrowserCompanionError("ChatGPT changed pages during the account health check", "external_interference");
      this.cached = {page,url,generation,until:this.now()+HEALTH_ACCOUNT_TTL_MS,result:{account}};
      return account;
    }).catch(error => {
      if (current()) this.cached = {page,url,generation,until:this.now()+HEALTH_ACCOUNT_FAILURE_TTL_MS,result:{error}};
      throw error;
    }).finally(() => { if (this.pending?.promise === promise) this.pending = undefined; });
    this.pending = {page,url,generation,promise};
    return promise;
  }
}
