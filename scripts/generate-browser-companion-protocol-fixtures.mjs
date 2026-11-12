#!/usr/bin/env node
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const schemaPath = join(root, "browser-companion", "protocol-schema-v1.json");
const outputPath = join(root, "browser-companion", "protocol-fixtures", "v1.json");
const schema = await readFile(schemaPath);
const schemaHash = createHash("sha256").update(schema).digest("hex");
const build = `fixture+protocol.${schemaHash}`;

const baseAction = {
  actionId:"fixture-action",
  idempotencyKey:"fixture-execute",
  url:"https://chatgpt.com/c/fixture",
  pairingGeneration:7,
  connectorName:"Codexify",
  expectedAccountFingerprint:"fixture-account",
  model:"gpt-6-sol",
  effort:"high",
  expectedUrl:"https://chatgpt.com/c/fixture",
  autoApproveToolCalls:false,
};

const fixture = {
  schemaHash,
  build,
  descriptor:{
    version:1,
    kind:"codexify-browser-companion",
    pid:1234,
    endpoint:"http://127.0.0.1:43123",
    token:"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    build,
    createdAt:"2026-10-09T12:00:00Z",
  },
  health:{
    protocolVersion:1,
    kind:"codexify-browser-companion",
    build,
    status:"idle",
    authenticated:true,
    accountFingerprint:"fixture-account",
    activeActionId:null,
  },
  capabilities:{
    protocolVersion:1,
    build,
    accountFingerprint:"fixture-account",
    catalogRevision:"fixture-catalog",
    models:[{
      id:"gpt-6-sol",
      label:"GPT-6 Sol",
      efforts:[
        { id:"high", label:"High", available:true, reason:null },
        { id:"xhigh", label:"Extra High", available:false, reason:"not_available_for_account" },
      ],
    }],
  },
  revealRequest:{ url:"https://chatgpt.com/c/fixture" },
  revealReceipt:{
    protocolVersion:1,
    build,
    revealed:true,
    activeActionId:null,
  },
  executeRequests:[
    { ...baseAction, actionId:"fixture-pair", idempotencyKey:"fixture-pair-execute", model:null, effort:null, prompt:{ kind:"pair_verify", reference:"A".repeat(43) } },
    { ...baseAction, actionId:"fixture-chat", idempotencyKey:"fixture-chat-execute", prompt:{ kind:"offline_chat", batchId:"fixture-batch", messages:[{ id:"fixture-message", markdown:"Preserve <data> & Markdown." }] } },
    { ...baseAction, actionId:"fixture-goal", idempotencyKey:"fixture-goal-execute", prompt:{ kind:"offline_goal", goalRevision:3 } },
    { ...baseAction, prompt:{ kind:"offline_combined", batchId:"fixture-batch", messages:[{ id:"fixture-message", markdown:"Preserve <data> & Markdown." }], goalRevision:3 } },
    {...baseAction,actionId:"fixture-renewal",idempotencyKey:"fixture-renewal-execute",prompt:{kind:"turn_renewal",handoffId:`renew-${"a".repeat(64)}`}},
    {...baseAction,actionId:"fixture-continuation",idempotencyKey:"fixture-continuation-execute",prompt:{kind:"task_continuation"}},
    {...baseAction,actionId:"fixture-retained",idempotencyKey:"fixture-retained-execute",url:"https://chatgpt.com/new",expectedUrl:"https://chatgpt.com/new",model:null,effort:null,retainPairingWindow:true,prompt:{kind:"create_chat",continuationToken:"A".repeat(43),reference:"A".repeat(43),setupReference:null}},
    {...baseAction,actionId:"fixture-paired-followup",idempotencyKey:"fixture-paired-followup-execute",pairingContinuationOf:"fixture-retained",prompt:{kind:"offline_goal",goalRevision:3}},
    {...baseAction,actionId:"fixture-ticket",idempotencyKey:"fixture-ticket-execute",agentTicket:"FreshT_1",prompt:{kind:"task_continuation"}},
  ],
  actionSnapshot:{
    protocolVersion:1,
    build,
    actionId:"fixture-action",
    state:"generating",
    sendAccepted:true,
    canonicalUrl:"https://chatgpt.com/c/fixture",
    selected:{ model:"gpt-6-sol", effort:"high" },
    lastEventSequence:8,
    attention:null,
  },
  waitingSnapshot:{
    protocolVersion:1,
    build,
    actionId:"fixture-waiting",
    state:"waiting",
    sendAccepted:false,
    canonicalUrl:null,
    selected:null,
    lastEventSequence:2,
    attention:null,
    cooldownUntilMs:180000,
  },
  cancelRequest:{ actionId:"fixture-action", idempotencyKey:"fixture-cancel" },
  cancelReceipt:{
    protocolVersion:1,
    build,
    actionId:"fixture-action",
    state:"ambiguous",
    cancelled:true,
  },
  events:[
    { sequence:1, actionId:"fixture-action", atMs:1, type:"queued" },
    { sequence:2, actionId:"fixture-action", atMs:2, type:"navigating" },
    { sequence:3, actionId:"fixture-action", atMs:3, type:"composer_ready" },
    { sequence:4, actionId:"fixture-action", atMs:4, type:"model_verified", model:"gpt-6-sol", effort:"high" },
    { sequence:5, actionId:"fixture-action", atMs:5, type:"connector_verified" },
    { sequence:6, actionId:"fixture-action", atMs:6, type:"send_accepted" },
    { sequence:7, actionId:"fixture-action", atMs:7, type:"status_delta", text:"Working", continuation:false },
    { sequence:8, actionId:"fixture-action", atMs:8, type:"commentary_delta", markdown:"Visible commentary", continuation:false },
    { sequence:9, actionId:"fixture-action", atMs:9, type:"answer_delta", markdown:"Answer", continuation:true },
    { sequence:10, actionId:"fixture-action", atMs:10, type:"submission_complete", canonicalUrl:"https://chatgpt.com/c/fixture" },
    { sequence:11, actionId:"fixture-action", atMs:11, type:"generation_complete", finalMarkdown:"Answer", canonicalUrl:"https://chatgpt.com/c/fixture" },
    { sequence:12, actionId:"fixture-action", atMs:12, type:"approval_required", message:"Approve once" },
    { sequence:13, actionId:"fixture-action", atMs:13, type:"authentication_required" },
    { sequence:14, actionId:"fixture-action", atMs:14, type:"rate_limited", message:"Cooldown" },
    { sequence:15, actionId:"fixture-action", atMs:15, type:"external_interference", message:"Draft changed" },
    { sequence:16, actionId:"fixture-action", atMs:16, type:"ambiguous_send", message:"Send acceptance uncertain" },
    { sequence:17, actionId:"fixture-action", atMs:17, type:"failed", code:"selector_drift", message:"Selector changed" },
    { sequence:18, actionId:"fixture-action", atMs:18, type:"conversation_created", canonicalUrl:"https://chatgpt.com/c/fixture" },
    { sequence:19, actionId:"fixture-waiting", atMs:19, type:"waiting", availableAtMs:180000 },
  ],
};

await mkdir(dirname(outputPath), { recursive:true });
await writeFile(outputPath, `${JSON.stringify(fixture, null, 2)}\n`);
console.log(`${outputPath}\n${schemaHash}`);
