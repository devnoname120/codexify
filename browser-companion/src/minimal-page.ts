import type { BrowserContext, Page } from "patchright";

export type BrowserSimplicity = "off" | "simplified" | "bones";

export function parseBrowserSimplicity(value:unknown):BrowserSimplicity {
  if(value === "off" || value === "simplified" || value === "bones")return value;
  throw new Error("Browser simplicity must be off, simplified, or bones");
}

export function browserSimplicityFromEnvironment():BrowserSimplicity {
  return process.env.CODEXIFY_BROWSER_SIMPLICITY === undefined
    ? process.env.CODEXIFY_BROWSER_MINIMAL_UI === "1" ? "simplified" : "off"
    : parseBrowserSimplicity(process.env.CODEXIFY_BROWSER_SIMPLICITY);
}

// Only presentation chrome is hidden. In particular, never hide the composer,
// Stop, Copy, model/connector menus, permission prompts, or error surfaces:
// the browser agent and the person using this window both depend on them.
export const MINIMAL_CHATGPT_CSS = `
  html { --codex-sidebar-preferred-width: 52px !important; }
  #app-shell-sidebar, *:has(> #app-shell-sidebar) {
    width: 52px !important; min-width: 52px !important;
    max-width: 52px !important; flex: 0 0 52px !important;
  }
  #app-shell-sidebar [class*="ConversationSidebar"],
  [class~="group/home-suggestions"],
  #stage-slideover-sidebar,
  [data-testid="sidebar"],
  [data-testid="history-sidebar"],
  nav[aria-label="Chat history"],
  [data-testid="conversation-starters"],
  [data-testid="prompt-suggestions"],
  [data-testid="composer-suggestions"],
  [data-testid="onboarding-welcome"],
  [data-testid="feedback-button"] {
    display: none !important;
  }

  /* Keep animation events working, but finish decorative motion immediately.
     Never remove transforms: menus and the composer use them for positioning. */
  *, *::before, *::after {
    animation-duration: 0.01ms !important;
    animation-delay: 0ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: 0ms !important;
    transition-delay: 0ms !important;
    scroll-behavior: auto !important;
    backdrop-filter: none !important;
  }
  main :where(pre, code, [data-message-author-role], [data-conversation-role]) {
    box-shadow: none !important;
    text-shadow: none !important;
  }
  /* Deliberately plain, compact presentation in the dedicated harness only. */
  body { font-family: system-ui, sans-serif !important; }
  :where([data-message-author-role], [data-conversation-role], #prompt-textarea) {
    font-family: system-ui, sans-serif !important;
    font-size: 14px !important;
    line-height: 1.45 !important;
  }
  :where([data-turn-key], [data-turn-id-container]) { padding-block: 6px !important; }
  main :where(button, pre, [class*="rounded"]), form:has(#prompt-textarea), form[data-chatgpt-composer] {
    border-radius: 4px !important;
    box-shadow: none !important;
  }
  #codexify-fast-indicator {
    all: initial !important;
    position: fixed !important; bottom: 5px !important; left: 8px !important;
    z-index: 2147483646 !important; pointer-events: none !important;
    padding: 3px 7px !important; border: 1px solid #4ade80 !important;
    border-radius: 3px !important; background: #101713 !important;
    color: #a7f3ba !important; font: 600 10px/1.3 system-ui, sans-serif !important;
    letter-spacing: .08em !important;
  }

  /* Let Chrome skip layout/paint for offscreen history, without removing any
     text, IDs, receipts or tool output from the DOM. Match only OUTER turns,
     and keep the four newest sibling turns fully rendered in both renderers.
     Focused/expanded old turns stay ordinary so their controls can overflow. */
  @supports (content-visibility: auto) {
    :where([data-turn-key], [data-turn-id-container], [data-testid^="conversation-turn-"]):not(:is([data-turn-key], [data-turn-id-container], [data-testid^="conversation-turn-"]) *):nth-last-child(n + 5 of :is([data-turn-key], [data-turn-id-container], [data-testid^="conversation-turn-"])):not(:focus-within):not(:has([aria-expanded="true"], [role="dialog"], [aria-busy="true"], [data-streaming-response-status="streaming"])) {
      content-visibility: auto !important;
      contain-intrinsic-block-size: auto 600px;
    }
  }
`;

// Scope this separately so the same rules can be delivered through the live
// presentation API without changing Simplified or Off. No per-token DOM scans.
export const BONES_SHELL_CSS = `
  html[data-codexify-simplicity="bones"] {
    --codex-sidebar-preferred-width: 0px !important;
  }
  html[data-codexify-simplicity="bones"] *,
  html[data-codexify-simplicity="bones"] *::before,
  html[data-codexify-simplicity="bones"] *::after {
    border-radius: 0 !important; box-shadow: none !important;
    text-shadow: none !important;
  }
  html[data-codexify-simplicity="bones"] :is(aside.app-shell-left-panel, #app-shell-sidebar, *:has(> #app-shell-sidebar)) {
    width: 0px !important; min-width: 0px !important;
    max-width: 0px !important; flex: 0 0 0px !important;
    overflow: visible !important; contain: none !important;
    border: 0 !important; background: transparent !important;
    visibility: hidden !important;
  }
  html[data-codexify-simplicity="bones"] #app-shell-sidebar :is(
    [data-testid="accounts-profile-button"], [data-testid="profile-button"],
    [data-testid="account-menu-button"], [data-account-plan-menu-target="true"],
    [aria-label="Open profile menu"]
  ) {
    visibility: visible !important; position: fixed !important;
    left: 6px !important; bottom: 6px !important; top: auto !important;
    width: 28px !important; height: 28px !important; min-width: 28px !important;
    padding: 0 !important; z-index: 50 !important;
  }
  html[data-codexify-simplicity="bones"] aside.app-shell-left-panel :has(#app-shell-sidebar),
  html[data-codexify-simplicity="bones"] #app-shell-sidebar :has([aria-label="Open profile menu"], [data-testid="accounts-profile-button"], [data-account-plan-menu-target="true"]) {
    overflow: visible !important; contain: none !important;
  }
  html[data-codexify-simplicity="bones"] #app-shell-sidebar :is(
    [data-testid="create-new-chat-button"], [data-testid="new-chat-button"]
  ) {
    visibility: visible !important; position: fixed !important;
    left: 6px !important; top: 6px !important;
    width: 28px !important; height: 28px !important; min-width: 28px !important;
    padding: 0 !important; z-index: 50 !important;
  }
  html[data-codexify-simplicity="bones"] main {
    border: 0 !important; border-radius: 0 !important;
  }
  html[data-codexify-simplicity="bones"] :is([class*="Workspace-"], [class*="MainContentClip-"], [class*="MainContentSurface-"]) {
    margin-inline: 0 !important; padding-inline: 0 !important;
    border: 0 !important; border-radius: 0 !important;
    box-shadow: none !important; width: 100% !important;
  }
  /* The current shell paints its rounded page frame in an empty sibling,
     outside main. Leave live regions/dialogs alone; hide only that backdrop. */
  html[data-codexify-simplicity="bones"] [class*="PageSurface-"]:empty {
    display: none !important;
  }
  html[data-codexify-simplicity="bones"][data-codexify-chat-page="true"] main :is(header, [role="tablist"]):not(:has([data-testid="model-switcher-dropdown-button"], [data-codexify-effort-control="true"], [data-codex-intelligence-trigger="true"])):not(form *):not([role="dialog"] *):not([data-turn-key] *):not([data-turn-id-container] *):not([data-testid^="conversation-turn-"] *) {
    display: none !important;
  }
  html[data-codexify-simplicity="bones"][data-codexify-chat-page="true"] main:not(:has([data-turn-key], [data-turn-id-container], [data-testid^="conversation-turn-"])) :is(h1, h2):not(form *):not([role="dialog"] *):not([role="alert"] *) {
    display: none !important;
  }
  html[data-codexify-simplicity="bones"] :is(
    [data-testid="voice-mode-button"], [data-testid="composer-speech-button"],
    [data-testid="composer-dictation-button"], button[aria-label="Start Voice"],
    button[aria-label="Start voice mode"], button[aria-label="Use voice"],
    button[aria-label="Dictate"], [data-testid="temporary-chat-button"]
  ) { display: none !important; }
  html[data-codexify-simplicity="bones"] #codexify-fast-indicator {
    display: none !important;
  }
`;

// Keep the React tree, messages, receipts, menus and automation controls intact.
export const BONES_CHATGPT_CSS = MINIMAL_CHATGPT_CSS + BONES_SHELL_CSS + `
  [data-testid="assistant-avatar"], [data-testid="user-avatar"],
  [data-testid="composer-disclaimer"], [data-testid="conversation-share-button"],
  [data-testid="upgrade-plan-button"], [data-testid="upsell-banner"],
  [data-testid="chat-feedback-thumbs-up"], [data-testid="chat-feedback-thumbs-down"] {
    display: none !important;
  }
  body, main { background-image: none !important; }
  main :where([data-message-author-role], [data-conversation-role], [data-user-message-bubble]) {
    background: transparent !important; border: 0 !important;
    border-radius: 0 !important; box-shadow: none !important;
    padding: 2px 0 !important;
  }
  /* Removing a filled user bubble must also remove its inverted text color. */
  html[data-codexify-simplicity="bones"] main [data-user-message-bubble] {
    color: var(--text-primary, inherit) !important;
  }
  main :where(.markdown, [data-markdown-text-style], [data-message-author-role], [data-conversation-role]) {
    max-width: none !important; text-shadow: none !important;
  }
  main :where(.markdown p, .markdown ul, .markdown ol) { margin-block: 4px !important; }
  main :where(pre, blockquote) { border-radius: 0 !important; box-shadow: none !important; }
  :where([data-turn-key], [data-turn-id-container]) { padding-block: 3px !important; }
  form:has(#prompt-textarea), form[data-chatgpt-composer] {
    border-radius: 0 !important; box-shadow: none !important;
    background-image: none !important;
  }
`;

export function browserSimplicityCss(level:BrowserSimplicity):string {
  return level === "off" ? "" : level === "bones" ? BONES_CHATGPT_CSS : MINIMAL_CHATGPT_CSS;
}

// Install before ChatGPT's scripts on each navigation, including full reloads.
// This affects Codexify's dedicated Chrome context, not the user's own browser.
const minimalStyleArguments = { css: MINIMAL_CHATGPT_CSS, styleId: "codexify-minimal-chatgpt",revision:0,level:"simplified" as BrowserSimplicity };

function attachMinimalStyle({ css, styleId, revision, level }: typeof minimalStyleArguments): void {
    if (location.origin !== "https://chatgpt.com") return;
    const scope=globalThis as typeof globalThis & {__CODEXIFY_FAST_UI__?:{revision:number;refresh:(nextCss:string,nextLevel:BrowserSimplicity)=>void}};
    if(scope.__CODEXIFY_FAST_UI__){
      if(revision>=scope.__CODEXIFY_FAST_UI__.revision){scope.__CODEXIFY_FAST_UI__.revision=revision;scope.__CODEXIFY_FAST_UI__.refresh(css,level);}
      return;
    }
    // Full-page settings/plugin details also use MAIN headers and headings.
    // Restrict chat chrome removal to known chat routes; unknown routes retain
    // their controls. Navigation events cover SPA pushState without patching
    // history or inspecting streaming DOM mutations.
    const updateChatRoute=()=>{
      const path=location.pathname;
      const chat=path==="/"||/^\/new\/?$/.test(path)
        ||/^\/(?:g\/[^/]+\/)?c\/[^/]+\/?$/.test(path)
        ||/^\/g\/g-p-[^/]+\/project\/?$/.test(path);
      if(document.documentElement)document.documentElement.dataset.codexifyChatPage=String(chat);
    };
    (window as Window & {navigation?:EventTarget}).navigation?.addEventListener("currententrychange",updateChatRoute);
    window.addEventListener("popstate",updateChatRoute);
    // React can replace HEAD during hydration/navigation. Adopted styles do
    // not belong to that tree; retain a style-node fallback and repair either
    // one after a document-shell replacement without observing streaming text.
    let sheet:CSSStyleSheet|undefined;
    try {sheet=new CSSStyleSheet();sheet.replaceSync(css);}catch{}
    let head:HTMLHeadElement|null=null,body:HTMLElement|null=null,root:HTMLElement|null=null;
    const headObserver=new MutationObserver(()=>schedule());
    const bodyObserver=new MutationObserver(()=>schedule());
    let queued=false;
    const schedule=()=>{
      if(queued)return;queued=true;
      queueMicrotask(()=>{queued=false;install();});
    };
    const install = () => {
      // Init scripts and Patchright evaluations may live in different JS
      // worlds. Coordinate through the shared document, not globalThis alone.
      // Otherwise old worlds retain adopted sheets and can reclaim the page
      // after hydration, even when a live update reports success.
      const currentRevision=scope.__CODEXIFY_FAST_UI__?.revision??revision;
      const documentRevision=Number(document.documentElement?.dataset.codexifyStyleRevision??-1);
      if(documentRevision>currentRevision)return;
      updateChatRoute();
      if(document.documentElement)document.documentElement.dataset.codexifyStyleRevision=String(currentRevision);
      if(document.documentElement&&root!==document.documentElement){root=document.documentElement;rootObserver.observe(root,{childList:true});}
      if(document.documentElement){
        document.documentElement.dataset.codexifyUi=level === "off" ? "off" : "fast-v2";
        document.documentElement.dataset.codexifySimplicity=level;
      }
      if(sheet || level === "off"){
        const retained=document.adoptedStyleSheets.filter(candidate=>{
          if(candidate===sheet)return false;
          try{return ![...candidate.cssRules].some(rule=>"selectorText" in rule&&rule.selectorText==="#codexify-fast-indicator");}
          catch{return true;}
        });
        if(level === "off")document.adoptedStyleSheets=retained;
        else if(sheet && (retained.length!==document.adoptedStyleSheets.length-1||document.adoptedStyleSheets.at(-1)!==sheet))
          document.adoptedStyleSheets=[...retained,sheet];
      }
      if(level === "off"){
        document.getElementById(styleId)?.remove();
        document.getElementById("codexify-fast-indicator")?.remove();
        headObserver.disconnect();bodyObserver.disconnect();rootObserver.disconnect();
        head=null;body=null;root=null;
        return;
      }
      rootObserver.observe(document,{childList:true});
      if(document.head){
        if(head!==document.head){head=document.head;headObserver.disconnect();headObserver.observe(head,{childList:true});}
        const existing=document.getElementById(styleId);
        if(existing){if(existing.textContent!==css)existing.textContent=css;}
        else {const style=document.createElement("style");style.id=styleId;style.textContent=css;head.appendChild(style);}
      }
      if(document.body){
        if(body!==document.body){body=document.body;bodyObserver.disconnect();bodyObserver.observe(body,{childList:true});}
        if(!document.getElementById("codexify-fast-indicator")){
          const badge=document.createElement("div");badge.id="codexify-fast-indicator";
          badge.setAttribute("aria-hidden","true");
          body.appendChild(badge);
        }
        document.getElementById("codexify-fast-indicator")!.textContent=`CODEXIFY · ${level === "bones" ? "BONES" : "SIMPLIFIED"}`;
      }
    };
    scope.__CODEXIFY_FAST_UI__={revision,refresh:(nextCss,nextLevel)=>{css=nextCss;level=nextLevel;sheet?.replaceSync(css);install();}};
    const rootObserver=new MutationObserver(()=>schedule());
    // Root observers watch only shell children, never every token/tool update.
    rootObserver.observe(document,{childList:true});
    document.addEventListener("DOMContentLoaded",install,{once:true});
    install();
}

export async function installMinimalChatGptPage(context: BrowserContext,extraCss="",revision=0,level:BrowserSimplicity="simplified"): Promise<void> {
  await context.addInitScript(attachMinimalStyle, {...minimalStyleArguments,css:level === "off" ? "" : browserSimplicityCss(level)+"\n"+extraCss,revision,level});
}

// An already-open tab does not receive newly registered init scripts until
// it navigates. Preserve its conversation by styling it in place.
export async function styleRestoredChatGptPage(page: Page,extraCss="",revision=0,level:BrowserSimplicity="simplified"): Promise<void> {
  await page.evaluate(attachMinimalStyle, {...minimalStyleArguments,css:level === "off" ? "" : browserSimplicityCss(level)+"\n"+extraCss,revision,level});
}
