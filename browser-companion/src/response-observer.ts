// Derived from miuuyy/codex-chatgpt-web
// src/adapters/chatgpt-web/browser-worker.ts at f9ad4ae83a579287105ad822dd0c3e0029b04ef6.
// The DOM projection and append-only boundaries intentionally remain close to upstream.
import type { Locator } from "patchright";
import { CHATGPT_COMPLETION_ACTION_SELECTOR } from "./session";
import type { ChatGptMarkdownSegment } from "./response-stream";
import { CHATGPT_STOPPED_THINKING_LABELS } from "./ui-labels";
import {matchesUiLabel,uiLabels,escapeUiPattern} from "./ui-localization";
import { BrowserCompanionError } from "./errors";
import {observeReadOnly} from "./observation";

const CHATGPT_DOM_REVISION_ATTRIBUTES = [
  "aria-hidden", "aria-label", "aria-busy", "aria-disabled", "aria-expanded", "class",
  "data-item-anchor", "data-is-last-node", "data-message-author-role", "data-state",
  "data-streaming-response-status", "data-testid", "data-turn", "data-turn-id",
  "data-turn-id-container", "data-turn-key", "data-conversation-role",
  "data-chatgpt-agent-turn-start", "data-content-search-unit-key", "data-user-message-bubble",
  "data-markdown-text-style", "data-markdown-text-tone", "disabled", "hidden", "inert",
  "open", "role", "start", "style",
] as const;

export interface ChatGptVisibleTraceBlock {
  kind: "answer" | "commentary" | "status";
  text: string;
  key?: string;
  complete?: boolean;
  uiControl?: boolean;
}

export interface ChatGptVisibleTraceEvent {
  kind: "reasoning" | "commentary";
  text: string;
  continuation?: boolean;
}

export interface ChatGptResponseDomSnapshot {
  responsePresent: boolean;
  visibleText: string;
  fullHtml: string;
  markdownSegments: ChatGptMarkdownSegment[];
  completionActionVisible: boolean;
  stoppedThinkingVisible: boolean;
  traceBlocks: ChatGptVisibleTraceBlock[];
}

export interface ChatGptResponseDomCache {
  key?: string;
  snapshot?: ChatGptResponseDomSnapshot;
  fullScans?: number;
  cacheHits?: number;
}

const absentResponseDomSnapshot = (): ChatGptResponseDomSnapshot => ({
  responsePresent:false,
  visibleText:"",
  fullHtml:"",
  markdownSegments:[],
  completionActionVisible:false,
  stoppedThinkingVisible:false,
  traceBlocks:[],
});

export class ChatGptVisibleTraceTracker {
  private readonly emittedTrace = new Map<string, string>();
  private readonly traceCandidates = new Map<string, { text:string; changedAt:number }>();

  constructor(private readonly traceStabilityMs = 250) {}

  observe(blocks: ChatGptVisibleTraceBlock[], completionActionVisible: boolean, now = Date.now()): ChatGptVisibleTraceEvent[] {
    const output: ChatGptVisibleTraceEvent[] = [];
    let statusSlot = 0;
    let commentarySlot = 0;
    for (const block of blocks) {
      if (block.kind === "answer") continue;
      const index = block.kind === "status" ? statusSlot++ : commentarySlot++;
      const slot = block.key ? `${block.kind}:${block.key}` : `${block.kind}:${index}`;
      const stripped = block.text.replace(/\r\n/g, "\n").split("\n")
        .map(line => line.replace(/[\t ]+/g, " ").trim()).join("\n")
        .replace(/\n{3,}/g, "\n\n").trim();
      const text = block.kind === "status" ? stripped.replace(/\s+/g, " ") : stripped;
      if (!text) continue;
      let candidate = this.traceCandidates.get(slot);
      if (!candidate || candidate.text !== text) {
        candidate = { text, changedAt:now };
        this.traceCandidates.set(slot, candidate);
        if (!completionActionVisible && this.traceStabilityMs > 0) continue;
      }
      if (block.kind === "commentary" && block.complete === false && !completionActionVisible) continue;
      if (!completionActionVisible && now - candidate.changedAt < this.traceStabilityMs) continue;
      const previous = this.emittedTrace.get(slot);
      if (previous === text) continue;
      this.emittedTrace.set(slot, text);
      const kind = block.kind === "commentary" ? "commentary" : "reasoning";
      if (previous && text.startsWith(previous)) output.push({ kind, text:text.slice(previous.length), continuation:true });
      else output.push({ kind, text });
    }
    return output;
  }
}

export function isChatGptTraceControl(block: ChatGptVisibleTraceBlock): boolean {
  if (block.kind !== "status") return false;
  const text = block.text.replace(/\s+/g, " ").trim();
  return block.uiControl === true || matchesUiLabel(text,"answerNow") || matchesUiLabel(text,"thinkingStatus");
}

export function stripChatGptTraceControlSuffix(block: ChatGptVisibleTraceBlock): ChatGptVisibleTraceBlock {
  if (block.kind !== "status") return block;
  const text = block.text.replace(new RegExp(`(?:^|\\s)(?:${uiLabels("answerNow").map(escapeUiPattern).join("|")})\\s*$`,"u"), "").trimEnd();
  return text === block.text ? block : { ...block, text };
}

function chatGptBrowserTabClosedError(): BrowserCompanionError {
  return new BrowserCompanionError("The shared ChatGPT window was closed", "external_interference");
}

export async function responseDomSnapshot(
  responseTurn: Locator,
  cache?: ChatGptResponseDomCache,
  signal?: AbortSignal,
): Promise<ChatGptResponseDomSnapshot> {
  signal?.throwIfAborted();
  const observed = await observeReadOnly(responseTurn.evaluate((element, options) => {
    const root = element as HTMLElement;
    type ObserverState = {
      id: number;
      revision: number;
      observer: MutationObserver;
      rendered: Map<HTMLElement, boolean>;
    };
    type ObserverRegistry = {
      documentId: string;
      nextId: number;
      nextNodeId: number;
      nodeIds: WeakMap<Node, number>;
      states: WeakMap<Element, ObserverState>;
    };
    const scope = globalThis as typeof globalThis & {
      __CODEX_WEB_GPT_RESPONSE_OBSERVERS__?: ObserverRegistry;
    };
    const registry = scope.__CODEX_WEB_GPT_RESPONSE_OBSERVERS__ ??= {
      documentId: `${performance.timeOrigin}:${Math.random().toString(36).slice(2)}`,
      nextId: 0,
      nextNodeId: 0,
      nodeIds: new WeakMap(),
      states: new WeakMap<Element, ObserverState>(),
    };
    let observerState = registry.states.get(root);
    if (!observerState) {
      observerState = {
        id: ++registry.nextId,
        revision: 0,
        observer: undefined as unknown as MutationObserver,
        rendered: new Map<HTMLElement, boolean>(),
      };
      const state = observerState;
      state.observer = new MutationObserver(() => {
        state.revision += 1;
      });
      state.observer.observe(root, {
        subtree: true,
        childList: true,
        characterData: true,
        attributes: true,
        attributeFilter: options.attributeFilter,
      });
      registry.states.set(root, state);
    }
    // A browser turn can remain readable while Chrome is minimized or its content has no
    // measured width. Layout geometry is therefore not response visibility:
    // completed Markdown can have width=0 while remaining connected, rendered and readable.
    // One snapshot is synchronous. Share ancestor visibility checks within
    // that snapshot instead of repeatedly resolving the same styles for each
    // paragraph, reference and toolbar in a long answer.
    const renderedAncestors = new Map<HTMLElement, boolean>();
    const isRendered = (candidate: HTMLElement): boolean => {
      if (!candidate.isConnected) return false;
      const chain: HTMLElement[] = [];
      let rendered = true;
      for (let node: HTMLElement | null = candidate; node; node = node.parentElement) {
        const known = renderedAncestors.get(node);
        if (known !== undefined) { rendered = known; break; }
        const style = getComputedStyle(node);
        chain.push(node);
        if (node.hidden || style.display === "none" || style.visibility === "hidden" || style.opacity === "0") {
          rendered = false;
          break;
        }
      }
      for (const node of chain) renderedAncestors.set(node, rendered);
      return rendered;
    };
    // CSS animations and stylesheet changes can reveal an answer or its completion controls
    // without mutating this subtree. Recheck the rendering dependencies of the cached scan;
    // unchanged text/HTML still avoids the expensive serialization below.
    for (const [candidate, rendered] of observerState.rendered) {
      if (isRendered(candidate) !== rendered) {
        observerState.revision += 1;
        break;
      }
    }
    const observerKey = `${registry.documentId}:${observerState.id}:${observerState.revision}`;
    if (options.knownKey === observerKey) return { key: observerKey };
    observerState.rendered.clear();
    const renderedInDom = (candidate: HTMLElement): boolean => {
      const rendered = isRendered(candidate);
      observerState.rendered.set(candidate, rendered);
      return rendered;
    };

    // ChatGPT's DIL renderer has no .markdown class (#538). Read its response root within the
    // assistant-owned PUIK container; the CSS module hash is build-specific. Both renderers
    // feed the same content serializer and completion checks below, without reading UI text.
    const answerRootSelector = '.markdown, [data-message-author-role="assistant"] .puik-root.not-markdown > [class*="_DilResponseRoot"], [data-markdown-text-style="assistant-message"]';
    // In the Activity renderer, the agent-start marker owns the progress block
    // before an assistant search unit exists. Final answers have their own unit.
    const activityContainers = [...root.querySelectorAll<HTMLElement>("[data-chatgpt-agent-turn-start]")]
      .map(marker => marker.parentElement!);
    // ChatGPT uses the same content renderer for intermediate commentary and for the final
    // answer. Older responses nested commentary in the streaming-status container. Pro can also
    // render a completed commentary Markdown root immediately before that live status container.
    // Final-answer Markdown follows the live status instead, so DOM order remains the semantic
    // boundary without relying on localized labels such as "Pro thinking".
    const allMarkdownRoots = [...root.querySelectorAll<HTMLElement>(answerRootSelector)]
      .filter(candidate => {
        if (!root.hasAttribute("data-turn-key") && !candidate.hasAttribute("data-markdown-text-style")) return true;
        const unit = candidate.closest("[data-content-search-unit-key]");
        return unit ? Array.from(unit.children)
          .some(child => child.getAttribute("data-conversation-role") === "assistant")
          : activityContainers.some(container => container.contains(candidate));
      })
      .filter(candidate => !candidate.parentElement?.closest(answerRootSelector))
      .filter(renderedInDom);
    const streamingStatusContainers = [...root.querySelectorAll<HTMLElement>("[data-streaming-response-status]")]
      .filter(renderedInDom);
    // Captured Activity uses the same Markdown component for public action summaries
    // and assistant commentary, including summaries outside activity-header rows.
    // Its explicit tone distinguishes these within the agent's progress section;
    // a final-answer search unit remains an answer regardless of its text tone.
    const activitySummaryRoots = new Set(allMarkdownRoots.filter(candidate => (
      candidate.getAttribute("data-markdown-text-tone") === "tertiary"
      && !candidate.closest("[data-content-search-unit-key]")
      && activityContainers.some(container => container.contains(candidate))
    )));
    // CHATGPT_COMMENTARY_CLASSIFIER_BEGIN
    // Self-contained so the test suite can execute this exact source against a synthetic DOM;
    // it must not close over anything from the surrounding evaluate scope.
    const selectChatGptAnswerRoots = (
      markdownRoots: HTMLElement[],
      statusContainers: HTMLElement[],
      activityContainers: HTMLElement[] = [],
    ): { commentaryRoots: HTMLElement[]; answerRoots: HTMLElement[] } => {
      const firstStatusContainer = statusContainers[0];
      const commentary = markdownRoots.filter(candidate => (
        (!candidate.closest("[data-content-search-unit-key]")
          && activityContainers.some(container => container.contains(candidate)))
        || candidate.closest("[data-streaming-response-status]") !== null
        // Chain-of-thought components carry reasoning, never the final answer, so containment is
        // a position-independent commentary signal. Position alone cannot separate "commentary
        // between two status containers" from "answer between two tool calls".
        || candidate.closest('[data-testid^="cot-v5"]') !== null
        // Only Markdown that precedes the FIRST status container is prior commentary. Keying
        // this on "some status follows me" silently reclassified answer text as commentary as
        // soon as a second tool call opened another status container below it, which both zeroed
        // the visible text and dropped every answer chunk emitted between tool calls.
        || (firstStatusContainer !== undefined && Boolean(
          // 4 is Node.DOCUMENT_POSITION_FOLLOWING, inlined to keep this function standalone.
          candidate.compareDocumentPosition(firstStatusContainer) & 4,
        ))
      ));
      return {
        commentaryRoots: commentary,
        answerRoots: markdownRoots.filter(candidate => !commentary.includes(candidate)),
      };
    };
    // CHATGPT_COMMENTARY_CLASSIFIER_END
    const classified = selectChatGptAnswerRoots(
      allMarkdownRoots.filter(candidate => !activitySummaryRoots.has(candidate)),
      streamingStatusContainers,
      activityContainers,
    );
    const commentaryRoots = classified.commentaryRoots;
    const renderedRoots = classified.answerRoots;
    // CHATGPT_MARKDOWN_CONTENT_BEGIN
    const chatGptMarkdownContent = (
      markdownRoot: HTMLElement,
      onClone?: (original: HTMLElement, clone: HTMLElement) => void,
    ): HTMLElement => {
      const content = markdownRoot.cloneNode(true) as HTMLElement;
      onClone?.(markdownRoot, content);
      // A formula's visual and accessibility layers hydrate independently. Compare
      // its TeX source, just as Markdown conversion does, so cosmetic changes cannot
      // look like an edit to text already delivered to Codex.
      for (const math of Array.from(content.querySelectorAll<HTMLElement>(".katex"))) {
        if (math.closest("pre, code")) continue;
        const sources = math.querySelectorAll('annotation[encoding="application/x-tex"]');
        if (sources.length !== 1) continue;
        const source = content.ownerDocument.createElement("annotation");
        source.setAttribute("encoding", "application/x-tex");
        source.textContent = sources[0]!.textContent;
        math.textContent = "";
        math.appendChild(source);
      }
      // Writing cards expose a copy-content boundary separate from their title,
      // format picker and other changing controls. Keep only that owned content.
      const writingCard = '[data-markdown-copy="rich-block"]';
      const cards = [...(content.matches(writingCard) ? [content] : []),
        ...Array.from(content.querySelectorAll<HTMLElement>(writingCard))];
      for (const card of cards.reverse()) {
        const bodies = Array.from(card.querySelectorAll('[data-markdown-copy-content="true"]'))
          .filter(body => body.closest(writingCard) === card);
        if (bodies.length !== 1) continue;
        const children = Array.from(bodies[0]!.childNodes);
        card.textContent = "";
        for (const child of children) card.appendChild(child);
      }
      // #769 captured a resource preview changing its title to filename/type after
      // surrounding prose was already delivered. This is a presentation control,
      // not answer text. Preserve cards that expose an actual link destination.
      const referenceSelector = '[data-chatgpt-copy-reference][data-markdown-copy="contents"]';
      for (const card of [content, ...Array.from(content.querySelectorAll(referenceSelector))]
        .filter(node => node.matches(referenceSelector) && !node.closest("pre, code"))) {
        if (card.querySelector('[class~="group/resource-row"]') && !card.querySelector("a[href]")) {
          // Keep the reference boundary even when its current view has no answer
          // content: a download link may replace this preview on a later frame.
          card.textContent = "";
        }
      }
      // These are embedded renderers, not Markdown answer text. Their loading labels, controls
      // and plot axes change independently of generation (including after a later paragraph).
      // Keep their UI out of both the emitted HTML and the text consistency fingerprint.
      // Also remove the media already excluded by chatGptHtmlToMarkdown, so their
      // accessibility labels cannot become consistency fingerprints for untransmitted text.
      // Ordinary code blocks, surrounding prose and the original observed DOM remain intact.
      for (const widget of Array.from(content.querySelectorAll(
        ".chart-widget-container, [data-code-block-preview-pane], script, style, svg, img, picture, source",
      ))) widget.remove();
      for (const button of Array.from(content.querySelectorAll("button"))) {
        // Observed file-reference controls have a label but no authoritative download URL.
        // Keep only their text; never carry button attributes or infer a link from the name.
        if (button.matches(".behavior-btn.entity-underline")
          && !button.closest('[hidden], [aria-hidden="true"]')) {
          for (const hidden of Array.from(button.querySelectorAll('[hidden], [aria-hidden="true"], .sr-only, [role="tooltip"]'))) {
            hidden.remove();
          }
          button.replaceWith(content.ownerDocument.createTextNode(button.textContent ?? ""));
        } else {
          button.remove();
        }
      }
      // The new renderer wraps code in DIVs, including a localized toolbar that can
      // disappear on completion. Project only the code into PRE before fingerprinting
      // and Markdown conversion; otherwise the toolbar changes the committed text and
      // Turndown collapses code newlines as if they were ordinary inline whitespace.
      const codeBlockSelector = 'pre, [data-markdown-copy="code-block"]';
      for (const block of Array.from(content.querySelectorAll(codeBlockSelector))) {
        if (block.parentElement?.closest(codeBlockSelector)) continue;
        const codes = block.querySelectorAll("code");
        if (codes.length !== 1) continue;
        const code = codes[0]!.cloneNode(true);
        const pre = block.tagName === "PRE" ? block : content.ownerDocument.createElement("pre");
        block.textContent = "";
        pre.appendChild(code);
        if (pre !== block) block.appendChild(pre);
      }
      return content;
    };
    // ChatGPT may merge adjacent `.markdown` roots or virtualize an earlier prefix while a streamed
    // answer is finalized. Root boundaries and visible indices therefore are not identity:
    // flatten semantic blocks and preserve ChatGPT's source ranges across that reparenting.
    const flattenedMarkdownSegments: Array<{
      nodeKey: string;
      tag: string;
      html: string;
      text: string;
      pendingLinks: boolean;
      mutableReference: boolean;
      linkTargets: string[];
      group?: string;
      sourceStart?: number;
      sourceEnd?: number;
    }> = [];
    const blockMarkdownTags = new Set([
      "address", "article", "aside", "blockquote", "div", "dl", "fieldset", "figcaption",
      "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr",
      "li", "main", "nav", "ol", "p", "pre", "section", "table", "ul",
    ]);
    const markdownText = (element: HTMLElement): string => {
      // Detached content has no layout-derived innerText. Preserve textual line boundaries
      // explicitly: plain textContent would conflate "A<br>B" with "AB" in the guard.
      const parts: string[] = [];
      const blockBoundary = () => {
        if (parts.length > 0 && !parts.at(-1)!.endsWith("\n")) parts.push("\n");
      };
      const visit = (node: Node) => {
        if (node.nodeType === Node.TEXT_NODE) parts.push(node.textContent ?? "");
        if (!(node instanceof HTMLElement)) return;
        const tag = node.tagName.toLowerCase();
        const block = blockMarkdownTags.has(tag);
        if (block) blockBoundary();
        if (tag === "br") parts.push("\n");
        node.childNodes.forEach(visit);
        if (block) blockBoundary();
      };
      visit(element);
      return parts.join("").trim();
    };
    // CHATGPT_MARKDOWN_CONTENT_END
    // A DOM position is not a block identity: a file preview leaving the answer
    // must not give the following paragraph the preview's already committed key.
    const nodeKey = (node: Node): string => {
      let id = registry.nodeIds.get(node);
      if (id === undefined) {
        id = registry.nextNodeId++;
        registry.nodeIds.set(node, id);
      }
      return `dom:${registry.documentId}:${id}`;
    };
    const rememberClone = (original: HTMLElement, clone: HTMLElement): void => {
      const source = document.createTreeWalker(original, NodeFilter.SHOW_ALL);
      const copied = document.createTreeWalker(clone, NodeFilter.SHOW_ALL);
      do {
        nodeKey(source.currentNode);
        registry.nodeIds.set(copied.currentNode, registry.nodeIds.get(source.currentNode)!);
      } while (source.nextNode() && copied.nextNode());
    };
    let listGroupIndex = 0;
    const sourceRange = (candidate: Element): { sourceStart: number; sourceEnd: number } | undefined => {
      const startAttribute = candidate.getAttribute("data-start");
      const endAttribute = candidate.getAttribute("data-end");
      if (startAttribute === null || endAttribute === null) return undefined;
      if (!startAttribute.trim() || !endAttribute.trim()) return undefined;
      const sourceStart = Number(startAttribute);
      const sourceEnd = Number(endAttribute);
      return Number.isFinite(sourceStart) && Number.isFinite(sourceEnd) && sourceEnd >= sourceStart
        ? { sourceStart, sourceEnd }
        : undefined;
    };
    const linkState = (element: HTMLElement): { pendingLinks: boolean; linkTargets: string[]; mutableReference: boolean } => {
      // ChatGPT may paint a link label before supplying its destination. An append-only
      // response cannot add that destination back after committing the label as plain text.
      const anchors = [element, ...element.querySelectorAll<HTMLElement>("a")]
        .filter(candidate => candidate.tagName === "A" && Boolean(candidate.textContent?.trim()));
      return {
        mutableReference: [element, ...element.querySelectorAll('[data-chatgpt-copy-reference][data-markdown-copy="contents"]')]
          .some(node => node.matches('[data-chatgpt-copy-reference][data-markdown-copy="contents"]') && !node.closest("pre, code")),
        pendingLinks: anchors.some(candidate => !candidate.getAttribute("href")?.trim()),
        linkTargets: anchors.flatMap(candidate => {
          const href = candidate.getAttribute("href");
          return href?.trim() ? [href] : [];
        }),
      };
    };
    const appendBlockSegment = (child: HTMLElement) => {
      const tag = child.tagName.toLowerCase();
      const childRange = sourceRange(child);
      const listItems = tag === "ol" || tag === "ul"
        ? [...child.children].filter(candidate => candidate.tagName === "LI") as HTMLElement[]
        : [];
      if (listItems.length === 0) {
        flattenedMarkdownSegments.push({
          nodeKey: nodeKey(child),
          tag,
          html: child.outerHTML,
          text: markdownText(child),
          ...linkState(child),
          ...childRange,
        });
        return;
      }

      const group = childRange
        ? `list:${childRange.sourceStart}:${tag}`
        : `list:${listGroupIndex++}:${tag}`;
      const orderedStart = tag === "ol" ? Number(child.getAttribute("start") ?? "1") : undefined;
      listItems.forEach((item, itemIndex) => {
        const shell = child.cloneNode(false) as HTMLElement;
        shell.removeAttribute("data-is-last-node");
        if (orderedStart !== undefined && Number.isFinite(orderedStart)) {
          shell.setAttribute("start", String(orderedStart + itemIndex));
        }
        shell.append(item.cloneNode(true));
        flattenedMarkdownSegments.push({
          nodeKey: nodeKey(item),
          tag: `${tag}:item`,
          html: shell.outerHTML,
          text: markdownText(item),
          ...linkState(item),
          group,
          ...sourceRange(item),
        });
      });
    };
    const answerContentRoots = renderedRoots.map(root => chatGptMarkdownContent(root, rememberClone));
    answerContentRoots.forEach((markdownRoot) => {
      const children = [...markdownRoot.children] as HTMLElement[];
      const hasBlockChildren = children.some(child => blockMarkdownTags.has(child.tagName.toLowerCase()));
      if (!hasBlockChildren) {
        if (markdownRoot.innerHTML.trim() || linkState(markdownRoot).mutableReference) flattenedMarkdownSegments.push({
          nodeKey: nodeKey(markdownRoot),
          tag: "root",
          html: markdownRoot.innerHTML,
          text: markdownText(markdownRoot),
          ...linkState(markdownRoot),
          ...sourceRange(markdownRoot),
        });
        return;
      }

      let inlineRun: Node[] = [];
      const flushInlineRun = () => {
        if (inlineRun.length === 0) return;
        const nodes = inlineRun;
        inlineRun = [];
        const shell = document.createElement("span");
        nodes.forEach(node => shell.append(node.cloneNode(true)));
        const text = markdownText(shell);
        if (text || linkState(shell).mutableReference) {
          const rangedElements = nodes.flatMap(node => node instanceof Element
            ? [node, ...node.querySelectorAll<HTMLElement>("[data-start][data-end]")]
            : []);
          const ranges = rangedElements
            .map(sourceRange)
            .filter((range): range is { sourceStart: number; sourceEnd: number } => range !== undefined);
          flattenedMarkdownSegments.push({
            nodeKey: nodeKey(nodes[0]!),
            tag: "inline",
            html: shell.outerHTML,
            text,
            ...linkState(shell),
            ...(ranges.length > 0 ? {
              sourceStart: Math.min(...ranges.map(range => range.sourceStart)),
              sourceEnd: Math.max(...ranges.map(range => range.sourceEnd)),
            } : {}),
          });
        }
      };

      markdownRoot.childNodes.forEach((node) => {
        if (node instanceof HTMLElement && blockMarkdownTags.has(node.tagName.toLowerCase())) {
          flushInlineRun();
          appendBlockSegment(node);
          return;
        }
        inlineRun.push(node);
      });
      flushInlineRun();
    });
    const markdownSegments = flattenedMarkdownSegments.map((segment, index, segments) => ({
      // Source offsets may arrive after this exact node has already streamed. Keep its
      // DOM identity as well; the buffer independently uses ranges across later remounts.
      key: `${segment.nodeKey}:${segment.tag}`,
      tag: segment.tag,
      html: segment.html,
      text: segment.text,
      ...(segment.group ? { group: segment.group } : {}),
      ...(segment.sourceStart !== undefined ? { sourceStart: segment.sourceStart } : {}),
      ...(segment.sourceEnd !== undefined ? { sourceEnd: segment.sourceEnd } : {}),
      streamable: index < segments.length - 1 && !segment.pendingLinks,
      ...(segment.mutableReference ? { mutableReference: true } : {}),
      linkTargets: segment.linkTargets,
    }));
    const rendered = renderedRoots.at(-1);
    const completionAction = rendered
      ? [...root.querySelectorAll<HTMLElement>(options.completionActionSelector)]
        .filter(renderedInDom)
        .find(candidate => !rendered.contains(candidate)
          && Boolean(rendered.compareDocumentPosition(candidate) & Node.DOCUMENT_POSITION_FOLLOWING))
      : undefined;
    const completionActionSet = new Set(completionAction ? [completionAction] : []);
    const candidates = new Map<HTMLElement, ChatGptVisibleTraceBlock["kind"]>();
    renderedRoots.forEach(candidate => candidates.set(candidate, "answer"));
    commentaryRoots.forEach(candidate => candidates.set(candidate, "commentary"));
    activitySummaryRoots.forEach(candidate => candidates.set(candidate, "status"));
    const overlapsRenderedAnswer = (candidate: HTMLElement): boolean => renderedRoots.some(rendered => (
      candidate.contains(rendered) || rendered.contains(candidate)
    ));
    const overlapsCommentary = (candidate: HTMLElement): boolean => commentaryRoots.some(commentary => (
      candidate.contains(commentary) || commentary.contains(candidate)
    ));
    const overlapsActivitySummary = (candidate: HTMLElement): boolean => [...activitySummaryRoots].some(summary => (
      candidate.contains(summary) || summary.contains(candidate)
    ));
    const statusSemantic = (candidate: HTMLElement): HTMLElement => {
      // Current cot-v5 action rows expose the semantic text on their item anchor while the
      // discoverable data-testid lives on a textless icon below it. Promote that descendant to
      // the owned row; otherwise every non-button action is silently filtered as empty text.
      return candidate.closest<HTMLElement>("button")
        ?? candidate.closest<HTMLElement>("[data-item-anchor]")
        ?? candidate;
    };
    const traceText = (candidate: HTMLElement): string => {
      const ariaLabel = candidate.getAttribute("aria-label")?.trim();
      if (ariaLabel) return ariaLabel;
      // Animated ChatGPT action counters visually split a phrase around the changing number, so
      // `innerText` can become `Searching websites\n3`. The button's screen-reader label already
      // carries the stable semantic phrase (`Searching 3 websites`) without enclosing unrelated
      // commentary from the surrounding streaming-status container.
      const screenReaderText = [...candidate.querySelectorAll<HTMLElement>(".sr-only")]
        .map(element => element.textContent?.replace(/\s+/g, " ").trim() ?? "")
        .find(Boolean);
      return screenReaderText || candidate.innerText.trim();
    };
    const traceKey = (candidate: HTMLElement, kind: ChatGptVisibleTraceBlock["kind"]): string | undefined => {
      const statusContainer = candidate.closest<HTMLElement>("[data-streaming-response-status]");
      const itemAnchor = candidate.closest<HTMLElement>("[data-item-anchor]");
      if (!statusContainer || !itemAnchor) return undefined;
      const anchorIndex = [...statusContainer.querySelectorAll<HTMLElement>("[data-item-anchor]")]
        .indexOf(itemAnchor);
      return anchorIndex >= 0 ? `${kind}:anchor:${anchorIndex}` : undefined;
    };
    const hasFollowingRenderedSibling = (candidate: HTMLElement): boolean => {
      const itemAnchor = candidate.closest<HTMLElement>("[data-item-anchor]");
      for (
        let sibling = itemAnchor?.nextElementSibling;
        sibling;
        sibling = sibling.nextElementSibling
      ) {
        if (sibling instanceof HTMLElement && renderedInDom(sibling) && sibling.innerText.trim()) {
          return true;
        }
      }
      return false;
    };
    root.querySelectorAll<HTMLElement>(
      'button, [role="status"], [aria-busy="true"], [data-testid*="cot"], [data-testid*="reason"], [data-testid*="thought"]',
    ).forEach(candidate => {
      if (completionActionSet.has(candidate)) return;
      if (overlapsRenderedAnswer(candidate) || overlapsCommentary(candidate)) return;
      const semantic = statusSemantic(candidate);
      // A renderer may wrap the final Markdown in a reason/status container. That wrapper and
      // its descendants still belong exclusively to the final-answer stream; assigning either
      // side to the trace stream duplicates or truncates the answer under Codex's `Working` UI.
      if (!overlapsRenderedAnswer(semantic)
        && !overlapsCommentary(semantic)
        && !overlapsActivitySummary(semantic)
        && !candidates.has(semantic)) {
        candidates.set(semantic, "status");
      }
    });
    root.querySelectorAll<HTMLElement>("[data-streaming-response-status]").forEach(container => {
      if (!overlapsRenderedAnswer(container)
        && !overlapsCommentary(container)
        && ![...candidates.keys()].some(candidate => container.contains(candidate))) {
        candidates.set(container, "status");
      }
    });
    const traceByKey = new Map<string, ChatGptVisibleTraceBlock>();
    [...candidates]
      .filter(([candidate]) => renderedInDom(candidate))
      .sort(([left], [right]) => left === right
        ? 0
        : left.compareDocumentPosition(right) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1)
      .map(([candidate, kind]) => ({
        kind,
        text: traceText(candidate),
        key: traceKey(candidate, kind),
        ...(kind === "commentary" ? { complete: hasFollowingRenderedSibling(candidate) } : {}),
        // Footer controls such as the model picker and overflow menu are siblings of the final
        // Markdown inside the assistant turn. They are UI, not model trace. Real action buttons
        // are scoped by ChatGPT's streaming-status container.
        uiControl: candidate.matches("button")
          && candidate.closest("[data-streaming-response-status]") === null,
      }))
      .filter(block => block.text.length > 0)
      .forEach((block, index) => {
        const key = block.key ?? `${block.kind}:fallback:${index}`;
        const previous = traceByKey.get(key);
        if (!previous || block.text.length > previous.text.length) traceByKey.set(key, block);
      });
    const traceBlocks = [...traceByKey.values()].map((block, index, blocks) => ({
      ...block,
      ...(block.kind === "commentary" ? {
        complete: block.complete === true || index < blocks.length - 1,
      } : {}),
    }));
    const stoppedThinkingVisible = (() => {
      // Only ChatGPT UI in the bound response may terminate the turn. A model quoting this
      // phrase in its answer or reasoning is ordinary content, not a stopped-thinking status.
      // Match the site's observed labels regardless of the account/document language.
      const labels = new Set<string>(options.stoppedThinkingLabels);
      const isStoppedLabel = (value: string | null): boolean => labels.has(value?.replace(/\s+/g, " ").trim() ?? "");
      const isStatus = (candidate: HTMLElement): boolean => {
        if (overlapsRenderedAnswer(candidate) || overlapsCommentary(candidate)
          || candidate.closest("pre, code, blockquote")) return false;
        for (let element: HTMLElement | null = candidate; element; element = element.parentElement) {
          if (!renderedInDom(element)) return false;
        }
        return true;
      };
      const ariaMatch = Array.from(root.querySelectorAll<HTMLElement>("[aria-label]"))
        .some(candidate => isStoppedLabel(candidate.getAttribute("aria-label")) && isStatus(candidate));
      if (ariaMatch) return true;
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        if (!isStoppedLabel(node.textContent)) continue;
        const parent = node.parentElement;
        if (parent && isStatus(parent)) return true;
      }
      return false;
    })();
    return {
      key: observerKey,
      snapshot: {
        responsePresent: true,
        // Completion and tool-progress checks must observe the same answer as delivery.
        // Chart animation, preview loading and copy controls are not new model output.
        visibleText: answerContentRoots.map(markdownText).filter(Boolean).join("\n\n"),
        fullHtml: answerContentRoots.map(candidate => candidate.innerHTML).join(""),
        markdownSegments,
        completionActionVisible: completionAction !== undefined,
        stoppedThinkingVisible,
        traceBlocks,
      },
    };
  }, {
    completionActionSelector: CHATGPT_COMPLETION_ACTION_SELECTOR,
    stoppedThinkingLabels: [...CHATGPT_STOPPED_THINKING_LABELS],
    knownKey: cache?.key,
    attributeFilter: [...CHATGPT_DOM_REVISION_ATTRIBUTES],
  }, { timeout: 2_000 }).catch(() => undefined), signal);
  if (!observed) {
    if (responseTurn.page().isClosed()) {
      throw chatGptBrowserTabClosedError();
    }
    return absentResponseDomSnapshot();
  }
  const snapshot = observed.snapshot ?? cache?.snapshot ?? absentResponseDomSnapshot();
  if (observed.snapshot && cache) {
    cache.key = observed.key;
    cache.snapshot = observed.snapshot;
    cache.fullScans = (cache.fullScans ?? 0) + 1;
  } else if (!observed.snapshot && cache?.snapshot) {
    cache.cacheHits = (cache.cacheHits ?? 0) + 1;
  }
  snapshot.traceBlocks = snapshot.traceBlocks
    .map(stripChatGptTraceControlSuffix)
    .filter(block => block.text.length > 0 && !isChatGptTraceControl(block));
  return snapshot;
}
