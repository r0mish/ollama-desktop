import { Marked, type Tokens } from "marked";
import DOMPurify from "dompurify";
import hljs from "highlight.js/lib/common";

const escapeHtml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

const marked = new Marked({
  gfm: true,
  breaks: false,
  renderer: {
    code({ text, lang }: Tokens.Code) {
      const language = (lang || "").trim().split(/\s+/)[0];
      let html: string;
      try {
        html = language && hljs.getLanguage(language)
          ? hljs.highlight(text, { language, ignoreIllegals: true }).value
          : hljs.highlightAuto(text).value;
      } catch {
        html = escapeHtml(text);
      }
      return `<div class="code-block"><div class="code-head"><span>${escapeHtml(language || "code")}</span>` +
        `<button type="button" class="copy-code">Copier</button></div>` +
        `<pre><code class="hljs">${html}</code></pre></div>`;
    },
  },
});

DOMPurify.addHook("afterSanitizeAttributes", (node) => {
  if (node.tagName === "A") node.setAttribute("data-external", "");
});

export function renderMarkdown(src: string): string {
  const html = marked.parse(src, { async: false }) as string;
  return DOMPurify.sanitize(html, {
    FORBID_TAGS: ["style", "form", "input", "iframe", "img"],
    FORBID_ATTR: ["style"],
  });
}

export { escapeHtml };
