// markdown.js — (T2) markdown-lite renderer for assistant chat messages.
//
// Renders a small, safe markdown subset into a DocumentFragment using ONLY
// document.createElement / textContent — never innerHTML — so model output is
// inert by construction (CSP is `script-src 'self'` and no markup the model
// emits can become an element we did not create here).
//
// Supported:
//   fenced code blocks (``` with optional language label)
//   inline `code`, **bold**, *italic*
//   links [text](url) — http/https only; any other scheme is neutralized to
//     plain text. There is no external-link opener in the app, so links render
//     as non-navigating spans with the URL on the title attribute.
//   headings (# .. ###), unordered/ordered lists (2-space indent nesting),
//   paragraphs, blockquotes

const FENCE_RE = /^```([^\s`]*)\s*$/;
const HEADING_RE = /^(#{1,3})\s+(.*)$/;
const QUOTE_RE = /^>\s?(.*)$/;
const LIST_RE = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
// Inline tokens, checked in order: [link](url), **bold**, *italic*. Code spans
// are split out first so no formatting is applied inside backticks.
const INLINE_RE = /\[([^\]]+)\]\(([^)\s]+)\)|\*\*([^*]+)\*\*|\*([^*]+)\*/g;
const CODE_SPAN_RE = /`([^`]+)`/g;
const SAFE_LINK_RE = /^https?:\/\//i;

/** Append inline-formatted content (code/bold/italic/links) to `container`. */
function appendInline(container, text) {
  let codeCursor = 0;
  CODE_SPAN_RE.lastIndex = 0;
  let codeMatch;
  while ((codeMatch = CODE_SPAN_RE.exec(text)) !== null) {
    appendRichText(container, text.slice(codeCursor, codeMatch.index));
    const code = document.createElement("code");
    code.className = "md-code";
    code.textContent = codeMatch[1];
    container.append(code);
    codeCursor = codeMatch.index + codeMatch[0].length;
  }
  appendRichText(container, text.slice(codeCursor));
}

/** Append bold/italic/link formatting (everything except code spans). */
function appendRichText(container, text) {
  let cursor = 0;
  INLINE_RE.lastIndex = 0;
  let match;
  while ((match = INLINE_RE.exec(text)) !== null) {
    if (match.index > cursor) {
      container.append(document.createTextNode(text.slice(cursor, match.index)));
    }
    if (match[1] !== undefined) {
      container.append(linkElement(match[1], match[2]));
    } else if (match[3] !== undefined) {
      const strong = document.createElement("strong");
      strong.className = "md-bold";
      strong.textContent = match[3];
      container.append(strong);
    } else if (match[4] !== undefined) {
      const em = document.createElement("em");
      em.className = "md-italic";
      em.textContent = match[4];
      container.append(em);
    }
    cursor = match.index + match[0].length;
  }
  if (cursor < text.length) {
    container.append(document.createTextNode(text.slice(cursor)));
  }
}

/**
 * Build a link element for [label](url). http/https only: any other scheme
 * (javascript:, data:, …) is neutralized to plain text. Links are
 * non-navigating (no external-link opener exists in the app): the URL rides
 * the title attribute.
 */
function linkElement(label, url) {
  if (!SAFE_LINK_RE.test(url)) {
    return document.createTextNode(label);
  }
  const link = document.createElement("span");
  link.className = "md-link";
  link.title = url;
  link.textContent = label;
  return link;
}

/** Render a fenced code block with an optional language label. */
function codeBlockElement(language, codeText) {
  const wrapper = document.createElement("div");
  wrapper.className = "md-codeblock";
  if (language) {
    const label = document.createElement("div");
    label.className = "md-codeblock-lang";
    label.textContent = language;
    wrapper.append(label);
  }
  const pre = document.createElement("pre");
  pre.className = "md-pre";
  const code = document.createElement("code");
  code.textContent = codeText;
  pre.append(code);
  wrapper.append(pre);
  return wrapper;
}

/**
 * Render a run of list items (possibly nested by 2-space indents) into
 * `fragment`. Each item: { depth, ordered, text }.
 */
function appendList(fragment, items) {
  // Stack of open lists: { depth, ordered, element }.
  const stack = [];
  for (const item of items) {
    while (
      stack.length > 0 &&
      (item.depth < stack[stack.length - 1].depth ||
        (item.depth === stack[stack.length - 1].depth &&
          item.ordered !== stack[stack.length - 1].ordered))
    ) {
      stack.pop();
    }
    if (
      stack.length === 0 ||
      item.depth > stack[stack.length - 1].depth
    ) {
      const list = document.createElement(item.ordered ? "ol" : "ul");
      list.className = "md-list";
      const parent = stack[stack.length - 1]?.element;
      if (parent) {
        // Nest under the parent's last item (the one this list belongs to).
        const anchor = parent.lastElementChild ?? parent;
        anchor.append(list);
      } else {
        fragment.append(list);
      }
      stack.push({ depth: item.depth, ordered: item.ordered, element: list });
    }
    const li = document.createElement("li");
    appendInline(li, item.text);
    stack[stack.length - 1].element.append(li);
  }
}

/** Append a paragraph-ish block; consecutive lines become <br> breaks. */
function appendInlineLines(container, lines) {
  lines.forEach((line, index) => {
    if (index > 0) container.append(document.createElement("br"));
    appendInline(container, line);
  });
}

/**
 * Render markdown-lite `text` into a DocumentFragment. Never throws: anything
 * unrecognized degrades to plain paragraph text.
 *
 * @param {string} text
 * @returns {DocumentFragment}
 */
export function renderMarkdown(text) {
  const fragment = document.createDocumentFragment();
  const lines = String(text ?? "").split("\n");
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    // Blank line: block separator.
    if (line.trim() === "") {
      i += 1;
      continue;
    }

    // Fenced code block (an unclosed fence runs to EOF).
    const fence = FENCE_RE.exec(line);
    if (fence) {
      const language = fence[1] || "";
      const codeLines = [];
      i += 1;
      while (i < lines.length && !FENCE_RE.test(lines[i])) {
        codeLines.push(lines[i]);
        i += 1;
      }
      i += 1; // consume the closing fence (or EOF)
      fragment.append(codeBlockElement(language, codeLines.join("\n")));
      continue;
    }

    // Heading (# .. ###).
    const heading = HEADING_RE.exec(line);
    if (heading) {
      const level = heading[1].length;
      const el = document.createElement(`h${level}`);
      el.className = "md-heading";
      appendInline(el, heading[2]);
      fragment.append(el);
      i += 1;
      continue;
    }

    // Blockquote: consecutive "> " lines.
    if (QUOTE_RE.test(line)) {
      const quoteLines = [];
      while (i < lines.length && QUOTE_RE.test(lines[i])) {
        quoteLines.push(QUOTE_RE.exec(lines[i])[1]);
        i += 1;
      }
      const quote = document.createElement("blockquote");
      quote.className = "md-quote";
      appendInlineLines(quote, quoteLines);
      fragment.append(quote);
      continue;
    }

    // List: consecutive list-item lines (2-space indent per nesting level).
    if (LIST_RE.test(line)) {
      const items = [];
      while (i < lines.length && LIST_RE.test(lines[i])) {
        const match = LIST_RE.exec(lines[i]);
        items.push({
          depth: Math.floor(match[1].replace(/\t/g, "  ").length / 2),
          ordered: /^\d/.test(match[2]),
          text: match[3],
        });
        i += 1;
      }
      appendList(fragment, items);
      continue;
    }

    // Paragraph: consecutive lines up to the next blank line or block start.
    const paraLines = [];
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !FENCE_RE.test(lines[i]) &&
      !HEADING_RE.test(lines[i]) &&
      !QUOTE_RE.test(lines[i]) &&
      !LIST_RE.test(lines[i])
    ) {
      paraLines.push(lines[i]);
      i += 1;
    }
    const para = document.createElement("p");
    para.className = "md-para";
    appendInlineLines(para, paraLines);
    fragment.append(para);
  }

  return fragment;
}
