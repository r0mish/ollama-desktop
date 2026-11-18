// Sélecteur de modèle : recherche, capacités, taille, modèles déjà en mémoire.
import { api, type Assistant, type Kind, type ModelInfo } from "./api";
import { escapeHtml } from "./markdown";
import { capsHtml, fmtBytes } from "./models-ui";
import { closePopover, repositionPopover, togglePopover } from "./popover";

interface Deps {
  button: HTMLButtonElement;
  models: () => ModelInfo[];
  /** Modèles chargés en mémoire par Ollama. */
  loaded: () => string[];
  setLoaded: (names: string[]) => void;
  selected: () => string;
  kind: () => Kind;
  pick: (model: string) => void;
  manage: () => void;
  /** Assistants proposés en tête du sélecteur principal. */
  assistants: () => Assistant[];
  pickAssistant: (a: Assistant) => void;
  manageAssistants: () => void;
  /** Nom de l'assistant appliqué à la conversation affichée. */
  assistant: () => string | undefined;
}

/** Options d'un menu de modèles ouvert ailleurs que sur le bouton principal (comparaison…). */
export interface ModelMenuOptions {
  selected: string;
  pick: (model: string) => void;
  /** Modèles à ne pas proposer. */
  exclude?: string[];
  title?: string;
}

let deps: Deps;

const fold = (s: string) => s.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();

/** Libellé du bouton : modèle choisi, ou invitation à en installer un. */
export function renderModelButton() {
  const id = deps.selected();
  const known = deps.models().some((m) => m.id === id);
  const label = deps.button.querySelector("span")!;
  const assistant = deps.assistant();
  label.textContent = id ? (assistant ? `${assistant} · ${id}` : id) : "Aucun modèle";
  deps.button.classList.toggle("missing", !!id && !known && deps.models().length > 0);
  deps.button.title = id ? (known || !deps.models().length ? `Modèle : ${id}` : `${id} n'est pas installé sur ce serveur`) : "Choisir un modèle";
}

function rows(query: string): ModelInfo[] {
  const words = fold(query).split(/\s+/).filter(Boolean);
  const loaded = deps.loaded();
  return deps
    .models()
    .filter((m) => {
      const hay = fold(`${m.id} ${m.family ?? ""} ${m.tools ? "outils tools" : ""} ${m.vision ? "vision" : ""} ${m.thinking ? "raisonnement thinking" : ""}`);
      return words.every((w) => hay.includes(w));
    })
    .sort((a, b) => Number(loaded.includes(b.id)) - Number(loaded.includes(a.id)) || a.id.localeCompare(b.id));
}

/** Menu de modèles ancré à `button`. Sans `menu` : le sélecteur principal (assistants, gestion des modèles). */
export function openModelMenu(button: HTMLElement, menu?: ModelMenuOptions) {
  const p = togglePopover(button, "mp");
  if (!p) return;
  const main = !menu;
  const selected = menu ? menu.selected : deps.selected();
  const pick = menu ? menu.pick : deps.pick;
  p.innerHTML = `${menu?.title ? `<div class="mp-section">${escapeHtml(menu.title)}</div>` : ""}
    <input class="mp-search" placeholder="Rechercher un modèle…" spellcheck="false" autocomplete="off" />
    <div class="mp-list" role="listbox"></div>
    ${
      main
        ? `<div class="mp-foot"><button type="button" class="btn-ghost sm" data-manage>Gérer les modèles…</button>
            <button type="button" class="btn-ghost sm" data-assistants>Assistants…</button></div>`
        : ""
    }`;
  const input = p.querySelector<HTMLInputElement>(".mp-search")!;
  const list = p.querySelector<HTMLElement>(".mp-list")!;
  let active = 0;
  let shown: ModelInfo[] = [];

  const render = () => {
    shown = rows(input.value).filter((m) => !menu?.exclude?.includes(m.id));
    active = Math.min(active, Math.max(0, shown.length - 1));
    // Assistants : en tête du sélecteur principal, tant qu'aucune recherche n'est saisie.
    const helpers = main && !input.value.trim() ? deps.assistants() : [];
    const assistantsHtml = helpers.length
      ? `<div class="mp-section">Assistants</div>${helpers
          .map(
            (a) => `<button type="button" class="mp-item${a.name === deps.assistant() ? " selected" : ""}" data-assistant="${escapeHtml(a.id)}">
              <span class="mp-name">${escapeHtml(a.name)}</span><span class="mp-meta">${escapeHtml([a.model || "modèle par défaut", a.description].filter(Boolean).join(" · "))}</span></button>`,
          )
          .join("")}<div class="mp-section">Modèles</div>`
      : "";
    const loaded = deps.loaded();
    const code = deps.kind() === "code";
    list.innerHTML = assistantsHtml + (shown.length
      ? shown
          .map((m, i) => {
            const meta = [m.parameterSize, m.quantization, m.size ? fmtBytes(m.size) : ""].filter(Boolean).join(" · ");
            const noTools = code && !m.tools;
            return `<button type="button" role="option" class="mp-item${i === active ? " active" : ""}${m.id === selected ? " selected" : ""}${noTools ? " dim" : ""}" data-id="${escapeHtml(m.id)}"${
              noTools ? ' title="Ce modèle ne gère pas les appels d\'outils, indispensables à l\'agent de code"' : ""
            }>
              <span class="mp-name">${loaded.includes(m.id) ? '<i class="mp-dot" title="Déjà en mémoire : réponse immédiate"></i>' : ""}${escapeHtml(m.id)}${capsHtml(m)}</span>
              <span class="mp-meta">${escapeHtml(meta)}${noTools ? " · sans outils" : ""}</span>
            </button>`;
          })
          .join("")
      : `<p class="muted small mp-empty">${deps.models().length ? "Aucun modèle ne correspond." : "Aucun modèle installé sur ce serveur."}</p>`);
    list.querySelector(".mp-item.active")?.scrollIntoView({ block: "nearest" });
    repositionPopover();
  };
  const choose = (id: string | undefined) => {
    if (!id) return;
    closePopover();
    pick(id);
  };

  input.addEventListener("input", () => {
    active = 0;
    render();
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      active = (active + (e.key === "ArrowDown" ? 1 : -1) + shown.length) % Math.max(1, shown.length);
      render();
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(shown[active]?.id);
    }
  });
  p.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    if (t.closest("[data-manage]")) {
      closePopover();
      return deps.manage();
    }
    if (t.closest("[data-assistants]")) {
      closePopover();
      return deps.manageAssistants();
    }
    const helper = deps.assistants().find((a) => a.id === t.closest<HTMLElement>("[data-assistant]")?.dataset.assistant);
    if (helper) {
      closePopover();
      return deps.pickAssistant(helper);
    }
    choose(t.closest<HTMLElement>("[data-id]")?.dataset.id);
  });
  render();
  input.focus();
  // État de la mémoire à jour à l'ouverture.
  api
    .runningModels()
    .then((r) => {
      deps.setLoaded(r.map((x) => x.name));
      if (p.isConnected && !p.hidden && p.classList.contains("mp")) render();
    })
    .catch(() => {});
}

export function initModelPicker(d: Deps) {
  deps = d;
  d.button.addEventListener("click", () => openModelMenu(d.button));
  renderModelButton();
}
