// Assistants personnalisés : un modèle, des consignes et des réglages réutilisables.
import { api, type Assistant, type ConvParams, type ModelInfo } from "./api";
import { escapeHtml } from "./markdown";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

interface Deps {
  toast: (msg: string, error?: boolean) => void;
  models: () => ModelInfo[];
  /** Après création, modification ou suppression. */
  changed: () => void;
  /** Utilise l'assistant pour la conversation affichée. */
  use: (a: Assistant) => void;
}

let deps: Deps;
let list: Assistant[] = [];
let current: Assistant | null = null;

const dlg = () => $<HTMLDialogElement>("#assistants-dialog");
const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

export const assistants = () => list;

export async function loadAssistants() {
  list = await api.listAssistants().catch(() => []);
  return list;
}

const f = {
  name: () => $<HTMLInputElement>("#as-name"),
  desc: () => $<HTMLInputElement>("#as-desc"),
  model: () => $<HTMLSelectElement>("#as-model"),
  system: () => $<HTMLTextAreaElement>("#as-system"),
  temp: () => $<HTMLInputElement>("#as-temp"),
  ctx: () => $<HTMLSelectElement>("#as-ctx"),
  think: () => $<HTMLSelectElement>("#as-think"),
};

function renderList() {
  $("#as-list").innerHTML =
    list
      .map(
        (a) => `<button type="button" class="ssh-host${current?.id === a.id ? " active" : ""}" data-id="${escapeHtml(a.id)}">
          <strong>${escapeHtml(a.name)}</strong><small>${escapeHtml(a.model || "modèle par défaut")}</small></button>`,
      )
      .join("") || '<p class="muted small">Aucun assistant.</p>';
}

function select(a: Assistant | null) {
  current = a;
  $("#as-title").textContent = a ? a.name : "Nouvel assistant";
  const ids = deps.models().map((m) => m.id);
  if (a?.model && !ids.includes(a.model)) ids.unshift(a.model);
  f.model().innerHTML =
    '<option value="">Modèle par défaut</option>' + ids.map((id) => `<option value="${escapeHtml(id)}">${escapeHtml(id)}</option>`).join("");
  f.name().value = a?.name ?? "";
  f.desc().value = a?.description ?? "";
  f.model().value = a?.model ?? "";
  f.system().value = a?.systemPrompt ?? "";
  f.temp().value = a?.params?.temperature != null ? String(a.params.temperature) : "";
  f.ctx().value = a?.params?.numCtx != null ? String(a.params.numCtx) : "";
  f.think().value = a?.params?.think == null ? "" : a.params.think ? "on" : "off";
  $("#as-delete").hidden = !a;
  $("#as-use").hidden = !a;
  renderList();
}

async function save(): Promise<Assistant | null> {
  const name = f.name().value.trim();
  if (!name) {
    deps.toast("Donnez un nom à l'assistant.", true);
    return null;
  }
  const params: ConvParams = {};
  const t = f.temp().value.trim() === "" ? NaN : Number(f.temp().value.replace(",", "."));
  if (Number.isFinite(t)) params.temperature = Math.min(2, Math.max(0, t));
  if (f.ctx().value) params.numCtx = Number(f.ctx().value);
  if (f.think().value) params.think = f.think().value === "on";
  const a: Assistant = {
    id: current?.id ?? crypto.randomUUID().slice(0, 8),
    name,
    description: f.desc().value.trim(),
    model: f.model().value,
    systemPrompt: f.system().value.trim(),
    ...(Object.keys(params).length ? { params } : {}),
  };
  const next = [...list.filter((x) => x.id !== a.id), a].sort((x, y) => x.name.localeCompare(y.name));
  try {
    await api.saveAssistants(next);
  } catch (e) {
    deps.toast(errMsg(e), true);
    return null;
  }
  list = next;
  select(a);
  deps.changed();
  return a;
}

export function openAssistantsDialog(id?: string | null) {
  select(id === null ? null : (list.find((a) => a.id === id) ?? list[0] ?? null));
  dlg().showModal();
  f.name().focus();
}

export function initAssistantsUi(d: Deps) {
  deps = d;
  $("#as-close").addEventListener("click", () => dlg().close());
  $("#as-new").addEventListener("click", () => select(null));
  $("#as-list").addEventListener("click", (e) => {
    const id = (e.target as HTMLElement).closest<HTMLElement>("[data-id]")?.dataset.id;
    const a = list.find((x) => x.id === id);
    if (a) select(a);
  });
  $("#as-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (await save()) deps.toast("Assistant enregistré");
  });
  $("#as-use").addEventListener("click", async () => {
    const a = await save();
    if (!a) return;
    dlg().close();
    deps.use(a);
  });
  $("#as-delete").addEventListener("click", async (e) => {
    const btn = e.currentTarget as HTMLElement;
    if (!current) return;
    if (!btn.classList.contains("confirm")) {
      btn.classList.add("confirm");
      btn.textContent = "Confirmer la suppression";
      setTimeout(() => {
        btn.classList.remove("confirm");
        btn.textContent = "Supprimer";
      }, 3000);
      return;
    }
    const next = list.filter((x) => x.id !== current!.id);
    await api.saveAssistants(next).catch((err) => deps.toast(errMsg(err), true));
    list = next;
    btn.classList.remove("confirm");
    btn.textContent = "Supprimer";
    select(list[0] ?? null);
    deps.changed();
  });
}
