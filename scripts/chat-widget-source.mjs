import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

export const chatHtml = readFileSync(new URL("../src/markdown_chat_ui.html", import.meta.url), "utf8")
  .replace("/* CODEXIFY_MARKDOWN_LIBRARY */", () => "/*\n" + readFileSync(new URL("../src/vendor/markdown-it.LICENSE", import.meta.url), "utf8") + "*/\n" + readFileSync(new URL("../src/vendor/markdown-it.min.js", import.meta.url), "utf8"))
  .replace("/* CODEXIFY_MARKDOWN_RENDERER */", () => readFileSync(new URL("../src/markdown_chat_render.js", import.meta.url), "utf8"));
const setupHtml = readFileSync(new URL("../src/setup_ui.html", import.meta.url), "utf8");
const section = (text, start, end) => {
  assert(text.includes(start) && text.includes(end));
  return text.split(start)[1].split(end)[0];
};
const style = section(chatHtml, "<style>", "</style>")
  .replaceAll(':root:not([data-theme="light"])', ':host(:not([data-theme="light"]))')
  .replaceAll(':root[data-theme="dark"]', ':host([data-theme="dark"])')
  .replaceAll(":root", ":host")
  .replaceAll("body {", ":host { display:block;");
const body = section(chatHtml, "<body>", "<script>");
const script = section(chatHtml, "<script>", "</script>");
export const setupChatHtml = setupHtml.replace("<script>", () => `<template id="codexify-chat-template"><style>${style}</style>${body}</template>\n<script>${script}</script>\n<script>`);
if (process.env.CODEXIFY_CHAT_PREVIEW_HTML) {
  assert.equal(setupChatHtml, readFileSync(process.env.CODEXIFY_CHAT_PREVIEW_HTML, "utf8"), "Browser fixture must match the Rust-served resource byte for byte");
}
