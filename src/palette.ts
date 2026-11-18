// Palette de commandes (Ctrl/⌘+K) : actions, conversations, modèles et projets au clavier.
import { escapeHtml } from "./markdown";

export interface PaletteItem {
  group: string;
  label: string;
  /** Texte secondaire (raccourci, détail). */
  hint?: string;
  /** Mots-clés supplémentaires pour la recherche. */
  keywords?: string;
  run: () => void;
}

interface Deps {
  /** Éléments proposés pour une requête (déjà triés par pertinence dans chaque groupe). */
  items: (query: string) => PaletteItem[];
  /** Résultats supplémentaires asynchrones (recherche dans le contenu des conversations). */
  search?: (query: string) => Promise<PaletteItem[]>;
}

let deps: Deps;
let shown: PaletteItem[] = [];
let active = 0;
let seq = 0;

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;
const dlg = () => $<HTMLDialogElement>("#palette");

export const fold = (s: string) => s.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();

/** Tous les mots de la requête figurent dans le texte (sans casse ni accents). */
export function matches(query: string, text: string): boolean {
  const hay = fold(text);
  return fold(query)
    .split(/\s+/)
    .filter(Boolean)
    .every((w) => hay.includes(w));
}

function render() {
  const list = $("#palette-list");
  let html = "";
  let group = "";
  shown.forEach((it, i) => {
    if (it.group !== group) {
      group = it.group;
      html += `<div class="group">${escapeHtml(group)}</div>`;
    }
    html += `<button type="button" class="pal-item${i === active ? " active" : ""}" data-i="${i}"><span>${escapeHtml(it.label)}</span>${
      it.hint ? `<small>${escapeHtml(it.hint)}</small>` : ""
    }</button>`;
  });
  list.innerHTML = html || '<p class="muted small pal-empty">Aucun résultat</p>';
  list.querySelector(".pal-item.active")?.scrollIntoView({ block: "nearest" });
}

function update() {
  const q = $<HTMLInputElement>("#palette-input").value.trim();
  shown = deps.items(q);
  active = 0;
  render();
  const mine = ++seq;
  if (q.length < 2 || !deps.search) return;
  deps
    .search(q)
    .then((extra) => {
      if (mine !== seq || !dlg().open) return;
      const labels = new Set(shown.map((s) => `${s.group}\0${s.label}`));
      shown = [...shown, ...extra.filter((e) => !labels.has(`${e.group}\0${e.label}`))];
      render();
    })
    .catch(() => {});
}

function run(i: number) {
  const it = shown[i];
  if (!it) return;
  dlg().close();
  it.run();
}

export function openPalette() {
  if (dlg().open) return dlg().close();
  $<HTMLInputElement>("#palette-input").value = "";
  dlg().showModal();
  update();
  $<HTMLInputElement>("#palette-input").focus();
}

export function initPalette(d: Deps) {
  deps = d;
  const input = $<HTMLInputElement>("#palette-input");
  input.addEventListener("input", update);
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      active = (active + (e.key === "ArrowDown" ? 1 : -1) + shown.length) % Math.max(1, shown.length);
      render();
    } else if (e.key === "Enter") {
      e.preventDefault();
      run(active);
    }
  });
  $("#palette-list").addEventListener("click", (e) => {
    const i = (e.target as HTMLElement).closest<HTMLElement>("[data-i]")?.dataset.i;
    if (i !== undefined) run(Number(i));
  });
  // Clic sur le fond : fermeture.
  dlg().addEventListener("mousedown", (e) => {
    if (e.target === dlg()) dlg().close();
  });
}
