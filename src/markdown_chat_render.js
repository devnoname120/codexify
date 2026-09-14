(() => {
  "use strict";
  const parser = window.markdownit({ html:false, breaks:true, linkify:true });
  const tags = new Set(["p", "strong", "em", "s", "blockquote", "ul", "ol", "li", "h1", "h2", "h3", "h4", "h5", "h6", "table", "thead", "tbody", "tr", "th", "td"]);
  function linkKind(href) {
    if (!href || /[\u0000-\u0020\u007f]/.test(href)) return null;
    if (/^https?:\/\//i.test(href) || /^mailto:/i.test(href)) return "external";
    if (href.startsWith("#")) return "anchor";
    if (href.startsWith("sandbox:/mnt/data/") || href.startsWith("codexify://artifact/")) return "file";
    if (!/^[a-z][a-z\d+.-]*:/i.test(href) && !href.startsWith("/") && !href.includes("\\")) return "file";
    return null;
  }
  parser.validateLink = href => linkKind(href) !== null;
  function link(href, title, actions) {
    const kind = linkKind(href);
    const node = document.createElement(kind ? "a" : "span");
    if (!kind) return node;
    node.setAttribute("href", href);
    if (title) node.title = title;
    if (kind === "file") node.className = "attachment-link";
    else if (kind === "external") { node.target = "_blank"; node.rel = "noopener noreferrer"; }
    node.addEventListener("click", event => {
      event.preventDefault();
      if (node.getAttribute("aria-busy") === "true") return;
      node.setAttribute("aria-busy", "true");
      Promise.resolve().then(() => actions.open(href, kind)).catch(error => actions.error(error))
        .finally(() => node.removeAttribute("aria-busy"));
    });
    return node;
  }
  function renderTokens(tokens, parent, actions) {
    const stack = [parent];
    for (const token of tokens) {
      if (token.hidden) continue;
      const container = stack.at(-1);
      if (token.type === "inline") { renderTokens(token.children || [], container, actions); continue; }
      if (token.type === "image") {
        const node = link(token.attrGet("src"), token.attrGet("title"), actions);
        node.classList.add("image-reference");
        node.textContent = token.content || "View image";
        container.append(node); continue;
      }
      if (token.nesting === -1) { if (stack.length > 1) stack.pop(); continue; }
      if (token.type === "link_open") {
        const node = link(token.attrGet("href"), token.attrGet("title"), actions);
        container.append(node); stack.push(node); continue;
      }
      if (token.nesting === 1 && tags.has(token.tag)) {
        const node = document.createElement(token.tag);
        if (token.tag === "table") {
          const scroller = document.createElement("div"); scroller.className = "table-scroll";
          scroller.tabIndex = 0; scroller.setAttribute("role", "region"); scroller.setAttribute("aria-label", "Scrollable table");
          scroller.append(node); container.append(scroller);
        } else container.append(node);
        const align = token.attrGet("style")?.match(/^text-align:(left|right|center)$/)?.[1];
        if (align) node.style.textAlign = align;
        if (token.tag === "ol" && /^\d+$/.test(token.attrGet("start") || "")) node.start = Number(token.attrGet("start"));
        stack.push(node); continue;
      }
      if (token.type === "fence" || token.type === "code_block") {
        const pre = document.createElement("pre"), code = document.createElement("code");
        code.textContent = token.content; pre.append(code); container.append(pre); continue;
      }
      if (token.type === "code_inline") {
        const code = document.createElement("code"); code.textContent = token.content; container.append(code); continue;
      }
      if (token.type === "softbreak" || token.type === "hardbreak") { container.append(document.createElement("br")); continue; }
      if (token.type === "hr") { container.append(document.createElement("hr")); continue; }
      if (token.content) container.append(document.createTextNode(token.content));
    }
  }
  window.renderCodexifyMarkdown = (container, text, actions) => renderTokens(parser.parse(text, {}), container, actions);
})();
