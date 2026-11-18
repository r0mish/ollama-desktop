// Panneau d'aperçu : rendu d'un bloc HTML ou SVG écrit par le modèle, contenu d'une pièce
// jointe ou d'un fichier de référence. Rien n'est chargé depuis le réseau, aucun script ne s'exécute.
import DOMPurify from "dompurify";
import { escapeHtml } from "./markdown";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

export type PreviewKind = "html" | "svg" | "text" | "image";

interface Preview {
  title: string;
  kind: PreviewKind;
  content: string;
}

let current: Preview | null = null;
let view: "render" | "source" = "render";
let notify: (msg: string, error?: boolean) => void = () => {};
/** Appelé à l'ouverture, pour fermer les autres panneaux latéraux. */
let onOpen: () => void = () => {};

function render() {
  const p = current;
  if (!p) return;
  const body = $("#preview-body");
  const hasSource = p.kind === "html" || p.kind === "svg";
  $("#preview-tabs").hidden = !hasSource;
  $("#preview-note").hidden = !(p.kind === "html" && view === "render");
  document.querySelectorAll<HTMLElement>("#preview-tabs button").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
  $("#preview-copy").hidden = p.kind === "image";
  if (p.kind === "image") {
    body.innerHTML = `<div class="preview-image"><img alt="" /></div>`;
    body.querySelector("img")!.src = p.content;
  } else if (p.kind === "text" || view === "source") {
    body.innerHTML = `<pre class="preview-text">${escapeHtml(p.content)}</pre>`;
  } else if (p.kind === "svg") {
    body.innerHTML = `<div class="preview-svg">${DOMPurify.sanitize(p.content, { USE_PROFILES: { svg: true, svgFilters: true } })}</div>`;
  } else {
    // Cadre isolé : aucun script, aucun accès à l'application ; la politique de sécurité
    // de l'application bloque aussi toute ressource distante (images, polices, feuilles de style).
    body.innerHTML = `<iframe class="preview-frame" sandbox="" referrerpolicy="no-referrer"></iframe>`;
    body.querySelector("iframe")!.srcdoc = p.content;
  }
}

export function openPreview(p: Preview) {
  current = p;
  view = "render";
  onOpen();
  $("#preview-title").textContent = p.title;
  $("#preview-panel").hidden = false;
  render();
}

export function closePreview() {
  $("#preview-panel").hidden = true;
  $("#preview-body").innerHTML = "";
  current = null;
}

export function initPreview(opts: { toast: (msg: string, error?: boolean) => void; onOpen: () => void }) {
  notify = opts.toast;
  onOpen = opts.onOpen;
  $("#preview-close").addEventListener("click", closePreview);
  $("#preview-tabs").addEventListener("click", (e) => {
    const v = (e.target as HTMLElement).closest<HTMLElement>("[data-view]")?.dataset.view;
    if (!v) return;
    view = v as typeof view;
    render();
  });
  $("#preview-copy").addEventListener("click", async () => {
    if (!current) return;
    await navigator.clipboard.writeText(current.content);
    notify("Copié");
  });
}
