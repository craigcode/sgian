// markdown.test.js — unit tests for the (T2) markdown-lite renderer.
//
// The renderer builds DOM exclusively via createElement/textContent; these
// tests pin both the supported subset and the safety property that model
// output can never become markup (no innerHTML anywhere).

import { describe, it, expect } from "vitest";
import { renderMarkdown } from "../ui/src/markdown.js";

/** Render into a detached container div for querying. */
function render(text) {
  const div = document.createElement("div");
  div.append(renderMarkdown(text));
  return div;
}

describe("renderMarkdown — paragraphs and inline formatting", () => {
  it("renders plain text as a paragraph", () => {
    const div = render("hello world");
    const p = div.querySelector("p.md-para");
    expect(p).not.toBeNull();
    expect(p.textContent).toBe("hello world");
  });

  it("splits blank-line-separated paragraphs", () => {
    const div = render("one\n\ntwo\n\n\nthree");
    const paras = div.querySelectorAll("p.md-para");
    expect(paras).toHaveLength(3);
    expect(paras[0].textContent).toBe("one");
    expect(paras[1].textContent).toBe("two");
    expect(paras[2].textContent).toBe("three");
  });

  it("renders inline code, bold, and italic", () => {
    const div = render("run `npm test` then **commit** and *push*");
    expect(div.querySelector("code.md-code").textContent).toBe("npm test");
    expect(div.querySelector("strong.md-bold").textContent).toBe("commit");
    expect(div.querySelector("em.md-italic").textContent).toBe("push");
  });

  it("does not format inside code spans", () => {
    const div = render("`*not italic*`");
    const code = div.querySelector("code.md-code");
    expect(code.textContent).toBe("*not italic*");
    expect(div.querySelector("em")).toBeNull();
  });

  it("renders consecutive lines in one paragraph with breaks", () => {
    const div = render("first\nsecond");
    const p = div.querySelector("p.md-para");
    expect(p.querySelectorAll("br")).toHaveLength(1);
    expect(p.textContent).toBe("firstsecond");
  });
});

describe("renderMarkdown — code fences", () => {
  it("renders a fenced block with a language label", () => {
    const div = render("```js\nconst x = 1;\n```");
    const block = div.querySelector(".md-codeblock");
    expect(block).not.toBeNull();
    expect(block.querySelector(".md-codeblock-lang").textContent).toBe("js");
    expect(block.querySelector("pre.md-pre code").textContent).toBe("const x = 1;");
  });

  it("renders a fence without a language", () => {
    const div = render("```\nplain\n```");
    expect(div.querySelector(".md-codeblock-lang")).toBeNull();
    expect(div.querySelector("pre.md-pre code").textContent).toBe("plain");
  });

  it("keeps fence contents literal (no inline formatting, no markup)", () => {
    const div = render("```\n**not bold** <b>html</b>\n```");
    const code = div.querySelector("pre.md-pre code");
    expect(code.textContent).toBe("**not bold** <b>html</b>");
    expect(div.querySelector("strong")).toBeNull();
    expect(div.querySelector("b")).toBeNull();
  });

  it("renders an unclosed fence to EOF", () => {
    const div = render("```py\nprint(1)");
    expect(div.querySelector("pre.md-pre code").textContent).toBe("print(1)");
  });
});

describe("renderMarkdown — headings, lists, quotes", () => {
  it("renders # .. ### headings", () => {
    const div = render("# one\n\n## two\n\n### three\n\n#### not a heading");
    const headings = div.querySelectorAll(".md-heading");
    expect(headings).toHaveLength(3);
    expect(headings[0].tagName).toBe("H1");
    expect(headings[1].tagName).toBe("H2");
    expect(headings[2].tagName).toBe("H3");
    expect(headings[2].textContent).toBe("three");
    // Four hashes is not a heading: it stays paragraph text.
    expect(div.querySelector("p.md-para").textContent).toBe("#### not a heading");
  });

  it("renders unordered and ordered lists", () => {
    const div = render("- a\n- b\n\n1. one\n2. two");
    const ul = div.querySelector("ul.md-list");
    expect(ul.querySelectorAll(":scope > li")).toHaveLength(2);
    const ol = div.querySelector("ol.md-list");
    expect(ol.querySelectorAll(":scope > li")).toHaveLength(2);
    expect(ol.querySelector("li").textContent).toBe("one");
  });

  it("nests lists by 2-space indentation", () => {
    const div = render("- outer\n  - inner\n  - inner two\n- outer two");
    const outer = div.querySelector(":scope > ul.md-list");
    expect(outer.querySelectorAll(":scope > li")).toHaveLength(2);
    const nested = outer.querySelector(":scope > li > ul.md-list");
    expect(nested).not.toBeNull();
    expect(nested.querySelectorAll(":scope > li")).toHaveLength(2);
    expect(nested.querySelector("li").textContent).toBe("inner");
  });

  it("applies inline formatting inside list items", () => {
    const div = render("- `code` and **bold**");
    const li = div.querySelector("li");
    expect(li.querySelector("code.md-code").textContent).toBe("code");
    expect(li.querySelector("strong.md-bold").textContent).toBe("bold");
  });

  it("renders blockquotes", () => {
    const div = render("> quoted\n> more\n\nafter");
    const quote = div.querySelector("blockquote.md-quote");
    expect(quote).not.toBeNull();
    expect(quote.textContent).toBe("quotedmore");
    expect(div.querySelector("p.md-para").textContent).toBe("after");
  });
});

describe("renderMarkdown — links", () => {
  it("renders https links as titled non-navigating spans", () => {
    const div = render("see [the docs](https://example.com/docs)");
    const link = div.querySelector(".md-link");
    expect(link).not.toBeNull();
    expect(link.textContent).toBe("the docs");
    expect(link.title).toBe("https://example.com/docs");
    // No external-link opener exists in the app, so nothing navigates.
    expect(div.querySelector("a")).toBeNull();
  });

  it("renders http links too", () => {
    const div = render("[x](http://example.com)");
    expect(div.querySelector(".md-link").title).toBe("http://example.com");
  });

  it("neutralizes javascript: links to plain text", () => {
    const div = render("[click](javascript:alert(1))");
    expect(div.querySelector(".md-link")).toBeNull();
    expect(div.querySelector("a")).toBeNull();
    // The label survives as inert text; the scheme is dropped.
    expect(div.textContent).toContain("click");
    expect(div.textContent).not.toContain("javascript:");
  });
});

describe("renderMarkdown — safety (no markup breakout)", () => {
  it("treats raw HTML/script as inert text", () => {
    const div = render('<script>alert(1)</script> and <img src=x onerror=alert(1)>');
    expect(div.querySelectorAll("script")).toHaveLength(0);
    expect(div.querySelectorAll("img")).toHaveLength(0);
    expect(div.textContent).toContain("<script>alert(1)</script>");
    expect(div.textContent).toContain("<img src=x onerror=alert(1)>");
  });

  it("produces no script elements for adversarial mixed input", () => {
    const div = render(
      "# <svg onload=x>\n\n- [a](javascript:alert(1))\n\n```\n<script>alert(2)</script>\n```",
    );
    expect(div.querySelectorAll("script")).toHaveLength(0);
    expect(div.querySelectorAll("svg")).toHaveLength(0);
    expect(div.querySelectorAll("a")).toHaveLength(0);
  });
});
