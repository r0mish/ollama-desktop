import "highlight.js/styles/github-dark.css";
import "./styles.css";
import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  api,
  type ApiMessage,
  type Assistant,
  type ConvParams,
  type Conversation,
  type ConversationSummary,
  type InstructionFile,
  type Kind,
  type McpServerStatus,
  type Message,
  type ModelInfo,
  type PermissionMode,
  type ProjectContext,
  type Project,
  type Risk,
  type RunningModel,
  type Server,
  type ServerStatus,
  type Settings,
  type TextFile,
  type ToolCallRec,
  type ToolContext,
  type UpdateInfo,
} from "./api";
import { COMPACT_PROMPT, INIT_AGENTS_PROMPT, diffHtml, looksUnfinished, needsApproval, systemPrompt, toApiMessages, toolCardHtml, toolLabel, usesSudo } from "./agent";
import { aggregate, contextTokens, conversationUsage, fmtDuration, fmtSpeed, fmtTokens, today } from "./usage";
import { escapeHtml, renderMarkdown } from "./markdown";
import { initSshUi, loadHosts, openSshDialog, parseRemote, workspaceLabel } from "./ssh-ui";
import { initGitUi, scheduleGitRefresh, toggleGitPanel } from "./git-ui";
import { findProject, initProjectsUi, loadProjects, openProjectDialog, projectChars, projectList } from "./projects-ui";
import { cancelDictation, initDictation, toggleDictation } from "./dictation";
import { initModelsUi, openModelsDialog } from "./models-ui";
import { initPopover } from "./popover";
import { commitServerForm, initServersUi, renderServerChip, renderServers, serversDraft } from "./servers-ui";
import { initModelPicker, openModelMenu, renderModelButton } from "./model-picker";
import { assistants, initAssistantsUi, loadAssistants, openAssistantsDialog } from "./assistants-ui";
import { initConvSettings, isCustomized } from "./conv-settings";
import { initPalette, matches, openPalette, type PaletteItem } from "./palette";
import type { Platform } from "./agent";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

const el = {
  app: $("#app"),
  convList: $("#conv-list"),
  search: $<HTMLInputElement>("#search"),
  newChatLabel: $("#new-chat-label"),
  title: $("#conv-title"),
  wsChip: $<HTMLButtonElement>("#ws-chip"),
  permSelect: $<HTMLSelectElement>("#perm-select"),
  modelBtn: $<HTMLButtonElement>("#model-btn"),
  convSettingsBtn: $<HTMLButtonElement>("#conv-settings-btn"),
  exportBtn: $<HTMLButtonElement>("#export-btn"),
  messages: $("#messages"),
  composer: $<HTMLFormElement>("#composer"),
  input: $<HTMLTextAreaElement>("#input"),
  attachments: $("#attachments"),
  fileInput: $<HTMLInputElement>("#file-input"),
  webToggle: $<HTMLButtonElement>("#web-toggle"),
  mcpToggle: $<HTMLButtonElement>("#mcp-toggle"),
  sysToggle: $<HTMLButtonElement>("#sys-toggle"),
  sendBtn: $<HTMLButtonElement>("#send-btn"),
  hint: $("#hint"),
  drop: $("#drop-overlay"),
  toast: $("#toast"),
  settings: $<HTMLDialogElement>("#settings"),
  activity: $("#activity"),
  ctxMeter: $<HTMLButtonElement>("#ctx-meter"),
  usageInfo: $("#usage-info"),
  updateBanner: $("#update-banner"),
  gitBtn: $<HTMLButtonElement>("#git-btn"),
  changesBtn: $<HTMLButtonElement>("#changes-btn"),
  changesPanel: $("#changes-panel"),
  wsPicker: $<HTMLButtonElement>("#ws-picker"),
  wsMenu: $("#ws-menu"),
  instrChip: $<HTMLButtonElement>("#instr-chip"),
  projectSelect: $<HTMLSelectElement>("#project-select"),
  projectEdit: $<HTMLButtonElement>("#project-edit"),
  convProject: $<HTMLSelectElement>("#conv-project"),
};

const MAX_TEXT_FILE = 512 * 1024;
const MAX_PDF = 50 * 1024 * 1024;
const MAX_IMAGE_SIDE = 2048;
const MAX_CHAT_STEPS = 15;
const MAX_AUTO_CONTINUE = 2;
const AUTO_CONTINUE_TEXT = "Continue : exécute directement les actions annoncées avec les outils, sans les décrire.";

/** Ce que fait l'agent en ce moment (barre d'activité). */
interface Activity {
  phase: string;
  detail?: string;
  /** Début de la phase en cours. */
  since: number;
  /** Fin d'attente (nouvel essai), pour le compte à rebours. */
  until?: number;
  runStart: number;
  step: number;
  maxSteps: number;
}

/** Exécution en cours d'une conversation. */
interface Run {
  conv: Conversation;
  requestId: string;
  msgId: string;
  stopped: boolean;
  activity: Activity;
  /** Appels d'outils en attente d'une décision de l'utilisateur (et mot de passe sudo saisi). */
  approvals: Map<string, (d: Decision, sudo?: SudoInput) => void>;
}

type Decision = "allow" | "always" | "deny";
interface SudoInput {
  password: string;
  keep: boolean;
}

/** Bilan du dernier tour de l'agent, affiché tant que rien d'autre ne se passe. */
interface RunOutcome {
  /** Mode plan : le plan proposé peut être exécuté d'un clic. */
  plan?: boolean;
  kind: "done" | "stopped" | "error" | "limit" | "denied" | "truncated";
  text: string;
  canContinue: boolean;
}

/** Brouillon de la prochaine conversation (avant le premier envoi). */
interface Draft {
  workspace?: string;
  webSearch: boolean;
  mcp: boolean;
  permissionMode: PermissionMode;
  systemAccess?: boolean;
  projectId?: string;
  /** Consignes et réglages choisis avant le premier envoi (absent = réglages globaux). */
  systemPrompt?: string | null;
  params?: ConvParams;
  /** Nom de l'assistant appliqué. */
  assistant?: string;
}

const state = {
  settings: null as unknown as Settings,
  platform: { os: "linux", shell: "sh", shellPath: "", shellGuidance: "", version: "" },
  /** Serveur Ollama : version (connecté) ou erreur. */
  server: null as ServerStatus | null,
  models: [] as ModelInfo[],
  modelsLoaded: false,
  /** Modèles chargés en mémoire par Ollama (réponse immédiate). */
  loaded: [] as string[],
  running: [] as RunningModel[],
  /** Dernier modèle choisi à la main, par mode : repris par les nouvelles conversations. */
  lastModel: {} as Partial<Record<Kind, string>>,
  convs: [] as ConversationSummary[],
  workspaces: [] as string[],
  mcp: [] as McpServerStatus[],
  mode: "chat" as Kind,
  current: null as Conversation | null,
  draft: { webSearch: false, mcp: true, permissionMode: "ask" } as Draft,
  /** Exécutions en cours (génération ou outil), par conversation : plusieurs peuvent tourner à la fois. */
  runs: new Map<string, Run>(),
  /** Conversations dont l'exécution s'est terminée en arrière-plan (pastille dans la barre latérale). */
  unread: new Set<string>(),
  pending: { images: [] as string[], files: [] as TextFile[] },
  selectedModel: "",
  risks: new Map<string, Risk>(),
  /** Bilan de la dernière exécution, par conversation. */
  lastRuns: new Map<string, RunOutcome>(),
  update: null as UpdateInfo | null,
  /** Projet affiché dans la barre latérale ("" = toutes les conversations). */
  projectFilter: "",
  /** Fichiers de consignes (AGENTS.md…) par dossier de projet. */
  instructions: new Map<string, InstructionFile[]>(),
  /** Recherche plein texte : requête et résultats (id → extrait). */
  search: { query: "", hits: new Map<string, string | null>() },
  /** Fichiers de référence préparés pour la dernière question, par conversation. */
  projectCtx: new Map<string, ProjectContext>(),
};

const uid = () => crypto.randomUUID();

/** Exécution en cours de la conversation affichée. */
const currentRun = () => (state.current ? state.runs.get(state.current.id) : undefined);
/** La conversation est-elle celle affichée ? (une exécution continue en arrière-plan sinon) */
const viewing = (conv: Conversation) => state.current === conv;
/** Mots de passe sudo retenus pour la session (en mémoire uniquement), par machine cible. */
const sudoCache = new Map<string, string>();
/** Machine sur laquelle s'exécutent les commandes d'une conversation : serveur SSH ou cet ordinateur. */
const sudoTarget = (conv: Conversation) => parseRemote(conv.workspace)?.hostId ?? "local";
/** Conversations supprimées pendant leur exécution : plus aucune sauvegarde. */
const deletedConvs = new Set<string>();

/** Plateforme vue par l'agent : la machine locale, ou le serveur SSH du dossier de projet. */
function platformFor(conv: Conversation): Platform {
  const r = conv.kind === "code" ? parseRemote(conv.workspace) : null;
  if (!r) return state.platform;
  const label = r.host ? `${r.host.label} (${r.host.user ? `${r.host.user}@` : ""}${r.host.host})` : "serveur distant";
  return {
    os: r.host?.system || "Linux/Unix (distant)",
    shell: "bash (ou sh) sur le serveur distant, via SSH",
    shellGuidance: "Use POSIX/bash syntax. Be careful: this may be a production server — never restart services, change system configuration or delete data unless explicitly asked.",
    remote: label,
  };
}

/** Seuil au-delà duquel seuls des extraits sont joints (knowledge::FULL_CONTEXT_CHARS). */
const FULL_PROJECT_CHARS = 60_000;

function projectFilesFor(conv: Conversation): string | undefined {
  const p = findProject(conv.projectId);
  if (!p) return undefined;
  const ctx = state.projectCtx.get(conv.id);
  if (ctx) return ctx.text;
  // Pas encore préparé (estimation du contexte) : un gros projet n'est jamais joint en entier.
  return projectChars(p) > FULL_PROJECT_CHARS ? "(extraits pertinents sélectionnés à chaque question)" : undefined;
}

/** Invite système complète : consignes du dossier (AGENTS.md…) et projet inclus. */
const sysPrompt = (conv: Conversation) =>
  systemPrompt(conv, state.settings, platformFor(conv), {
    instructions: conv.kind === "code" && conv.workspace ? state.instructions.get(conv.workspace) : undefined,
    project: findProject(conv.projectId),
    projectFiles: projectFilesFor(conv),
  });

// ---------------------------------------------------------------- utilitaires

let toastTimer = 0;
function toast(msg: string, isError = false) {
  el.toast.textContent = msg;
  el.toast.className = isError ? "error" : "";
  el.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (el.toast.hidden = true), isError ? 6000 : 2500);
}

const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

function applyTheme(theme: Settings["theme"]) {
  if (theme === "system") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.dataset.theme = theme;
}

async function copy(text: string, btn?: HTMLElement) {
  await navigator.clipboard.writeText(text);
  if (btn) {
    const old = btn.innerHTML;
    btn.textContent = "Copié";
    setTimeout(() => (btn.innerHTML = old), 1200);
  }
}

const icons = {
  copy: '<svg viewBox="0 0 24 24"><rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"/></svg>',
  retry: '<svg viewBox="0 0 24 24"><path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/></svg>',
  edit: '<svg viewBox="0 0 24 24"><path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/></svg>',
  trash: '<svg viewBox="0 0 24 24"><path d="M3 6h18M8 6V4h8v2M6 6l1 14h10l1-14"/></svg>',
  file: '<svg viewBox="0 0 24 24"><path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><path d="M14 3v6h6"/></svg>',
  compare: '<svg viewBox="0 0 24 24"><rect x="3" y="4" width="7.5" height="16" rx="2"/><rect x="13.5" y="4" width="7.5" height="16" rx="2"/></svg>',
  folder: '<svg viewBox="0 0 24 24"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>',
  server: '<svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="7" rx="2"/><rect x="3" y="13" width="18" height="7" rx="2"/><path d="M7 7.5h.01M7 16.5h.01"/></svg>',
};

// ---------------------------------------------------------------- modèles

const connected = () => !!state.server?.version;

function modelIds(): string[] {
  const ids = state.models.map((m) => m.id);
  for (const extra of [state.current?.model, state.selectedModel]) {
    if (extra && !ids.includes(extra)) ids.unshift(extra);
  }
  return ids;
}

/** `auto` : libellé d'une option « automatique » (valeur vide), pour les réglages. */
function fillModelSelect(select: HTMLSelectElement, value: string, auto?: string) {
  const ids = modelIds();
  if (value && !ids.includes(value)) ids.unshift(value);
  const label = (id: string) => (state.models.some((m) => m.id === id) || !state.modelsLoaded ? id : `${id} (non installé)`);
  select.innerHTML =
    (auto !== undefined ? `<option value="">${escapeHtml(auto)}</option>` : "") +
    (ids.length ? "" : auto === undefined ? '<option value="">Aucun modèle installé</option>' : "") +
    ids.map((id) => `<option value="${escapeHtml(id)}">${escapeHtml(label(id))}</option>`).join("");
  select.value = value;
}

async function loadModels() {
  const srv = state.settings.servers.find((v) => v.id === state.settings.activeServer) ?? state.settings.servers[0];
  state.server = await api
    .serverStatus()
    .catch((e) => ({ id: srv?.id ?? "", name: srv?.name ?? "Ollama", url: srv?.url ?? "", tunnel: null, version: null, error: errMsg(e), insecure: false }));
  if (connected()) {
    try {
      state.models = await api.listModels();
      state.modelsLoaded = true;
    } catch (e) {
      toast(`Liste des modèles indisponible : ${errMsg(e)}`, true);
    }
    await refreshRunning();
  } else {
    state.running = [];
    state.loaded = [];
  }
  renderServerChip();
  if (!state.current && (!state.selectedModel || !modelInfo(state.selectedModel))) state.selectedModel = defaultModelFor(state.mode);
  renderModelButton();
  if (!state.current) renderWelcome();
  renderChrome();
}

/** Modèles chargés en mémoire sur le serveur (indicateur de la barre latérale). */
async function refreshRunning() {
  if (!connected()) return;
  state.running = await api.runningModels().catch(() => state.running);
  state.loaded = state.running.map((r) => r.name);
  renderServerChip();
}

/** Change de serveur Ollama (ou de liste de serveurs) puis recharge son état et ses modèles. */
async function applyServers(servers: Server[], activeServer: string) {
  const next = { ...state.settings, servers, activeServer };
  await api.saveSettings(next);
  state.settings = next;
  state.server = null;
  state.models = [];
  state.modelsLoaded = false;
  renderServerChip();
  if (!state.current) renderWelcome();
  await loadModels();
}

const modelInfo = (id: string) => state.models.find((m) => m.id === id);
/** Modèles préférés pour l'agent de code, faute de réglage explicite. */
const CODE_HINT = /devstral|coder|qwen3|gpt-oss|mistral-small|ministral|magistral|granite/i;

/**
 * Modèle d'une nouvelle conversation : le dernier choisi dans ce mode, sinon celui des réglages,
 * sinon un choix automatique (de préférence un modèle déjà chargé en mémoire).
 */
function defaultModelFor(kind: Kind): string {
  const last = state.lastModel[kind];
  if (last && modelInfo(last)) return last;
  const wanted = kind === "code" ? state.settings.codeModel : state.settings.defaultModel;
  if (wanted && (modelInfo(wanted) || !state.modelsLoaded)) return wanted;
  const local = state.models.filter((m) => !m.remote);
  let pool = local.length ? local : state.models;
  if (kind === "code") {
    const tools = pool.filter((m) => m.tools);
    if (tools.length) pool = tools;
  }
  const loaded = pool.find((m) => state.loaded.includes(m.id));
  const hinted = kind === "code" ? pool.find((m) => CODE_HINT.test(m.id)) : undefined;
  return (loaded ?? hinted ?? pool[0])?.id ?? wanted;
}

/** Applique un assistant (modèle, consignes, réglages) à la conversation affichée ou au brouillon. */
function useAssistant(a: Assistant) {
  // Un second clic sur l'assistant déjà choisi (avant le premier envoi) le retire.
  const off = !state.current && state.draft.assistant === a.name;
  const patch = off
    ? { assistant: undefined, systemPrompt: undefined, params: undefined }
    : { assistant: a.name, systemPrompt: a.systemPrompt || undefined, params: a.params };
  if (state.current) {
    Object.assign(state.current, { ...patch, systemPrompt: patch.systemPrompt ?? null });
    saveCurrent();
  } else {
    Object.assign(state.draft, patch);
  }
  if (!off && a.model) {
    if (modelInfo(a.model)) state.selectedModel = a.model;
    else toast(`Le modèle ${a.model} de cet assistant n'est pas installé sur ce serveur : modèle actuel conservé.`, true);
  }
  renderChrome();
  if (!state.current) renderWelcome();
  el.input.focus();
}

/** Choix explicite d'un modèle par l'utilisateur. */
function pickModel(model: string) {
  state.selectedModel = model;
  state.lastModel[opts().kind] = model;
  renderChrome();
}

/** Fenêtre de contexte effective : réglage de la conversation ou global, limité au maximum du modèle. */
function contextFor(model: string, conv?: { params?: ConvParams } | null): number | null {
  const max = modelInfo(model)?.maxContextLength ?? null;
  const n = conv?.params?.numCtx ?? state.settings.numCtx;
  if (!n) return null;
  return max ? Math.min(n, max) : n;
}

/** Options Ollama d'une requête : contexte et raisonnement (seulement si le modèle le gère). */
function chatOptions(conv: { model: string; params?: ConvParams }, think?: boolean) {
  return {
    numCtx: contextFor(conv.model, conv),
    think: modelInfo(conv.model)?.thinking ? (think ?? conv.params?.think ?? state.settings.thinking) : null,
  };
}

// ---------------------------------------------------------------- barre latérale

function dayBucket(ts: number): string {
  const d = new Date(ts);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const diff = (today.getTime() - new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()) / 86400000;
  if (diff <= 0) return "Aujourd'hui";
  if (diff === 1) return "Hier";
  if (diff < 7) return "7 derniers jours";
  if (diff < 30) return "30 derniers jours";
  return d.toLocaleDateString("fr-FR", { month: "long", year: "numeric" });
}

function renderSidebar() {
  document.querySelectorAll<HTMLElement>(".mode-tabs button").forEach((b) => {
    b.classList.toggle("active", b.dataset.mode === state.mode);
    // Pastille sur l'onglet : exécutions en cours, en attente d'approbation ou terminées dans ce mode.
    const runs = [...state.runs.values()].filter((r) => r.conv.kind === b.dataset.mode);
    const unread = state.convs.some((c) => c.kind === b.dataset.mode && state.unread.has(c.id));
    b.dataset.status = runs.some((r) => r.approvals.size) ? "waiting" : runs.length ? "running" : unread ? "unread" : "";
  });
  el.newChatLabel.textContent = state.mode === "code" ? "Nouvelle session de code" : "Nouvelle conversation";
  renderProjectBar();
  const q = el.search.value.trim().toLowerCase();
  const pf = state.projectFilter;
  const hits = state.search.query === q ? state.search.hits : null;
  const list = state.convs.filter(
    (c) => c.kind === state.mode && (!pf || c.projectId === pf) && (!q || c.title.toLowerCase().includes(q) || !!hits?.has(c.id)),
  );
  let html = "";
  let bucket = "";
  for (const c of list) {
    const b = dayBucket(c.updatedAt);
    if (b !== bucket) {
      html += `<div class="group">${escapeHtml(b)}</div>`;
      bucket = b;
    }
    const run = state.runs.get(c.id);
    const active =
      (c.id === state.current?.id ? " active" : "") +
      (run ? (run.approvals.size ? " waiting" : " running") : state.unread.has(c.id) ? " unread" : "");
    const subParts = [
      c.workspace ? workspaceLabel(c.workspace).name : "",
      !pf && c.projectId ? (findProject(c.projectId)?.name ?? "") : "",
    ].filter(Boolean);
    const snippet = q && hits?.get(c.id);
    const sub = snippet
      ? `<span class="conv-sub conv-snippet">${highlight(snippet, q)}</span>`
      : subParts.length ? `<span class="conv-sub">${escapeHtml(subParts.join(" · "))}</span>` : "";
    html += `<div class="conv${active}" data-id="${c.id}">
      <span class="conv-text"><span class="conv-title">${escapeHtml(c.title)}</span>${sub}</span>
      <span class="conv-actions">
        <button class="icon-btn sm" data-act="rename" title="Renommer">${icons.edit}</button>
        <button class="icon-btn sm" data-act="delete" title="Supprimer">${icons.trash}</button>
      </span>
    </div>`;
  }
  const empty = q ? (hits ? "Aucun résultat" : "Recherche…") : pf ? "Aucune conversation dans ce projet" : state.mode === "code" ? "Aucune session de code" : "Aucune conversation";
  el.convList.innerHTML = html || `<p class="empty-list">${empty}</p>`;
}

const projectOptions = (none: string) =>
  `<option value="">${none}</option>` +
  projectList()
    .map((p) => `<option value="${escapeHtml(p.id)}">${escapeHtml(p.name)}</option>`)
    .join("");

function renderProjectBar() {
  if (state.projectFilter && !findProject(state.projectFilter)) state.projectFilter = "";
  el.projectSelect.innerHTML = projectOptions("Toutes les conversations") + '<option value="__new">＋ Nouveau projet…</option>';
  el.projectSelect.value = state.projectFilter;
  el.projectSelect.classList.toggle("on", !!state.projectFilter);
  el.projectEdit.hidden = !state.projectFilter;
}

el.projectSelect.addEventListener("change", () => {
  const v = el.projectSelect.value;
  if (v === "__new") {
    el.projectSelect.value = state.projectFilter;
    return newProject();
  }
  state.projectFilter = v;
  if (state.current && (state.current.projectId ?? "") !== v) return newChat();
  if (!state.current) {
    state.draft.projectId = v || undefined;
    if (state.mode === "code") state.draft.workspace = projectWorkspace(v) ?? state.draft.workspace;
  }
  renderSidebar();
  if (!state.current) renderConversation();
});
el.projectEdit.addEventListener("click", () => openProjectDialog(findProject(state.projectFilter)));
el.convProject.addEventListener("change", () => {
  const id = el.convProject.value || undefined;
  // Nouvelle session de code : on se place dans le dossier du projet choisi.
  if (!state.current && state.mode === "code") state.draft.workspace = projectWorkspace(id) ?? state.draft.workspace;
  setOpt("projectId", id);
  if (!state.current) renderConversation();
});

/** Nouveau projet : en mode Code, le dossier ouvert est proposé par défaut. */
const newProject = () =>
  openProjectDialog(null, state.mode === "code" && opts().workspace ? { workspace: opts().workspace } : {});

/** Après création, modification ou suppression d'un projet. */
function onProjectChange(saved: Project | null, deletedId?: string) {
  if (saved) {
    state.projectFilter = saved.id;
    if (!state.current) {
      state.draft.projectId = saved.id;
      if (state.mode === "code") state.draft.workspace = projectWorkspace(saved.id) ?? state.draft.workspace;
    }
    toast(`Projet « ${saved.name} » enregistré`);
  }
  if (deletedId) {
    if (state.projectFilter === deletedId) state.projectFilter = "";
    if (state.draft.projectId === deletedId) state.draft.projectId = undefined;
    toast("Projet supprimé");
  }
  renderSidebar();
  if (state.current) renderChrome();
  else renderConversation();
}

async function refreshConvs() {
  state.convs = await api.listConversations();
  renderSidebar();
}

/** Met en évidence la requête dans un extrait (insensible à la casse et aux accents). */
function highlight(text: string, q: string): string {
  const fold = (s: string) => s.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();
  const plain = fold(text);
  const i = plain.indexOf(fold(q));
  if (i < 0 || plain.length !== text.length) return escapeHtml(text);
  return escapeHtml(text.slice(0, i)) + `<mark>${escapeHtml(text.slice(i, i + q.length))}</mark>` + escapeHtml(text.slice(i + q.length));
}

let searchTimer = 0;
let searchSeq = 0;
el.search.addEventListener("input", () => {
  renderSidebar();
  clearTimeout(searchTimer);
  const q = el.search.value.trim().toLowerCase();
  if (q.length < 2) return;
  // Recherche dans le contenu des conversations (côté Rust, données déchiffrées en mémoire seulement).
  searchTimer = window.setTimeout(async () => {
    const seq = ++searchSeq;
    const hits = await api.searchConversations(q).catch(() => []);
    if (seq !== searchSeq) return;
    state.search = { query: q, hits: new Map(hits.map((h) => [h.id, h.snippet])) };
    renderSidebar();
  }, 250);
});

document.querySelector(".mode-tabs")!.addEventListener("click", (e) => {
  const b = (e.target as HTMLElement).closest<HTMLElement>("[data-mode]");
  if (b) setMode(b.dataset.mode as Kind);
});

function setMode(mode: Kind) {
  if (state.mode === mode && !state.current) return;
  state.mode = mode;
  newChat();
}

el.convList.addEventListener("click", async (ev) => {
  const target = ev.target as HTMLElement;
  const row = target.closest<HTMLElement>(".conv");
  if (!row) return;
  const id = row.dataset.id!;
  const act = target.closest<HTMLElement>("[data-act]")?.dataset.act;

  if (act === "delete") {
    const btn = target.closest<HTMLElement>("[data-act]")!;
    if (!btn.classList.contains("confirm")) {
      btn.classList.add("confirm");
      btn.title = "Cliquer à nouveau pour confirmer";
      setTimeout(() => btn.classList.remove("confirm"), 3000);
      return;
    }
    deletedConvs.add(id);
    const run = state.runs.get(id);
    if (run) await stopRun(run);
    state.unread.delete(id);
    state.lastRuns.delete(id);
    await api.deleteConversation(id);
    if (state.current?.id === id) newChat();
    await refreshConvs();
    return;
  }

  if (act === "rename") {
    const titleEl = row.querySelector<HTMLElement>(".conv-title")!;
    const input = document.createElement("input");
    input.className = "rename";
    input.value = titleEl.textContent ?? "";
    titleEl.replaceWith(input);
    input.focus();
    input.select();
    let done = false;
    const finish = async (save: boolean) => {
      if (done) return;
      done = true;
      const title = input.value.trim();
      if (save && title) {
        const conv = state.current?.id === id ? state.current : await api.loadConversation(id);
        conv.title = title;
        await api.saveConversation(conv);
        if (state.current?.id === id) el.title.textContent = title;
      }
      await refreshConvs();
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") finish(true);
      if (e.key === "Escape") finish(false);
    });
    input.addEventListener("blur", () => finish(true));
    return;
  }

  if (state.current?.id !== id) await openConversation(id);
});

async function openConversation(id: string) {
  try {
    // Conversation en cours d'exécution : on reprend l'objet vivant, pas la copie du disque.
    state.current = state.runs.get(id)?.conv ?? (await api.loadConversation(id));
  } catch (e) {
    toast(errMsg(e), true);
    return;
  }
  state.unread.delete(id);
  state.current.kind ??= "chat";
  state.mode = state.current.kind;
  state.selectedModel = state.current.model || defaultModelFor(state.mode);
  if (state.current.workspace) api.touchWorkspace(state.current.workspace).catch(() => {});
  renderSidebar();
  renderConversation();
  el.input.focus();
}

/** Dossier de travail du projet, s'il est toujours autorisé. */
const projectWorkspace = (id?: string | null) => {
  const ws = findProject(id)?.workspace;
  return ws && state.workspaces.includes(ws) ? ws : undefined;
};

function newChat() {
  state.current = null;
  state.draft = {
    // Pas de dossier repris en silence : on le choisit explicitement (sauf projet avec dossier).
    workspace: state.mode === "code" ? projectWorkspace(state.projectFilter) : undefined,
    webSearch: state.settings.webSearchDefault,
    mcp: true,
    permissionMode: "ask",
    projectId: state.projectFilter || undefined,
  };
  state.selectedModel = defaultModelFor(state.mode);
  renderSidebar();
  renderConversation();
  el.input.focus();
}

// ---------------------------------------------------------------- options de la conversation

/** Options effectives : celles de la conversation ouverte, sinon du brouillon. */
const opts = () => {
  const c = state.current;
  return {
    kind: c?.kind ?? state.mode,
    workspace: c ? c.workspace : state.draft.workspace,
    webSearch: c ? !!c.webSearch : state.draft.webSearch,
    mcp: c ? c.mcp !== false : state.draft.mcp,
    systemAccess: !!(c ? c.systemAccess : state.draft.systemAccess),
    permissionMode: (c ? c.permissionMode : state.draft.permissionMode) ?? "ask",
    projectId: c ? c.projectId : state.draft.projectId,
    systemPrompt: c ? c.systemPrompt : state.draft.systemPrompt,
    params: c ? c.params : state.draft.params,
    assistant: c ? c.assistant : state.draft.assistant,
  };
};

function setOpt<K extends keyof Draft>(key: K, value: Draft[K]) {
  if (state.current) {
    (state.current as unknown as Draft)[key] = value;
    saveCurrent();
  } else {
    state.draft[key] = value;
  }
  renderChrome();
}

const mcpToolCount = () => state.mcp.filter((s) => s.state === "ready").reduce((n, s) => n + s.tools.length, 0);

/** Barre du haut et bascules du compositeur. */
function renderChrome() {
  const c = state.current;
  const o = opts();
  el.title.textContent = c?.title ?? (o.kind === "code" ? "Nouvelle session de code" : "Nouvelle conversation");
  el.exportBtn.hidden = !c;
  el.wsChip.hidden = o.kind !== "code" || !o.workspace;
  if (o.workspace) {
    const l = workspaceLabel(o.workspace);
    el.wsChip.querySelector("span")!.textContent = l.name;
    el.wsChip.title = l.remote ? `${l.detail}\nCliquer pour copier le chemin` : `${l.detail}\nCliquer pour ouvrir le dossier`;
    el.wsChip.classList.toggle("remote", l.remote);
  }
  renderInstrChip(o.kind === "code" ? o.workspace : undefined);
  renderWsPicker();
  const projects = projectList();
  el.convProject.hidden = projects.length === 0;
  if (projects.length) {
    el.convProject.innerHTML = projectOptions("Sans projet");
    el.convProject.value = o.projectId && findProject(o.projectId) ? o.projectId : "";
    el.convProject.classList.toggle("on", !!el.convProject.value);
  }
  renderChanges();
  if (o.kind !== "code" || !o.workspace) toggleGitPanel(false);
  if (o.kind === "code") scheduleGitRefresh(200);
  else el.gitBtn.hidden = true;
  renderModelButton();
  el.convSettingsBtn.classList.toggle("on", isCustomized(o.systemPrompt, o.params, state.settings));
  el.permSelect.hidden = o.kind !== "code";
  el.permSelect.value = o.permissionMode;
  el.webToggle.classList.toggle("on", o.webSearch);
  const configured = state.mcp.filter((s) => s.state !== "disabled").length;
  el.mcpToggle.hidden = configured === 0;
  el.mcpToggle.classList.toggle("on", o.mcp);
  el.sysToggle.hidden = o.kind !== "code";
  el.sysToggle.classList.toggle("on", o.systemAccess);
  el.mcpToggle.querySelector("span")!.textContent = `MCP ${mcpToolCount()}`;
  el.mcpToggle.title = state.mcp.map((s) => `${s.name} : ${s.state === "ready" ? `${s.tools.length} outils` : s.state}`).join("\n");
  el.input.placeholder = o.kind === "code" ? "Décrivez la tâche à réaliser dans le projet…" : "Écrivez votre message…";
  $("#mic-btn").hidden = !state.settings.transcriptionUrl;
  updateComposerState();
  renderUsage();
  renderActivity();
}

el.webToggle.addEventListener("click", () => {
  if (!opts().webSearch && !state.settings.searxngUrl) {
    toast("Indiquez d'abord l'URL de votre instance SearXNG.", true);
    return openSettings("tools");
  }
  setOpt("webSearch", !opts().webSearch);
});
el.mcpToggle.addEventListener("click", () => setOpt("mcp", !opts().mcp));
el.sysToggle.addEventListener("click", () => {
  const on = !opts().systemAccess;
  setOpt("systemAccess", on);
  if (on) toast("Accès système activé : l'agent peut lire et modifier des fichiers hors du projet, avec votre approbation.");
});
el.permSelect.addEventListener("change", () => setOpt("permissionMode", el.permSelect.value as PermissionMode));
el.wsChip.addEventListener("click", () => {
  const ws = opts().workspace;
  if (!ws) return;
  const r = parseRemote(ws);
  if (r) {
    copy(r.path);
    toast(`Chemin copié : ${r.path}`);
  } else {
    revealItemInDir(ws).catch((e) => toast(errMsg(e), true));
  }
});

const instrLoading = new Set<string>();

/** (Re)lit les fichiers de consignes du dossier. */
async function loadInstructions(ws: string): Promise<InstructionFile[]> {
  instrLoading.add(ws);
  try {
    const files = await api.workspaceInstructions(ws).catch(() => [] as InstructionFile[]);
    state.instructions.set(ws, files);
    return files;
  } finally {
    instrLoading.delete(ws);
    if (opts().kind === "code" && opts().workspace === ws) {
      renderInstrChip(ws);
      if (!state.current) renderWelcome();
      else renderUsage();
    }
  }
}

function renderInstrChip(ws: string | undefined) {
  const files = ws ? state.instructions.get(ws) : undefined;
  if (ws && !files && !instrLoading.has(ws)) loadInstructions(ws);
  el.instrChip.hidden = !files?.length;
  if (!ws || !files?.length) return;
  el.instrChip.querySelector("span")!.textContent = files.map((f) => f.name).join(" + ");
  el.instrChip.title =
    "Consignes du projet lues automatiquement par l'agent à chaque tâche :\n" +
    files.map((f) => `• ${f.name} (${(f.content.length / 1024).toFixed(1)} Ko${f.truncated ? ", tronqué" : ""})`).join("\n") +
    (parseRemote(ws) ? "" : "\nCliquer pour afficher le fichier");
}

el.instrChip.addEventListener("click", () => {
  const ws = opts().workspace;
  const f = ws ? state.instructions.get(ws)?.[0] : undefined;
  if (!ws || !f || parseRemote(ws)) return;
  const sep = ws.includes("\\") && !ws.includes("/") ? "\\" : "/";
  revealItemInDir(`${ws.replace(/[\\/]$/, "")}${sep}${f.name}`).catch((e) => toast(errMsg(e), true));
});

function selectWorkspace(ws: string) {
  toggleWsMenu(false);
  state.draft.workspace = ws;
  api
    .touchWorkspace(ws)
    .then(() => api.listWorkspaces())
    .then((w) => (state.workspaces = w))
    .catch(() => {});
  renderConversation();
  el.input.focus();
}

/** Bouton « dossier » de la zone de saisie (nouvelle session de code uniquement). */
function renderWsPicker() {
  const show = !state.current && state.mode === "code";
  el.wsPicker.hidden = !show;
  if (!show) return toggleWsMenu(false);
  const ws = state.draft.workspace;
  const l = ws ? workspaceLabel(ws) : null;
  el.wsPicker.querySelector("span")!.textContent = l ? l.name : "Choisir un dossier";
  el.wsPicker.classList.toggle("on", !!ws);
  el.wsPicker.classList.toggle("remote", !!l?.remote);
  el.wsPicker.title = l ? `${l.detail}\nCliquer pour changer de dossier` : "Choisir le dossier de travail : local ou serveur SSH";
}

function toggleWsMenu(open = el.wsMenu.hidden) {
  if (!open) {
    el.wsMenu.hidden = true;
    return;
  }
  const cur = state.draft.workspace;
  const recent = state.workspaces
    .slice(0, 8)
    .map((w) => {
      const l = workspaceLabel(w);
      return `<button type="button" class="ws-menu-item${w === cur ? " current" : ""}" data-ws="${escapeHtml(w)}" title="${escapeHtml(l.detail)}">${l.remote ? icons.server : icons.folder}<span>${escapeHtml(l.name)}</span><small>${escapeHtml(l.detail)}</small></button>`;
    })
    .join("");
  el.wsMenu.innerHTML = `${recent ? `<div class="group">Dossiers récents</div>${recent}<hr />` : ""}
    <button type="button" class="ws-menu-item action" data-pick-ws>${icons.folder}<span>Ouvrir un dossier local…</span></button>
    <button type="button" class="ws-menu-item action" data-ssh>${icons.server}<span>Dossier sur un serveur SSH…</span></button>`;
  el.wsMenu.hidden = false;
}

el.wsPicker.addEventListener("click", (e) => {
  e.stopPropagation();
  toggleWsMenu();
});
el.wsMenu.addEventListener("click", (e) => {
  const t = e.target as HTMLElement;
  const ws = t.closest<HTMLElement>("[data-ws]")?.dataset.ws;
  if (ws) return selectWorkspace(ws);
  if (t.closest("[data-pick-ws]")) {
    toggleWsMenu(false);
    return chooseWorkspace();
  }
  if (t.closest("[data-ssh]")) {
    toggleWsMenu(false);
    return openSshDialog();
  }
});
document.addEventListener("click", (e) => {
  if (!el.wsMenu.hidden && !(e.target as HTMLElement).closest("#ws-menu, #ws-picker")) toggleWsMenu(false);
});

function openRemoteWorkspace(ws: string) {
  api.listWorkspaces().then((w) => (state.workspaces = w)).catch(() => {});
  if (state.current) newChat();
  state.draft.workspace = ws;
  renderConversation();
  el.input.focus();
}

async function chooseWorkspace() {
  try {
    const path = await api.pickWorkspace();
    if (!path) return;
    state.workspaces = await api.listWorkspaces();
    state.draft.workspace = path;
    renderConversation();
    el.input.focus();
  } catch (e) {
    toast(errMsg(e), true);
  }
}

// ---------------------------------------------------------------- messages

/** Modèles proposés pour un premier téléchargement. */
const STARTER_MODELS = [
  { id: "ministral-3:8b", title: "Polyvalent", desc: "Mistral AI · chat, images, outils · ≈ 6 Go" },
  { id: "qwen3:8b", title: "Raisonnement", desc: "Alibaba · réfléchit avant de répondre · ≈ 5 Go" },
  { id: "devstral-small-2", title: "Agent de code", desc: "Mistral AI · pour l'onglet Code · ≈ 15 Go" },
];

/** Étapes de l'accueil guidé : serveur, premier modèle, prise en main. */
function onboardingHtml(step: 1 | 2 | 3, body: string): string {
  const steps = ["Serveur Ollama", "Premier modèle", "C'est parti"];
  const head = steps
    .map((label, i) => `<li class="${i + 1 < step ? "done" : i + 1 === step ? "current" : ""}"><span>${i + 1 < step ? "✓" : i + 1}</span>${label}</li>`)
    .join("");
  return `<div class="welcome setup"><ol class="stepper">${head}</ol>${body}</div>`;
}

function renderWelcome() {
  if (!state.server) {
    el.messages.innerHTML = `<div class="welcome"><p class="muted">Connexion à Ollama…</p></div>`;
    return;
  }
  if (!connected()) {
    el.messages.innerHTML = onboardingHtml(
      1,
      `<h2>${escapeHtml(state.server.name)} est injoignable</h2>
      <p class="muted">${escapeHtml(state.server.error ?? "")}</p>
      ${
        state.server.tunnel
          ? `<ol class="setup-steps">
        <li>Vérifiez qu'Ollama tourne sur le serveur : <code>systemctl status ollama</code></li>
        <li>Vérifiez la connexion SSH dans « Serveurs SSH… » (clé installée, empreinte validée).</li>
      </ol>`
          : `<ol class="setup-steps">
        <li>Installez Ollama : <a href="https://ollama.com/download" data-external>ollama.com/download</a> (Linux : <code>curl -fsSL https://ollama.com/install.sh | sh</code>, ou le paquet de votre distribution)</li>
        <li>Lancez le serveur : <code>ollama serve</code> (ou <code>systemctl start ollama</code>)</li>
        <li>Ollama sur une autre machine ? Ajoutez-la dans les réglages : adresse directe ou tunnel SSH.</li>
      </ol>`
      }
      <p class="row center"><button class="btn-primary" data-retry-server>Réessayer</button><button class="btn-ghost" data-open-settings>Serveurs…</button></p>
      <p class="muted small">La connexion est retentée automatiquement toutes les 15 secondes.</p>`,
    );
    return;
  }
  if (state.modelsLoaded && !state.models.length) {
    el.messages.innerHTML = onboardingHtml(
      2,
      `<h2>Choisissez un premier modèle</h2>
      <p>Connecté à Ollama ${escapeHtml(state.server.version ?? "")}. Le modèle est téléchargé une fois, puis tourne entièrement sur votre machine.</p>
      <div class="starter-models">${STARTER_MODELS.map(
        (m) => `<button class="starter" data-pull-model="${m.id}"><strong>${m.title}</strong><code>${m.id}</code><small>${m.desc}</small></button>`,
      ).join("")}</div>
      <p><button class="btn-ghost" data-open-models>Voir tous les modèles…</button></p>`,
    );
    return;
  }
  if (!state.settings.onboarded) {
    el.messages.innerHTML = onboardingHtml(
      3,
      `<h2>Tout est prêt</h2>
      <p>${state.models.length} modèle${state.models.length > 1 ? "s" : ""} disponible${state.models.length > 1 ? "s" : ""} sur votre serveur Ollama. Rien ne quitte votre machine.</p>
      <div class="tips">
        <div class="tip"><strong>Chat</strong><small>Posez une question, joignez une image, un PDF ou du code. Plusieurs conversations peuvent tourner en même temps.</small></div>
        <div class="tip"><strong>Code</strong><small>Un agent lit, modifie et teste un projet local ou sur un serveur SSH, avec votre accord à chaque étape.</small></div>
        <div class="tip"><strong>${state.platform.os === "macos" ? "⌘" : "Ctrl+"}K</strong><small>La palette de commandes : actions, conversations, modèles et projets au clavier.</small></div>
      </div>
      <p><button class="btn-primary" data-finish-onboarding>Commencer</button></p>`,
    );
    return;
  }
  if (state.mode === "code") {
    const ws = state.draft.workspace;
    const recent = state.workspaces
      .filter((w) => w !== ws)
      .slice(0, 8)
      .map((w) => {
        const l = workspaceLabel(w);
        return `<button class="ws-item" data-ws="${escapeHtml(w)}" title="${escapeHtml(l.detail)}">${l.remote ? icons.server : icons.folder}<span>${escapeHtml(l.name)}</span><small>${escapeHtml(l.detail)}</small></button>`;
      })
      .join("");
    const cur = ws ? workspaceLabel(ws) : null;
    const selected = cur
      ? `<div class="ws-current selected">${cur.remote ? icons.server : icons.folder}<div><small class="ws-tag">Dossier de cette session</small><strong>${escapeHtml(cur.name)}</strong><small>${escapeHtml(cur.detail)}</small></div></div>
         ${instrHint(ws!)}`
      : "";
    el.messages.innerHTML = `<div class="welcome code">
      <h2>Nouvelle session de code</h2>
      <p>${cur ? "L'agent va travailler dans ce dossier. Décrivez la tâche ci-dessous, ou choisissez un autre dossier." : "Où l'agent doit-il travailler ? Choisissez un dossier de cet ordinateur ou d'un serveur distant."}</p>
      ${selected}
      <div class="ws-choices">
        <button class="ws-choice" data-pick-ws>${icons.folder}<strong>Dossier local</strong><small>Un dossier de cet ordinateur</small></button>
        <button class="ws-choice" data-ssh>${icons.server}<strong>Serveur SSH</strong><small>Un dossier sur une machine distante</small></button>
      </div>
      ${projectBanner()}
      ${recent ? `<div class="ws-recent"><div class="group">Dossiers récents</div>${recent}</div>` : ""}
      <p class="muted small center">Chaque modification et commande peut être soumise à votre approbation (menu « Tout demander » en haut).</p>
    </div>`;
    return;
  }
  const hour = new Date().getHours();
  const greet = hour < 6 || hour >= 18 ? "Bonsoir" : "Bonjour";
  const p = findProject(state.draft.projectId);
  const applied = state.draft.assistant;
  const cards = `<div class="assistant-cards">${assistants()
    .map(
      (a) => `<button class="assistant-card${a.name === applied ? " on" : ""}" data-assistant="${escapeHtml(a.id)}" title="${escapeHtml(a.description || a.systemPrompt.slice(0, 200))}">
        <strong>${escapeHtml(a.name)}</strong><small>${escapeHtml(a.description || a.model || "modèle par défaut")}</small></button>`,
    )
    .join("")}<button class="assistant-card add" data-new-assistant>＋ ${assistants().length ? "Nouvel assistant" : "Créer un assistant"}</button></div>`;
  el.messages.innerHTML = p
    ? `<div class="welcome"><h2>${escapeHtml(p.name)}</h2><p>Nouvelle conversation dans ce projet.</p>${projectBanner()}</div>`
    : `<div class="welcome wide"><h2>${applied ? escapeHtml(applied) : `${greet} 👋`}</h2><p>${applied ? "Assistant sélectionné pour cette conversation." : "Comment puis-je vous aider ?"}</p>${cards}</div>`;
}

function instrHint(ws: string): string {
  const files = state.instructions.get(ws);
  if (!files) return "";
  if (files.length) {
    return `<p class="instr-hint">✓ Consignes chargées automatiquement : <strong>${files.map((f) => escapeHtml(f.name)).join(", ")}</strong></p>`;
  }
  return `<p class="instr-hint">Aucun fichier <code>AGENTS.md</code> : l'agent redécouvre le projet à chaque session.
    <button class="btn-ghost sm" data-init-agents>Le faire générer par l'agent</button></p>`;
}

function projectBanner(): string {
  const p = findProject(state.draft.projectId);
  if (!p) return "";
  const n = p.files.length;
  const bits = [
    p.instructions.trim() ? "consignes" : "",
    n ? `${n} fichier${n > 1 ? "s" : ""} de référence` : "",
    p.workspace ? `dossier ${workspaceLabel(p.workspace).name}` : "",
    projectChars(p) > FULL_PROJECT_CHARS
      ? "recherche locale par extraits"
      : `≈ ${Math.round(projectChars(p) / 3.5).toLocaleString("fr-FR")} tokens de contexte`,
  ].filter(Boolean);
  return `<div class="project-banner">${icons.folder}<div><strong>Projet ${escapeHtml(p.name)}</strong><small>${bits.join(" · ")}</small></div>
    <button class="btn-ghost sm" data-edit-project>Modifier</button></div>`;
}

function messageHtml(m: Message, next: Message | undefined): string {
  if (m.role === "user" && m.auto) {
    return `<div class="msg auto" data-id="${m.id}">↻ Relance automatique : l'agent avait annoncé une action sans l'exécuter</div>`;
  }
  if (m.role === "user") {
    const imgs = (m.images ?? []).map((src) => `<img src="${src}" alt="image jointe" />`).join("");
    const files = (m.files ?? []).map((f) => `<span class="chip">${icons.file}${escapeHtml(f.name)}</span>`).join("");
    return `<div class="msg user" data-id="${m.id}">
      ${imgs || files ? `<div class="msg-attachments">${imgs}${files}</div>` : ""}
      <div class="bubble">${escapeHtml(m.content).replace(/\n/g, "<br>")}</div>
      <div class="msg-actions">
        <button class="icon-btn sm" data-act="copy" title="Copier">${icons.copy}</button>
        <button class="icon-btn sm" data-act="edit" title="Modifier et renvoyer">${icons.edit}</button>
      </div>
    </div>`;
  }
  const streaming = currentRun()?.msgId === m.id;
  const reasoning = m.reasoning
    ? `<details class="reasoning"${streaming && !m.content ? " open" : ""}><summary>Raisonnement</summary><div class="md">${renderMarkdown(m.reasoning)}</div></details>`
    : "";
  const tools = (m.toolCalls ?? []).map((tc) => toolCardHtml(tc, state.risks.get(tc.name) ?? guessRisk(tc.name))).join("");
  const body = m.content
    ? renderMarkdown(m.content)
    : streaming && !m.reasoning && !tools ? '<span class="typing"><i></i><i></i><i></i></span>' : "";
  const meta = [m.model, m.usage ? `${m.usage.promptTokens} → ${m.usage.completionTokens} tokens` : "", fmtSpeed(m.usage?.tokensPerSec), m.cancelled ? "interrompu" : ""]
    .filter(Boolean)
    .join(" · ");
  const error = m.error
    ? `<div class="msg-error">${escapeHtml(m.error)} <button class="btn-ghost sm" data-act="retry">Réessayer</button></div>`
    : "";
  const endOfTurn = !next || next.role === "user";
  const undoCount = !next && state.current ? undoable(state.current).length : 0;
  // Comparaison avec un autre modèle : dernière réponse d'un tour simple (sans appel d'outil).
  const msgs = state.current?.messages ?? [];
  const canCompare = !next && !m.toolCalls?.length && !m.alt && !!m.content && msgs[msgs.indexOf(m) - 1]?.role === "user" && state.models.length > 1;
  // Pendant l'exécution, le dernier message n'a pas encore d'actions (copier, régénérer…).
  const actions = streaming || !endOfTurn || (currentRun() && !next)
    ? ""
    : `<div class="msg-actions">
        <button class="icon-btn sm" data-act="copy" title="Copier">${icons.copy}</button>
        ${!next ? `<button class="icon-btn sm" data-act="retry" title="Régénérer">${icons.retry}</button>` : ""}
        ${canCompare ? `<button class="icon-btn sm" data-act="compare" title="Comparer avec un autre modèle">${icons.compare}</button>` : ""}
        ${undoCount ? `<button class="btn-ghost sm" data-undo-turn title="Remet les fichiers dans l'état d'avant ce tour de l'agent">↶ Annuler ${undoCount > 1 ? `les ${undoCount} modifications` : "la modification"}</button>` : ""}
        <span class="meta">${escapeHtml(meta)}</span>
      </div>`;
  if (m.alt) {
    // Deux réponses côte à côte : la réponse d'origine et celle du second modèle.
    const a = m.alt;
    const stat = (u: Message["usage"]) => (u ? [`${u.completionTokens} tokens`, fmtSpeed(u.tokensPerSec)].filter(Boolean).join(" · ") : "");
    const altBody = a.error
      ? `<div class="msg-error">${escapeHtml(a.error)}</div>`
      : a.content
        ? `<div class="md">${renderMarkdown(a.content)}</div>`
        : a.reasoning
          ? `<details class="reasoning" open><summary>Raisonnement</summary><div class="md">${renderMarkdown(a.reasoning)}</div></details>`
          : '<span class="typing"><i></i><i></i><i></i></span>';
    return `<div class="msg assistant has-alt" data-id="${m.id}">
      <div class="compare">
        <div class="cmp-col"><div class="cmp-head"><code>${escapeHtml(m.model ?? "")}</code><span>${escapeHtml(stat(m.usage))}</span></div>${body ? `<div class="md">${body}</div>` : ""}</div>
        <div class="cmp-col"><div class="cmp-head"><code>${escapeHtml(a.model)}</code><span>${escapeHtml(stat(a.usage))}</span><span class="spacer"></span>${
          currentRun()
            ? ""
            : `<button class="btn-ghost sm" data-act="keep-alt" title="Garder cette réponse à la place de l'autre">Garder</button>
               <button class="icon-btn sm" data-act="drop-alt" title="Fermer la comparaison">×</button>`
        }</div>${altBody}</div>
      </div>
    </div>`;
  }
  return `<div class="msg assistant${streaming ? " streaming" : ""}" data-id="${m.id}">
    ${reasoning}${body ? `<div class="md">${body}</div>` : ""}${tools ? `<div class="tools">${tools}</div>` : ""}${error}${actions}
  </div>`;
}

/** Risque déduit du nom quand la définition n'est pas (encore) chargée. */
function guessRisk(name: string): Risk {
  if (name === "write_file" || name === "edit_file") return "write";
  if (name === "run_command") return "exec";
  if (name.startsWith("mcp_")) return "mcp";
  return "read";
}

const nearBottom = () => el.messages.scrollHeight - el.messages.scrollTop - el.messages.clientHeight < 120;
const scrollBottom = () => (el.messages.scrollTop = el.messages.scrollHeight);

function renderConversation() {
  renderChrome();
  const c = state.current;
  if (!c || c.messages.length === 0) return renderWelcome();
  const divider = (i: number) =>
    c.compaction && c.compaction.upto === i
      ? `<details class="compaction"><summary>Contexte compacté ici le ${new Date(c.compaction.at).toLocaleString("fr-FR")} : les messages précédents sont remplacés par ce résumé pour le modèle</summary><div class="md">${renderMarkdown(c.compaction.summary)}</div></details>`
      : "";
  el.messages.innerHTML = `<div class="thread">${c.messages.map((m, i) => divider(i) + messageHtml(m, c.messages[i + 1])).join("")}${divider(c.messages.length)}</div>`;
  scrollBottom();
}

/** Re-rendu d'un seul message, limité à une fois par frame pendant le streaming. */
const dirty = new Set<Message>();
let rafPending = false;
function rerenderMessage(m: Message, force = false) {
  dirty.add(m);
  const draw = () => {
    rafPending = false;
    const c = state.current;
    if (!c) return dirty.clear();
    const stick = nearBottom();
    for (const msg of dirty) {
      const node = el.messages.querySelector(`.msg[data-id="${msg.id}"]`);
      const idx = c.messages.indexOf(msg);
      if (idx < 0) continue;
      if (!node) {
        // Nouveau message (tour suivant de l'agent) : on l'ajoute au fil.
        const thread = el.messages.querySelector(".thread");
        if (!thread) {
          renderConversation();
          break;
        }
        thread.insertAdjacentHTML("beforeend", messageHtml(msg, c.messages[idx + 1]));
        // Le message précédent perd ses actions de fin de tour.
        const prev = c.messages[idx - 1];
        const prevNode = prev && el.messages.querySelector(`.msg[data-id="${prev.id}"]`);
        if (prevNode) prevNode.outerHTML = messageHtml(prev, msg);
        continue;
      }
      const openDetails = [...node.querySelectorAll("details")].map((d) => d.open);
      node.outerHTML = messageHtml(msg, c.messages[idx + 1]);
      const fresh = el.messages.querySelector(`.msg[data-id="${msg.id}"]`);
      fresh?.querySelectorAll("details").forEach((d, i) => {
        if (openDetails[i] !== undefined) d.open = openDetails[i];
      });
    }
    dirty.clear();
    if (stick) scrollBottom();
  };
  if (force) return draw();
  if (!rafPending) {
    rafPending = true;
    requestAnimationFrame(draw);
  }
}

el.messages.addEventListener("click", async (ev) => {
  const t = ev.target as HTMLElement;
  if (t.closest("[data-open-settings]")) return openSettings();
  if (t.closest("[data-open-models]")) return openModelsDialog();
  if (t.closest("[data-new-assistant]")) return openAssistantsDialog(null);
  const card = assistants().find((a) => a.id === t.closest<HTMLElement>("[data-assistant]")?.dataset.assistant);
  if (card) return useAssistant(card);
  const starter = t.closest<HTMLElement>("[data-pull-model]");
  if (starter) return openModelsDialog(starter.dataset.pullModel);
  if (t.closest("[data-finish-onboarding]")) {
    state.settings.onboarded = true;
    api.saveSettings(state.settings).catch((e) => toast(errMsg(e), true));
    renderWelcome();
    return el.input.focus();
  }
  if (t.closest("[data-retry-server]")) {
    state.server = null;
    renderWelcome();
    return loadModels();
  }
  if (t.closest("[data-pick-ws]")) return chooseWorkspace();
  if (t.closest("[data-ssh]")) return openSshDialog();
  if (t.closest("[data-edit-project]")) return openProjectDialog(findProject(opts().projectId));
  if (t.closest("[data-init-agents]")) {
    el.input.value = INIT_AGENTS_PROMPT;
    autoGrow(el.input);
    return el.input.focus();
  }
  const wsItem = t.closest<HTMLElement>("[data-ws]");
  if (wsItem) return selectWorkspace(wsItem.dataset.ws!);

  const approve = t.closest<HTMLElement>("[data-approve]");
  if (approve) {
    const id = approve.closest<HTMLElement>("[data-tc]")?.dataset.tc;
    const resolve = id && currentRun()?.approvals.get(id);
    const card = approve.closest<HTMLElement>("[data-tc]");
    const pw = card?.querySelector<HTMLInputElement>(".sudo-pw");
    if (resolve) {
      resolve(approve.dataset.approve as Decision, pw ? { password: pw.value, keep: !!card?.querySelector<HTMLInputElement>(".sudo-keep")?.checked } : undefined);
    }
    return;
  }
  if (t.closest("[data-forget-sudo]") && state.current) {
    // Mot de passe oublié : la carte redemande la saisie.
    sudoCache.delete(sudoTarget(state.current));
    for (const m of state.current.messages) for (const tc of m.toolCalls ?? []) if (tc.status === "pending" && tc.sudo) tc.sudo = "ask";
    return renderConversation();
  }
  if (t.closest("[data-undo-turn]")) return undoTurn(t.closest<HTMLElement>("[data-undo-turn]")!);
  if (t.closest("[data-run-plan]")) return runPlan();

  const codeBtn = t.closest<HTMLElement>(".copy-code");
  if (codeBtn) {
    const code = codeBtn.closest(".code-block")?.querySelector("code")?.textContent ?? "";
    return copy(code, codeBtn);
  }

  const actBtn = t.closest<HTMLElement>("[data-act]");
  const msgEl = t.closest<HTMLElement>(".msg");
  if (!actBtn || !msgEl || !state.current) return;
  const msg = state.current.messages.find((m) => m.id === msgEl.dataset.id);
  if (!msg) return;

  switch (actBtn.dataset.act) {
    case "copy":
      return copy(turnText(msg), actBtn);
    case "retry":
      return regenerate(msg);
    case "edit":
      return editMessage(msg, msgEl);
    case "compare":
      return openModelMenu(actBtn, {
        selected: "",
        exclude: [msg.model ?? ""],
        title: "Comparer avec…",
        pick: (model) => compare(msg, model),
      });
    case "keep-alt":
    case "drop-alt":
      // « Garder » : la réponse du second modèle remplace celle d'origine.
      if (actBtn.dataset.act === "keep-alt" && msg.alt && !msg.alt.error) {
        Object.assign(msg, { content: msg.alt.content, model: msg.alt.model, usage: msg.alt.usage, reasoning: msg.alt.reasoning });
      }
      delete msg.alt;
      rerenderMessage(msg, true);
      return saveCurrent();
  }
});

el.messages.addEventListener("keydown", (e) => {
  const t = e.target as HTMLElement;
  if (e.key === "Enter" && t.classList.contains("sudo-pw")) {
    e.preventDefault();
    t.closest("[data-tc]")?.querySelector<HTMLElement>('[data-approve="allow"]')?.click();
  }
});

/** Texte d'un tour complet de l'agent (plusieurs messages assistant consécutifs). */
function turnText(msg: Message): string {
  if (msg.role === "user" || !state.current) return msg.content;
  const msgs = state.current.messages;
  let i = msgs.indexOf(msg);
  const parts: string[] = [];
  while (i >= 0 && msgs[i].role === "assistant") parts.unshift(msgs[i--].content);
  return parts.filter((p) => p.trim()).join("\n\n");
}

function editMessage(msg: Message, node: HTMLElement) {
  if (currentRun()) return;
  const bubble = node.querySelector<HTMLElement>(".bubble")!;
  const box = document.createElement("div");
  box.className = "edit-box";
  box.innerHTML = `<textarea rows="3"></textarea>
    <div class="row end"><button class="btn-ghost" data-edit="cancel">Annuler</button>
    <button class="btn-primary" data-edit="send">Envoyer</button></div>`;
  const ta = box.querySelector("textarea")!;
  ta.value = msg.content;
  bubble.replaceWith(box);
  node.querySelector(".msg-actions")?.remove();
  ta.focus();
  autoGrow(ta);
  ta.addEventListener("input", () => autoGrow(ta));
  box.addEventListener("click", async (e) => {
    const action = (e.target as HTMLElement).dataset.edit;
    if (action === "cancel") renderConversation();
    if (action === "send") {
      const text = ta.value.trim();
      if (!text || !state.current) return;
      const idx = state.current.messages.findIndex((m) => m.id === msg.id);
      msg.content = text;
      state.current.messages = state.current.messages.slice(0, idx + 1);
      await runAgent();
    }
  });
}

/** Envoie la même question à un second modèle et affiche les deux réponses côte à côte. */
async function compare(msg: Message, model: string) {
  const conv = state.current;
  if (!conv || state.runs.has(conv.id)) return;
  const idx = conv.messages.indexOf(msg);
  if (idx < 0) return;
  const now = Date.now();
  const run = startRun(conv, { phase: "Comparaison…", detail: model, since: now, runStart: now, step: 0, maxSteps: 0 });
  const alt = (msg.alt = { model, content: "" } as NonNullable<Message["alt"]>);
  run.requestId = uid();
  updateComposerState();
  renderSidebar();
  rerenderMessage(msg, true);
  try {
    // Même historique jusqu'à la question, sans outils : seules les réponses sont comparées.
    const messages = toApiMessages({ ...conv, messages: conv.messages.slice(0, idx) }, sysPrompt(conv));
    await api.chat(
      run.requestId,
      {
        model,
        messages,
        temperature: conv.params?.temperature ?? state.settings.temperature,
        maxTokens: state.settings.maxTokens,
        tools: [],
        ...chatOptions({ model, params: conv.params }),
      },
      (ev) => {
        if (ev.type === "delta") {
          alt.content += ev.text;
          setActivity(run, "Comparaison : rédaction…", model);
        } else if (ev.type === "reasoning") {
          alt.reasoning = (alt.reasoning ?? "") + ev.text;
          setActivity(run, "Comparaison : réflexion…", model);
        } else if (ev.type === "usage") {
          alt.usage = { promptTokens: ev.promptTokens, completionTokens: ev.completionTokens, tokensPerSec: ev.tokensPerSec };
          recordUsage(conv, model, ev.promptTokens, ev.completionTokens, true);
        } else if (ev.type === "retry") {
          setActivity(run, "Nouvel essai dans", ev.reason, { until: Date.now() + ev.delaySecs * 1000 });
        } else if (ev.type === "done" && ev.cancelled && !alt.content) {
          alt.error = "Comparaison interrompue.";
        }
        rerenderMessage(msg);
      },
    );
    if (!alt.content && !alt.error) alt.error = "Réponse vide du modèle.";
  } catch (e) {
    alt.error = errMsg(e);
  } finally {
    endRun(run);
    if (viewing(conv)) {
      rerenderMessage(msg, true);
      updateComposerState();
      renderActivity();
      renderUsage();
    }
    renderSidebar();
    await save(conv);
  }
}

async function regenerate(msg: Message) {
  if (currentRun() || !state.current) return;
  const msgs = state.current.messages;
  let idx = msgs.indexOf(msg);
  // On repart du dernier message utilisateur précédant ce tour.
  while (idx >= 0 && msgs[idx].role !== "user") idx--;
  if (idx < 0) return;
  state.current.messages = msgs.slice(0, idx + 1);
  renderConversation();
  await runAgent();
}

// ---------------------------------------------------------------- envoi et boucle d'agent

async function save(conv: Conversation) {
  if (deletedConvs.has(conv.id)) return;
  conv.updatedAt = Date.now();
  try {
    await api.saveConversation(conv);
    await refreshConvs();
  } catch (e) {
    toast(`Sauvegarde impossible : ${errMsg(e)}`, true);
  }
}

const saveCurrent = async () => {
  if (state.current) await save(state.current);
};

async function send() {
  if (currentRun()) return stop();
  const text = el.input.value.trim();
  const { images, files } = state.pending;
  if (!text && !images.length && !files.length) return;
  if (!connected()) {
    toast("Ollama est injoignable : vérifiez qu'il est lancé et son adresse dans les réglages.", true);
    return loadModels();
  }

  const o = opts();
  if (o.kind === "code" && !o.workspace) {
    toast("Choisissez d'abord le dossier de travail : local ou serveur SSH.", true);
    return toggleWsMenu(true);
  }
  const model = state.selectedModel;
  if (!model) {
    toast("Aucun modèle : téléchargez-en un.", true);
    return openModelsDialog();
  }
  const info = modelInfo(model);
  if (images.length && info?.vision === false) {
    toast(`Le modèle ${model} ne comprend pas les images. Choisissez un modèle vision (mistral-small3.2, ministral-3, gemma3, qwen3-vl…).`, true);
    return;
  }
  if (o.kind === "code" && info?.tools === false) {
    toast(`Le modèle ${model} ne gère pas les appels d'outils, indispensables à l'agent. Choisissez par ex. devstral-small-2, qwen3-coder ou qwen3.`, true);
    return;
  }

  if (!state.current) {
    const now = Date.now();
    const title = (text || files[0]?.name || "Image").replace(/\s+/g, " ").slice(0, 60);
    state.current = {
      id: uid(),
      title,
      kind: o.kind,
      model,
      createdAt: now,
      updatedAt: now,
      systemPrompt: o.systemPrompt ?? (state.settings.systemPrompt || null),
      ...(o.params ? { params: o.params } : {}),
      ...(o.assistant ? { assistant: o.assistant } : {}),
      messages: [],
      workspace: o.workspace,
      permissionMode: o.permissionMode,
      webSearch: o.webSearch,
      mcp: o.mcp,
      ...(o.kind === "code" && o.systemAccess ? { systemAccess: true } : {}),
      ...(o.projectId ? { projectId: o.projectId } : {}),
    };
  }
  for (const m of state.current.messages) for (const tc of m.toolCalls ?? []) delete tc.undo;
  state.current.messages.push({
    id: uid(),
    role: "user",
    content: text,
    createdAt: Date.now(),
    ...(images.length ? { images: [...images] } : {}),
    ...(files.length ? { files: [...files] } : {}),
  });
  el.input.value = "";
  autoGrow(el.input);
  state.pending = { images: [], files: [] };
  renderAttachments();
  await runAgent();
}

const toolCtx = (c: Conversation): ToolContext => ({
  workspace: c.kind === "code" ? c.workspace : undefined,
  web: !!c.webSearch,
  mcp: c.mcp !== false,
  system: c.kind === "code" && !!c.systemAccess,
  project: c.projectId && state.projectCtx.get(c.id)?.mode === "excerpts" ? c.projectId : undefined,
});

// ---------------------------------------------------------------- activité, contexte, consommation

let activityTimer = 0;

function setActivity(run: Run, phase: string, detail?: string, extra: Partial<Activity> = {}) {
  const a = run.activity;
  if (a.phase !== phase || a.detail !== detail) {
    Object.assign(a, { phase, detail, since: Date.now(), until: undefined }, extra);
  } else {
    Object.assign(a, extra);
  }
  if (viewing(run.conv)) renderActivity();
}

function renderActivity() {
  const a = currentRun()?.activity;
  const done = state.current ? state.lastRuns.get(state.current.id) : undefined;
  if (a) {
    const now = Date.now();
    const wait = a.until ? ` ${Math.max(0, Math.ceil((a.until - now) / 1000))} s` : "";
    const step = a.maxSteps > 1 ? ` · étape ${a.step}/${a.maxSteps}` : "";
    el.activity.className = "activity busy";
    el.activity.innerHTML = `<span class="pulse"></span><strong>${escapeHtml(a.phase)}${wait}</strong>${
      a.detail ? `<span class="act-detail">${escapeHtml(a.detail)}</span>` : ""
    }<span class="act-meta">${fmtDuration(now - a.since)} · total ${fmtDuration(now - a.runStart)}${step}</span><span class="act-hint">Échap pour arrêter</span>`;
    el.activity.hidden = false;
  } else if (done) {
    const icon = done.kind === "done" ? "✓" : done.kind === "stopped" ? "■" : "⚠";
    el.activity.className = `activity ${done.kind}`;
    el.activity.innerHTML = `<span class="act-icon">${icon}</span><span>${escapeHtml(done.text)}</span>${
      done.plan
        ? '<button type="button" class="btn-primary sm" data-run-plan title="Quitte le mode plan (chaque action reste soumise à votre accord) et lance l\'exécution">Exécuter le plan</button>'
        : done.canContinue ? '<button type="button" class="btn-primary sm" data-continue>Continuer</button>' : ""
    }`;
    el.activity.hidden = false;
  } else {
    el.activity.hidden = true;
  }
}

/** Démarre l'exécution d'une conversation (chronomètre de la barre d'activité compris). */
function startRun(conv: Conversation, activity: Activity): Run {
  const run: Run = { conv, requestId: "", msgId: "", stopped: false, activity, approvals: new Map() };
  state.runs.set(conv.id, run);
  state.lastRuns.delete(conv.id);
  clearInterval(activityTimer);
  activityTimer = window.setInterval(renderActivity, 1000);
  return run;
}

function endRun(run: Run) {
  state.runs.delete(run.conv.id);
  for (const resolve of [...run.approvals.values()]) resolve("deny");
  run.approvals.clear();
  if (!state.runs.size) clearInterval(activityTimer);
}

el.activity.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest("[data-run-plan]")) return runPlan();
  if (!(e.target as HTMLElement).closest("[data-continue]") || currentRun()) return;
  el.input.value = "Continue.";
  send();
});

function renderUsage() {
  const c = state.current;
  if (!c || !c.messages.length) {
    el.ctxMeter.hidden = true;
    el.usageInfo.hidden = true;
    return;
  }
  const model = currentRun() ? c.model : state.selectedModel || c.model;
  const ctx = contextTokens(c, () => toApiMessages(c, sysPrompt(c)));
  const max = contextFor(model, c);
  const pct = max ? Math.min(100, Math.round((ctx.tokens / max) * 100)) : null;
  el.ctxMeter.hidden = false;
  el.ctxMeter.classList.toggle("warn", (pct ?? 0) >= 70);
  el.ctxMeter.classList.toggle("crit", (pct ?? 0) >= 90);
  el.ctxMeter.innerHTML = `<span class="bar"><i style="width:${pct ?? 0}%"></i></span>${
    pct !== null ? `${pct} %` : `${ctx.exact ? "" : "≈"}${fmtTokens(ctx.tokens)}`
  }`;
  el.ctxMeter.title =
    `Contexte : ${ctx.exact ? "" : "≈ "}${fmtTokens(ctx.tokens)} tokens` +
    (max ? ` sur ${fmtTokens(max)} (${pct} %), reste ${fmtTokens(Math.max(0, max - ctx.tokens))}` : " (fenêtre de contexte : défaut du serveur)") +
    (c.compaction ? `\nHistorique compacté le ${new Date(c.compaction.at).toLocaleString("fr-FR")}` : "") +
    (state.settings.autoCompactPercent ? `\nCompaction automatique à ${state.settings.autoCompactPercent} %` : "") +
    "\nCliquer pour compacter maintenant (résumé de l'historique)";
  const u = conversationUsage(c);
  const speed = fmtSpeed([...c.messages].reverse().find((m) => m.usage?.tokensPerSec)?.usage?.tokensPerSec);
  el.usageInfo.hidden = u.promptTokens + u.completionTokens === 0;
  el.usageInfo.textContent = `${fmtTokens(u.promptTokens + u.completionTokens)} tokens${speed ? ` · ${speed}` : ""}`;
  el.usageInfo.title =
    `Tokens de cette conversation\nEntrée : ${u.promptTokens.toLocaleString("fr-FR")}\nSortie : ${u.completionTokens.toLocaleString("fr-FR")}` +
    (speed ? `\nVitesse de génération (dernière réponse) : ${speed}` : "");
}

el.ctxMeter.addEventListener("click", async () => {
  const c = state.current;
  if (!c || currentRun() || !c.messages.length) return;
  const run = startRun(c, { phase: "", since: Date.now(), runStart: Date.now(), step: 0, maxSteps: 0 });
  updateComposerState();
  try {
    await compact(run);
    toast("Historique compacté");
  } catch (e) {
    toast(`Compaction impossible : ${errMsg(e)}`, true);
  } finally {
    endRun(run);
    if (viewing(c)) {
      updateComposerState();
      renderConversation();
    }
    renderSidebar();
  }
});

function recordUsage(conv: Conversation, model: string, promptTokens: number, completionTokens: number, extra: boolean) {
  api.recordUsage(today(), model, promptTokens, completionTokens).catch(() => {});
  if (extra) {
    conv.extraUsage ??= {};
    const u = (conv.extraUsage[model] ??= { promptTokens: 0, completionTokens: 0 });
    u.promptTokens += promptTokens;
    u.completionTokens += completionTokens;
  }
}

/** Résume l'historique pour libérer le contexte ; les messages restent affichés. */
async function compact(run: Run) {
  const conv = run.conv;
  setActivity(run, "Compaction du contexte…", "résumé de l'historique par le modèle");
  const messages: ApiMessage[] = toApiMessages(conv, sysPrompt(conv));
  if (messages[messages.length - 1]?.role === "tool") messages.push({ role: "assistant", content: "(Pause.)" });
  messages.push({ role: "user", content: COMPACT_PROMPT });
  let summary = "";
  let failed = "";
  // Les définitions d'outils accompagnent l'historique (qui contient des appels), mais sans appel possible.
  const tools = await api.toolDefinitions(toolCtx(conv)).catch(() => []);
  const requestId = uid();
  run.requestId = requestId;
  await api.chat(
    requestId,
    { model: conv.model, messages, temperature: 0.2, maxTokens: 4000, tools: tools.map((t) => t.definition), toolChoice: "none", ...chatOptions(conv, false) },
    (ev) => {
      if (ev.type === "delta") summary += ev.text;
      else if (ev.type === "usage") recordUsage(conv, conv.model, ev.promptTokens, ev.completionTokens, true);
      else if (ev.type === "retry") setActivity(run, "Compaction : nouvel essai dans", ev.reason, { until: Date.now() + ev.delaySecs * 1000 });
      else if (ev.type === "done" && ev.cancelled) failed = "annulée";
    },
  );
  if (failed || !summary.trim()) throw new Error(failed || "résumé vide");
  conv.compaction = { summary: summary.trim(), upto: conv.messages.length, at: Date.now() };
  await save(conv);
}

const shouldCompact = (conv: Conversation) => {
  const pctLimit = state.settings.autoCompactPercent;
  const max = contextFor(conv.model, conv);
  if (!pctLimit || !max || conv.messages.length < 4) return false;
  if (conv.compaction && conv.compaction.upto >= conv.messages.length - 1) return false;
  const ctx = contextTokens(conv, () => toApiMessages(conv, sysPrompt(conv)));
  return ctx.tokens >= (max * pctLimit) / 100;
};

// ---------------------------------------------------------------- boucle d'agent

/** Un tour du modèle en streaming. Renvoie false en cas d'erreur ou d'arrêt. */
async function streamTurn(run: Run, msg: Message, tools: Awaited<ReturnType<typeof api.toolDefinitions>>): Promise<boolean> {
  const conv = run.conv;
  const requestId = uid();
  Object.assign(run, { requestId, msgId: msg.id });
  const messages = toApiMessages({ ...conv, messages: conv.messages.filter((m) => m !== msg) }, sysPrompt(conv));
  setActivity(run, "En attente du modèle…", `${conv.model} · chargement et lecture de l'invite`);
  let ok = true;
  try {
    await api.chat(
      requestId,
      {
        model: conv.model,
        messages,
        temperature: conv.params?.temperature ?? state.settings.temperature,
        maxTokens: state.settings.maxTokens,
        tools: tools.map((t) => t.definition),
        ...chatOptions(conv),
      },
      (ev) => {
        switch (ev.type) {
          case "delta":
            msg.content += ev.text;
            setActivity(run, "Rédaction…", `≈ ${fmtTokens(Math.round(msg.content.length / 3.5))} tokens`);
            break;
          case "reasoning":
            msg.reasoning = (msg.reasoning ?? "") + ev.text;
            setActivity(run, "Réflexion…", `≈ ${fmtTokens(Math.round(msg.reasoning.length / 3.5))} tokens`);
            break;
          case "usage":
            msg.usage = { promptTokens: ev.promptTokens, completionTokens: ev.completionTokens, tokensPerSec: ev.tokensPerSec };
            recordUsage(conv, conv.model, ev.promptTokens, ev.completionTokens, false);
            break;
          case "toolCalls":
            msg.toolCalls = ev.calls.map((c) => ({ ...c, status: "queued" as const }));
            setActivity(run, "Préparation des actions…", `${ev.calls.length} appel(s) d'outil`);
            break;
          case "retry":
            setActivity(run, "Nouvel essai dans", `${ev.reason} · tentative ${ev.attempt + 1}/${ev.maxAttempts}`, {
              until: Date.now() + ev.delaySecs * 1000,
            });
            break;
          case "done":
            if (ev.cancelled) {
              msg.cancelled = true;
              ok = false;
            }
            if (ev.finishReason === "length") {
              msg.error = "Réponse tronquée (limite de tokens atteinte ou fenêtre de contexte pleine).";
              ok = false;
            }
            break;
        }
        rerenderMessage(msg);
        if (ev.type === "usage" && viewing(conv)) renderUsage();
      },
    );
  } catch (e) {
    msg.error = errMsg(e);
    ok = false;
  }
  if (!ok) {
    // Appels d'outils jamais exécutés : on les marque pour garder un historique valide.
    for (const tc of msg.toolCalls ?? []) {
      tc.status = "denied";
      tc.result = "Non exécuté (interrompu).";
    }
  }
  return ok && !run.stopped;
}

function waitApproval(run: Run, tc: ToolCallRec): Promise<{ decision: Decision; sudo?: SudoInput }> {
  return new Promise((resolve) => {
    run.approvals.set(tc.id, (decision, sudo) => {
      run.approvals.delete(tc.id);
      renderSidebar();
      resolve({ decision, sudo });
    });
    renderSidebar();
  });
}

/** Exécute les appels d'outils d'un message. Renvoie "ok", "denied" ou "stopped". */
async function runToolCalls(run: Run, msg: Message): Promise<"ok" | "denied" | "stopped"> {
  const conv = run.conv;
  const ctx = toolCtx(conv);
  const calls = msg.toolCalls ?? [];
  for (let i = 0; i < calls.length; i++) {
    const tc = calls[i];
    const skipRest = (reason: string) => {
      for (const rest of calls.slice(i)) {
        if (rest.status === "queued" || rest.status === "pending" || rest.status === "running") {
          rest.status = "denied";
          rest.result ??= reason;
        }
      }
      rerenderMessage(msg, true);
    };
    if (run.stopped) {
      skipRest("Non exécuté : arrêté par l'utilisateur.");
      return "stopped";
    }

    const risk = state.risks.get(tc.name) ?? guessRisk(tc.name);
    // Accès système : un chemin hors du projet demande une approbation renforcée.
    if (ctx.system && (risk === "read" || risk === "write")) {
      tc.outside = (await api.toolOutside(ctx, tc.name, tc.arguments).catch(() => null)) ?? undefined;
    }
    // sudo : approbation systématique (même en mode Autonome), avec le mot de passe.
    const sudo = usesSudo(tc) && (platformFor(conv).remote || state.platform.os !== "windows");
    let sudoPassword = sudo ? sudoCache.get(sudoTarget(conv)) : undefined;
    if (sudo || needsApproval(risk, tc.name, conv.permissionMode ?? "ask", state.settings, !!tc.outside)) {
      if (sudo) tc.sudo = sudoPassword === undefined ? "ask" : "cached";
      if (risk === "write") {
        try {
          tc.diff = (await api.previewTool(ctx, tc.name, tc.arguments)) ?? undefined;
        } catch (e) {
          // Modification impossible (texte introuvable…) : on renvoie l'erreur au modèle sans déranger l'utilisateur.
          tc.status = "error";
          tc.result = `Erreur : ${errMsg(e)}`;
          rerenderMessage(msg, true);
          continue;
        }
      }
      tc.status = "pending";
      rerenderMessage(msg, true);
      if (viewing(conv)) scrollBottom();
      setActivity(run, "En attente de votre approbation", toolLabel(tc));
      notifyAttention(conv);
      const { decision, sudo: typed } = await waitApproval(run, tc);
      // Le mot de passe a pu être oublié pendant l'attente (bouton « Oublier »).
      if (sudo) sudoPassword = typed?.password ?? sudoCache.get(sudoTarget(conv));
      if (typed?.keep && typed.password) sudoCache.set(sudoTarget(conv), typed.password);
      delete tc.sudo;
      if (decision === "deny") {
        tc.status = "denied";
        tc.result = "L'utilisateur a refusé cette action. Arrête-toi et demande-lui comment procéder.";
        skipRest("Non exécuté : une action précédente a été refusée.");
        return run.stopped ? "stopped" : "denied";
      }
      if (decision === "always") {
        if (risk === "mcp") {
          state.settings.mcpAlwaysAllow = [...new Set([...state.settings.mcpAlwaysAllow, tc.name])];
          api.saveSettings(state.settings).catch((e) => toast(errMsg(e), true));
        } else if (risk === "write" && !tc.outside) {
          conv.permissionMode = "acceptEdits";
          if (viewing(conv)) renderChrome();
        }
      }
    }

    tc.status = "running";
    rerenderMessage(msg, true);
    setActivity(run, "Exécution", toolLabel(tc));
    const requestId = uid();
    Object.assign(run, { requestId, msgId: msg.id });
    try {
      const out = await api.runTool(requestId, ctx, tc.name, tc.arguments, {
        sudoPassword,
        // Sortie de la commande au fil de l'eau (seule la fin est gardée pour l'affichage).
        onOutput: (text) => {
          tc.live = ((tc.live ?? "") + text).slice(-8000);
          rerenderMessage(msg);
        },
      });
      tc.result = out.content;
      tc.status = out.isError ? "error" : "done";
      if (out.diff) tc.diff = out.diff;
      if (out.undo) tc.undo = out.undo;
    } catch (e) {
      tc.result = `Erreur : ${errMsg(e)}`;
      tc.status = "error";
    }
    delete tc.live;
    rerenderMessage(msg, true);
  }
  await save(conv);
  if (viewing(conv)) {
    renderUsage();
    renderChanges();
    if (conv.kind === "code") scheduleGitRefresh();
  }
  return run.stopped ? "stopped" : "ok";
}

/** Approbation requise : prévient si la fenêtre n'a pas le focus ou si la conversation n'est pas affichée. */
function notifyAttention(conv: Conversation) {
  if (!viewing(conv)) toast(`« ${conv.title} » attend votre approbation`);
  if (document.hasFocus()) return;
  document.title = "⏸ Approbation requise — Ollama Desktop";
  window.addEventListener("focus", () => (document.title = "Ollama Desktop"), { once: true });
}

async function runAgent() {
  const conv = state.current;
  if (!conv || state.runs.has(conv.id)) return;
  conv.model = state.selectedModel;
  const maxSteps = conv.kind === "code" ? Math.max(5, state.settings.maxSteps || 100) : MAX_CHAT_STEPS;
  const runStart = Date.now();
  const run = startRun(conv, { phase: "Préparation…", since: runStart, runStart, step: 1, maxSteps });
  updateComposerState();
  renderSidebar();

  if (conv.projectId && findProject(conv.projectId)?.files.length) {
    // Gros projets : seuls les extraits pertinents pour la dernière question sont joints.
    const question = [...conv.messages].reverse().find((m) => m.role === "user" && !m.auto)?.content ?? "";
    setActivity(run, "Recherche dans les fichiers du projet…");
    try {
      state.projectCtx.set(conv.id, await api.projectContext(conv.projectId, question));
    } catch {
      state.projectCtx.delete(conv.id);
    }
  }
  if (conv.kind === "code" && conv.workspace) {
    // Relu à chaque tâche : l'agent ou l'utilisateur ont pu modifier AGENTS.md.
    setActivity(run, "Lecture des consignes du projet…", workspaceLabel(conv.workspace).name);
    await loadInstructions(conv.workspace);
  }

  let tools: Awaited<ReturnType<typeof api.toolDefinitions>> = [];
  try {
    tools = await api.toolDefinitions(toolCtx(conv));
    for (const t of tools) state.risks.set(t.definition.function.name, t.risk);
  } catch (e) {
    toast(`Outils indisponibles : ${errMsg(e)}`, true);
  }
  // Mode plan : outils en lecture seule uniquement.
  if (conv.kind === "code" && conv.permissionMode === "plan") tools = tools.filter((t) => t.risk === "read");
  if (tools.length && modelInfo(conv.model)?.tools === false) {
    // Ollama refuse la requête entière si le modèle ne gère pas les outils.
    tools = [];
    toast(`${conv.model} ne gère pas les outils : recherche web et MCP désactivés pour cette réponse.`);
  }

  if (viewing(conv)) renderConversation();
  await save(conv);
  let outcome: RunOutcome = { kind: "done", text: "", canContinue: false };
  let steps = 0;
  let nudges = 0;
  try {
    for (let step = 1; ; step++) {
      if (step > maxSteps) {
        outcome = { kind: "limit", text: `Limite de ${maxSteps} étapes atteinte (réglable dans Réglages → Code).`, canContinue: true };
        break;
      }
      steps = step;
      run.activity.step = step;
      if (shouldCompact(conv)) {
        try {
          await compact(run);
          if (viewing(conv)) renderConversation();
        } catch (e) {
          toast(`Compaction automatique impossible : ${errMsg(e)}`, true);
        }
      }
      const msg: Message = { id: uid(), role: "assistant", content: "", model: conv.model, createdAt: Date.now() };
      conv.messages.push(msg);
      run.msgId = msg.id;
      rerenderMessage(msg, true);
      const ok = await streamTurn(run, msg, tools);
      if (!msg.content && !msg.reasoning && !msg.toolCalls?.length && !msg.error && !msg.cancelled) {
        msg.error = "Réponse vide du modèle.";
      }
      rerenderMessage(msg, true);
      await save(conv);
      if (viewing(conv)) renderUsage();
      if (!ok) {
        outcome = run.stopped || msg.cancelled
          ? { kind: "stopped", text: "Arrêté à votre demande.", canContinue: true }
          : msg.error?.startsWith("Réponse tronquée")
            ? { kind: "truncated", text: "Réponse coupée : limite de tokens atteinte (réglages « Tokens max » ou « Fenêtre de contexte »).", canContinue: true }
            : { kind: "error", text: `Interrompu par une erreur : ${msg.error ?? "inconnue"}`, canContinue: true };
        break;
      }
      if (!msg.toolCalls?.length) {
        // Réponse finale… ou action annoncée sans être exécutée : relance automatique.
        if (conv.kind === "code" && state.settings.autoContinue && nudges < MAX_AUTO_CONTINUE && looksUnfinished(msg.content)) {
          nudges++;
          conv.messages.push({ id: uid(), role: "user", content: AUTO_CONTINUE_TEXT, createdAt: Date.now(), auto: true });
          if (viewing(conv)) renderConversation();
          continue;
        }
        break;
      }
      const res = await runToolCalls(run, msg);
      if (res === "denied") {
        outcome = { kind: "denied", text: "En pause : vous avez refusé une action. Donnez vos consignes pour continuer.", canContinue: false };
        break;
      }
      if (res === "stopped") {
        outcome = { kind: "stopped", text: "Arrêté à votre demande.", canContinue: true };
        break;
      }
    }
  } finally {
    const elapsed = fmtDuration(Date.now() - runStart);
    if (outcome.kind === "done") {
      outcome.plan = conv.kind === "code" && conv.permissionMode === "plan";
      const u = conversationUsage(conv);
      outcome.text = `Terminé en ${elapsed} · ${steps} étape${steps > 1 ? "s" : ""} · ${fmtTokens(u.promptTokens + u.completionTokens)} tokens dans la conversation`;
    } else {
      outcome.text += ` (${elapsed}, ${steps} étape${steps > 1 ? "s" : ""})`;
    }
    endRun(run);
    if (!deletedConvs.has(conv.id)) {
      state.lastRuns.set(conv.id, outcome);
      if (viewing(conv)) {
        const last = conv.messages[conv.messages.length - 1];
        if (last) rerenderMessage(last, true);
        updateComposerState();
        renderActivity();
        renderUsage();
      } else {
        // Terminée en arrière-plan : pastille dans la barre latérale et notification.
        state.unread.add(conv.id);
        toast(outcome.kind === "done" ? `« ${conv.title} » : réponse terminée` : `« ${conv.title} » : ${outcome.text}`, outcome.kind === "error");
      }
    }
    renderSidebar();
    await save(conv);
  }
}

// ---------------------------------------------------------------- fichiers modifiés par l'agent

interface ChangedFile {
  path: string;
  diffs: { diff: string; undone: boolean }[];
  added: number;
  removed: number;
}

/** Fichiers écrits par l'agent dans la conversation, avec leurs diffs successifs. */
function changedFiles(conv: Conversation | null): ChangedFile[] {
  const files = new Map<string, ChangedFile>();
  for (const m of conv?.messages ?? []) {
    for (const tc of m.toolCalls ?? []) {
      if (!tc.diff || tc.status !== "done" || (tc.name !== "write_file" && tc.name !== "edit_file")) continue;
      let path = "";
      try {
        path = String(JSON.parse(tc.arguments).path ?? "");
      } catch {
        continue;
      }
      const f = files.get(path) ?? { path, diffs: [], added: 0, removed: 0 };
      f.diffs.push({ diff: tc.diff, undone: !!tc.undone });
      if (!tc.undone) {
        for (const line of tc.diff.split("\n")) {
          if (line.startsWith("+") && !line.startsWith("+++")) f.added++;
          else if (line.startsWith("-") && !line.startsWith("---")) f.removed++;
        }
      }
      files.set(path, f);
    }
  }
  return [...files.values()];
}

let changesOpenPath = "";

function renderChanges() {
  const files = state.current?.kind === "code" ? changedFiles(state.current) : [];
  el.changesBtn.hidden = files.length === 0;
  el.changesBtn.querySelector("span")!.textContent = String(files.length);
  if (!files.length) el.changesPanel.hidden = true;
  if (el.changesPanel.hidden) return;
  $("#changes-list").innerHTML = files
    .map(
      (f) => `<div class="git-file${f.path === changesOpenPath ? " open" : ""}" data-path="${escapeHtml(f.path)}">
        <span class="git-name">${escapeHtml(f.path)}${f.diffs.every((d) => d.undone) ? " <small>annulé</small>" : f.diffs.length > 1 ? ` <small>${f.diffs.length} modifications</small>` : ""}</span>
        <span class="chg-stat"><b class="add">+${f.added}</b> <b class="del">−${f.removed}</b></span>
      </div>`,
    )
    .join("");
  const open = files.find((f) => f.path === changesOpenPath) ?? files[0];
  changesOpenPath = open.path;
  $("#changes-diff").innerHTML = `<div class="git-diff-head"><code>${escapeHtml(open.path)}</code></div>${open.diffs
    .map((d, i) => `${open.diffs.length > 1 ? `<div class="tool-label chg-step">Modification ${i + 1}${d.undone ? " (annulée)" : ""}</div>` : ""}${diffHtml(d.diff)}`)
    .join("")}`;
}

el.changesBtn.addEventListener("click", () => {
  el.changesPanel.hidden = !el.changesPanel.hidden;
  if (!el.changesPanel.hidden) toggleGitPanel(false);
  renderChanges();
});
$("#changes-close").addEventListener("click", () => (el.changesPanel.hidden = true));
$("#changes-list").addEventListener("click", (e) => {
  const path = (e.target as HTMLElement).closest<HTMLElement>("[data-path]")?.dataset.path;
  if (path === undefined) return;
  changesOpenPath = path;
  renderChanges();
});

/** Sort du mode plan et demande à l'agent d'exécuter le plan qu'il vient de proposer. */
function runPlan() {
  const conv = state.current;
  if (!conv || currentRun()) return;
  conv.permissionMode = "ask";
  renderChrome();
  el.input.value = "Exécute ce plan.";
  send();
}

/** Appels du dernier tour dont la modification de fichier peut encore être annulée. */
function undoable(conv: Conversation): ToolCallRec[] {
  const out: ToolCallRec[] = [];
  for (let i = conv.messages.length - 1; i >= 0 && conv.messages[i].role === "assistant"; i--) {
    for (const tc of [...(conv.messages[i].toolCalls ?? [])].reverse()) if (tc.undo && !tc.undone) out.push(tc);
  }
  return out; // du plus récent au plus ancien : ordre d'annulation
}

/** Annule les modifications de fichiers du dernier tour de l'agent (deux clics pour confirmer). */
async function undoTurn(btn: HTMLElement) {
  const conv = state.current;
  if (!conv || currentRun()) return;
  if (!btn.classList.contains("confirm")) {
    btn.classList.add("confirm");
    btn.dataset.label = btn.textContent ?? "";
    btn.textContent = "Confirmer l'annulation";
    setTimeout(() => {
      if (!btn.isConnected) return;
      btn.classList.remove("confirm");
      btn.textContent = btn.dataset.label ?? "";
    }, 3000);
    return;
  }
  const ctx = toolCtx(conv);
  let done = 0;
  try {
    for (const tc of undoable(conv)) {
      await api.restoreFile(ctx, tc.undo!.path, tc.undo!.before);
      tc.undone = true;
      delete tc.undo;
      // Le modèle voit, au tour suivant, que sa modification n'est plus en place.
      tc.result = `${tc.result ?? ""}\n[Modification annulée ensuite par l'utilisateur : le fichier est revenu à son état précédent.]`;
      done++;
    }
    toast(`${done} modification${done > 1 ? "s" : ""} annulée${done > 1 ? "s" : ""}`);
  } catch (e) {
    toast(`Annulation interrompue : ${errMsg(e)}`, true);
  }
  await saveCurrent();
  renderConversation();
  scheduleGitRefresh();
}

async function stopRun(run: Run) {
  run.stopped = true;
  for (const resolve of [...run.approvals.values()]) resolve("deny");
  if (run.requestId) await api.cancel(run.requestId).catch(() => {});
}

async function stop() {
  const run = currentRun();
  if (run) await stopRun(run);
}

function updateComposerState() {
  el.app.classList.toggle("is-streaming", !!currentRun());
  el.sendBtn.title = currentRun() ? "Arrêter (Échap)" : "Envoyer";
  el.hint.textContent = state.settings?.sendOnEnter ? "Entrée pour envoyer · Maj+Entrée pour un retour ligne" : "Ctrl+Entrée pour envoyer";
}

function autoGrow(ta: HTMLTextAreaElement) {
  ta.style.height = "auto";
  ta.style.height = `${Math.min(ta.scrollHeight, 320)}px`;
}

el.composer.addEventListener("submit", (e) => {
  e.preventDefault();
  send();
});
el.input.addEventListener("input", () => autoGrow(el.input));
el.input.addEventListener("keydown", (e) => {
  if (e.key !== "Enter" || e.isComposing) return;
  const mod = e.ctrlKey || e.metaKey;
  if ((state.settings.sendOnEnter && !e.shiftKey) || mod) {
    e.preventDefault();
    send();
  }
});
el.exportBtn.addEventListener("click", async () => {
  if (!state.current) return;
  try {
    if (await api.exportConversation(state.current.id)) toast("Conversation exportée");
  } catch (e) {
    toast(errMsg(e), true);
  }
});

// ---------------------------------------------------------------- pièces jointes

function renderAttachments() {
  const { images, files } = state.pending;
  el.attachments.innerHTML =
    images.map((src, i) => `<span class="att img"><img src="${src}" alt="" /><button type="button" data-rm="img:${i}" title="Retirer">×</button></span>`).join("") +
    files.map((f, i) => `<span class="att chip">${icons.file}${escapeHtml(f.name)}<button type="button" data-rm="file:${i}" title="Retirer">×</button></span>`).join("");
}

el.attachments.addEventListener("click", (e) => {
  const rm = (e.target as HTMLElement).dataset.rm;
  if (!rm) return;
  const [kind, i] = rm.split(":");
  if (kind === "img") state.pending.images.splice(+i, 1);
  else state.pending.files.splice(+i, 1);
  renderAttachments();
});

const readAsDataUrl = (f: Blob) =>
  new Promise<string>((res, rej) => {
    const r = new FileReader();
    r.onload = () => res(r.result as string);
    r.onerror = () => rej(r.error);
    r.readAsDataURL(f);
  });

/** Redimensionne localement les grandes images pour limiter le volume envoyé. */
async function imageToDataUrl(file: File): Promise<string> {
  const url = await readAsDataUrl(file);
  const img = new Image();
  await new Promise((res, rej) => {
    img.onload = res;
    img.onerror = rej;
    img.src = url;
  });
  const scale = Math.min(1, MAX_IMAGE_SIDE / Math.max(img.width, img.height));
  if (scale === 1 && file.size < 4 * 1024 * 1024) return url;
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(img.width * scale);
  canvas.height = Math.round(img.height * scale);
  canvas.getContext("2d")!.drawImage(img, 0, 0, canvas.width, canvas.height);
  return canvas.toDataURL("image/jpeg", 0.88);
}

async function addPdf(file: File) {
  if (file.size > MAX_PDF) throw new Error(`trop volumineux (max ${MAX_PDF / 1048576} Mo)`);
  const placeholder: TextFile = { name: `${file.name} (conversion…)`, text: "" };
  state.pending.files.push(placeholder);
  renderAttachments();
  try {
    const b64 = (await readAsDataUrl(file)).split(",")[1];
    const { text } = await api.processDocument(b64);
    if (!text.trim()) throw new Error("aucun texte extrait");
    placeholder.name = file.name;
    placeholder.text = text;
  } catch (e) {
    state.pending.files.splice(state.pending.files.indexOf(placeholder), 1);
    throw e;
  } finally {
    renderAttachments();
  }
}

async function addFiles(list: Iterable<File>) {
  for (const file of list) {
    try {
      if (/^image\/(png|jpe?g|webp|gif)$/.test(file.type)) {
        state.pending.images.push(await imageToDataUrl(file));
        renderAttachments();
        continue;
      }
      if (file.type === "application/pdf" || /\.pdf$/i.test(file.name)) {
        await addPdf(file);
        continue;
      }
      if (file.size > MAX_TEXT_FILE) {
        toast(`${file.name} : trop volumineux (max ${MAX_TEXT_FILE / 1024} Ko)`, true);
        continue;
      }
      const buf = new Uint8Array(await file.arrayBuffer());
      if (buf.subarray(0, 8000).includes(0)) {
        toast(`${file.name} : format non pris en charge (images, PDF et fichiers texte uniquement)`, true);
        continue;
      }
      state.pending.files.push({ name: file.name, text: new TextDecoder().decode(buf) });
      renderAttachments();
    } catch (e) {
      toast(`${file.name} : ${errMsg(e)}`, true);
    }
  }
  el.input.focus();
}

$("#attach-btn").addEventListener("click", () => el.fileInput.click());
el.fileInput.addEventListener("change", async () => {
  await addFiles(el.fileInput.files ?? []);
  el.fileInput.value = "";
});
el.input.addEventListener("paste", (e) => {
  const files = [...(e.clipboardData?.files ?? [])];
  if (files.length) {
    e.preventDefault();
    addFiles(files);
  }
});

let dragDepth = 0;
const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes("Files");
window.addEventListener("dragenter", (e) => {
  if (!hasFiles(e)) return;
  dragDepth++;
  el.drop.hidden = false;
});
window.addEventListener("dragleave", () => {
  if (--dragDepth <= 0) {
    dragDepth = 0;
    el.drop.hidden = true;
  }
});
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("drop", (e) => {
  e.preventDefault();
  dragDepth = 0;
  el.drop.hidden = true;
  if (e.dataTransfer?.files.length) addFiles(e.dataTransfer.files);
});

// ---------------------------------------------------------------- réglages

const s = {
  model: $<HTMLSelectElement>("#s-model"),
  system: $<HTMLTextAreaElement>("#s-system"),
  temp: $<HTMLInputElement>("#s-temp"),
  max: $<HTMLInputElement>("#s-max"),
  theme: $<HTMLSelectElement>("#s-theme"),
  enter: $<HTMLInputElement>("#s-enter"),
  ctx: $<HTMLSelectElement>("#s-ctx"),
  keep: $<HTMLInputElement>("#s-keep"),
  thinking: $<HTMLInputElement>("#s-thinking"),
  searx: $<HTMLInputElement>("#s-searx"),
  webDefault: $<HTMLInputElement>("#s-web-default"),
  codeModel: $<HTMLSelectElement>("#s-code-model"),
  timeout: $<HTMLInputElement>("#s-timeout"),
  mcp: $<HTMLTextAreaElement>("#s-mcp"),
  mcpStatus: $("#mcp-status"),
  mcpAllowed: $("#mcp-allowed"),
  wsList: $("#ws-list"),
  error: $("#settings-error"),
  updates: $<HTMLInputElement>("#s-updates"),
  updateStatus: $("#update-status"),
  maxSteps: $<HTMLInputElement>("#s-max-steps"),
  compact: $<HTMLInputElement>("#s-compact"),
  autoContinue: $<HTMLInputElement>("#s-auto-continue"),
  usagePeriod: $<HTMLSelectElement>("#usage-period"),
  usageTotal: $("#usage-total"),
  usageTable: $("#usage-table"),
  shell: $<HTMLSelectElement>("#s-shell"),
  shellWrap: $("#s-shell-wrap"),
  shellInfo: $("#s-shell-info"),
  sttModel: $<HTMLInputElement>("#s-stt-model"),
  sttLang: $<HTMLSelectElement>("#s-stt-lang"),
  sttUrl: $<HTMLInputElement>("#s-stt-url"),
  sttVocab: $<HTMLTextAreaElement>("#s-stt-vocab"),
  encrypt: $<HTMLInputElement>("#s-encrypt"),
  vaultStatus: $("#vault-status"),
  vaultKeyRow: $("#vault-key-row"),
  vaultKey: $<HTMLInputElement>("#vault-key"),
  vaultKeyAction: $<HTMLButtonElement>("#vault-key-action"),
};

function renderShellInfo() {
  const p = state.platform;
  s.shellWrap.hidden = p.os !== "windows";
  s.shellInfo.innerHTML = `Shell utilisé par l'agent : <strong>${escapeHtml(p.shell)}</strong>${p.shellPath ? ` — <code>${escapeHtml(p.shellPath)}</code>` : ""}${
    p.os === "windows" && !/bash/i.test(p.shell)
      ? '<br>Conseil : installez <a href="https://git-scm.com/download/win" data-external>Git pour Windows</a> pour que l\'agent dispose de bash, la syntaxe que les modèles maîtrisent le mieux.'
      : ""
  }`;
}

async function renderUsageTab() {
  const log = await api.getUsage().catch(() => ({}));
  const { rows, total } = aggregate(log, Number(s.usagePeriod.value));
  s.usageTotal.textContent = rows.length ? `Total : ${fmtTokens(total)} tokens` : "";
  s.usageTable.innerHTML = rows.length
    ? `<table class="usage"><thead><tr><th>Modèle</th><th>Requêtes</th><th>Entrée</th><th>Sortie</th></tr></thead><tbody>${rows
        .map((r) => `<tr><td><code>${escapeHtml(r.model)}</code></td><td>${r.requests}</td><td>${fmtTokens(r.prompt)}</td><td>${fmtTokens(r.completion)}</td></tr>`)
        .join("")}</tbody></table>`
    : '<p class="muted small">Aucune consommation enregistrée sur cette période.</p>';
}
s.usagePeriod.addEventListener("change", renderUsageTab);

// ---------------------------------------------------------------- mises à jour

/** Installation automatique possible (build signé, type d'installation compatible). */
let updaterSupported = false;
let installing = false;

function renderUpdateBanner() {
  const u = state.update;
  if (!u?.available || u.latest === state.settings.skippedVersion) {
    el.updateBanner.hidden = true;
    return;
  }
  el.updateBanner.innerHTML = `<span>🎉 <strong>Ollama Desktop ${escapeHtml(u.latest)}</strong> est disponible (vous avez ${escapeHtml(u.current)}).</span>
    <span class="row">
      ${updaterSupported
        ? `<button type="button" class="btn-primary sm" data-upd="install">Installer et redémarrer</button>`
        : `<button type="button" class="btn-primary sm" data-upd="download">${u.assetName ? `Télécharger ${escapeHtml(u.assetName.replace(/^.*\./, "."))}` : "Télécharger"}</button>`}
      <button type="button" class="btn-ghost sm" data-upd="notes">Nouveautés</button>
      <button type="button" class="btn-ghost sm" data-upd="skip">Ignorer cette version</button>
      <button type="button" class="icon-btn sm" data-upd="close" title="Fermer">×</button>
    </span>`;
  el.updateBanner.hidden = false;
}

el.updateBanner.addEventListener("click", (e) => {
  const action = (e.target as HTMLElement).closest<HTMLElement>("[data-upd]")?.dataset.upd;
  const u = state.update;
  if (!action || !u) return;
  if (action === "download") openUrl(u.assetUrl ?? u.releaseUrl).catch((err) => toast(errMsg(err), true));
  if (action === "install") installUpdate();
  if (action === "notes") openUrl(u.releaseUrl).catch((err) => toast(errMsg(err), true));
  if (action === "skip") {
    state.settings.skippedVersion = u.latest;
    api.saveSettings(state.settings).catch(() => {});
  }
  if (action === "skip" || action === "close") el.updateBanner.hidden = true;
});

/** Télécharge, vérifie la signature, installe puis redémarre (géré côté Rust). */
async function installUpdate() {
  if (installing) return;
  if (state.runs.size) {
    toast("Attendez la fin des tâches en cours avant d'installer la mise à jour.", true);
    return;
  }
  installing = true;
  const btn = el.updateBanner.querySelector<HTMLButtonElement>('[data-upd="install"]');
  if (btn) btn.disabled = true;
  try {
    await api.installUpdate(({ done, total }) => {
      if (btn) btn.textContent = total ? `Téléchargement ${Math.round((done / total) * 100)} %` : `Téléchargement ${(done / 1048576).toFixed(1)} Mo`;
    });
  } catch (e) {
    toast(errMsg(e), true);
    if (btn) {
      btn.disabled = false;
      btn.textContent = "Installer et redémarrer";
    }
  } finally {
    installing = false;
  }
}

async function checkUpdate(manual = false) {
  try {
    state.update = await api.checkUpdate();
    const u = state.update;
    s.updateStatus.textContent = u.available
      ? `Version ${u.latest} disponible (installée : ${u.current}).`
      : `À jour (version ${u.current}).`;
    if (manual && u.available && u.latest === state.settings.skippedVersion) {
      state.settings.skippedVersion = "";
    }
    renderUpdateBanner();
  } catch (e) {
    s.updateStatus.textContent = `Vérification impossible : ${errMsg(e)}`;
    if (manual) toast(`Vérification impossible : ${errMsg(e)}`, true);
  }
}
$("#check-update").addEventListener("click", () => checkUpdate(true));

const MCP_EXAMPLE = `{
  "mcpServers": {
    "fichiers": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/chemin/vers/dossier"]
    },
    "serveur-distant": {
      "url": "https://mcp.example.org/mcp",
      "headers": { "Authorization": "Bearer VOTRE_JETON" },
      "disabled": true
    }
  }
}
`;

function showTab(tab: string) {
  document.querySelectorAll<HTMLElement>(".settings-tabs button").forEach((b) => b.classList.toggle("active", b.dataset.tab === tab));
  document.querySelectorAll<HTMLElement>("#settings .tab").forEach((t) => (t.hidden = t.dataset.tab !== tab));
}
document.querySelector(".settings-tabs")!.addEventListener("click", (e) => {
  const b = (e.target as HTMLElement).closest<HTMLElement>("[data-tab]");
  if (b) showTab(b.dataset.tab!);
});

const STATE_LABEL = { starting: "démarrage…", ready: "connecté", error: "erreur", disabled: "désactivé" };

function renderMcpStatus() {
  s.mcpStatus.innerHTML = state.mcp
    .map(
      (m) => `<div class="mcp-server" data-state="${m.state}">
        <div class="row"><span class="dot"></span><strong>${escapeHtml(m.name)}</strong>
          <span class="muted small">${m.transport}${m.protocol ? ` · ${escapeHtml(m.protocol)}` : ""} · ${STATE_LABEL[m.state]}${m.state === "ready" ? ` · ${m.tools.length} outils` : ""}</span></div>
        ${m.error ? `<pre class="small">${escapeHtml(m.error)}</pre>` : ""}
        ${m.tools.length ? `<details><summary class="small">Outils</summary><ul class="small">${m.tools.map((t) => `<li><code>${escapeHtml(t.name)}</code> ${escapeHtml(t.description.slice(0, 140))}</li>`).join("")}</ul></details>` : ""}
      </div>`,
    )
    .join("");
  const allowed = state.settings.mcpAlwaysAllow;
  s.mcpAllowed.innerHTML = allowed.length
    ? allowed.map((n) => `<span class="chip">${escapeHtml(n)}<button type="button" data-unallow="${escapeHtml(n)}" title="Retirer">×</button></span>`).join("")
    : '<p class="muted small">Aucun. Utilisez « Toujours autoriser » lors d\'une demande d\'approbation.</p>';
}

s.mcpAllowed.addEventListener("click", (e) => {
  const n = (e.target as HTMLElement).dataset.unallow;
  if (!n) return;
  state.settings.mcpAlwaysAllow = state.settings.mcpAlwaysAllow.filter((x) => x !== n);
  api.saveSettings(state.settings).catch((err) => toast(errMsg(err), true));
  renderMcpStatus();
});

let mcpPoll = 0;
async function pollMcp() {
  clearTimeout(mcpPoll);
  try {
    state.mcp = await api.mcpStatus();
  } catch {
    return;
  }
  if (el.settings.open) renderMcpStatus();
  renderChrome();
  if (state.mcp.some((m) => m.state === "starting")) mcpPoll = window.setTimeout(pollMcp, 800);
}

$("#mcp-apply").addEventListener("click", async () => {
  s.error.hidden = true;
  try {
    await api.mcpSaveConfig(s.mcp.value);
    toast("Configuration MCP appliquée");
    pollMcp();
  } catch (e) {
    s.error.textContent = errMsg(e);
    s.error.hidden = false;
  }
});
$("#mcp-example").addEventListener("click", () => {
  s.mcp.value = MCP_EXAMPLE;
});

function renderWsList() {
  s.wsList.innerHTML = state.workspaces.length
    ? state.workspaces.map((w) => `<div class="ws-row"><code>${escapeHtml(w)}</code><button type="button" class="btn-ghost sm danger" data-forget="${escapeHtml(w)}">Retirer</button></div>`).join("")
    : '<p class="muted small">Aucun dossier.</p>';
}
s.wsList.addEventListener("click", async (e) => {
  const w = (e.target as HTMLElement).dataset.forget;
  if (!w) return;
  state.workspaces = await api.forgetWorkspace(w);
  if (state.draft.workspace === w) state.draft.workspace = undefined;
  renderWsList();
});


async function renderVaultStatus() {
  const v = await api.vaultStatus().catch(() => null);
  if (!v) return;
  s.vaultStatus.className = v.error ? "small error" : "muted small";
  s.vaultStatus.textContent = v.error
    ? `⚠ Chiffrement inactif : ${v.error}`
    : v.enabled
      ? "✓ Données chiffrées sur cet appareil."
      : v.hasKey
        ? "Chiffrement désactivé (les données sont en clair sur le disque)."
        : "Chiffrement désactivé.";
}

let vaultMode: "copy" | "restore" = "copy";
$("#vault-show").addEventListener("click", async () => {
  try {
    s.vaultKey.value = await api.vaultExportKey();
    s.vaultKey.readOnly = true;
    vaultMode = "copy";
    s.vaultKeyAction.textContent = "Copier";
    s.vaultKeyRow.hidden = false;
    s.vaultKey.select();
  } catch (e) {
    toast(errMsg(e), true);
  }
});
$("#vault-restore").addEventListener("click", () => {
  s.vaultKey.value = "";
  s.vaultKey.readOnly = false;
  s.vaultKey.placeholder = "Collez la clé de récupération";
  vaultMode = "restore";
  s.vaultKeyAction.textContent = "Restaurer";
  s.vaultKeyRow.hidden = false;
  s.vaultKey.focus();
});
s.vaultKeyAction.addEventListener("click", async () => {
  if (vaultMode === "copy") return copy(s.vaultKey.value, s.vaultKeyAction);
  try {
    await api.vaultImportKey(s.vaultKey.value);
    s.vaultKeyRow.hidden = true;
    toast("Clé restaurée");
    await refreshConvs();
    await loadProjects();
    renderSidebar();
    renderVaultStatus();
  } catch (e) {
    toast(errMsg(e), true);
  }
});

async function openSettings(tab = "general") {
  const st = state.settings;
  renderServers();
  fillModelSelect(s.model, st.defaultModel, "Automatique (premier modèle installé)");
  fillModelSelect(s.codeModel, st.codeModel, "Automatique (premier modèle gérant les outils)");
  s.ctx.value = String(st.numCtx ?? 16384);
  if (s.ctx.value !== String(st.numCtx)) {
    s.ctx.insertAdjacentHTML("beforeend", `<option value="${st.numCtx}">${fmtTokens(st.numCtx)}</option>`);
    s.ctx.value = String(st.numCtx);
  }
  s.keep.value = st.keepAlive ?? "";
  s.thinking.checked = st.thinking !== false;
  s.system.value = st.systemPrompt;
  s.temp.value = st.temperature?.toString() ?? "";
  s.max.value = st.maxTokens?.toString() ?? "";
  s.theme.value = st.theme;
  s.enter.checked = st.sendOnEnter;
  s.searx.value = st.searxngUrl;
  s.webDefault.checked = st.webSearchDefault;
  s.timeout.value = String(st.commandTimeout);
  s.maxSteps.value = String(st.maxSteps);
  s.compact.value = String(st.autoCompactPercent);
  s.autoContinue.checked = st.autoContinue;
  s.updates.checked = st.checkUpdates;
  s.shell.value = st.shell ?? "auto";
  s.sttModel.value = st.transcriptionModel ?? "";
  s.sttLang.value = st.transcriptionLanguage ?? "fr";
  s.sttUrl.value = st.transcriptionUrl ?? "";
  s.sttVocab.value = st.transcriptionVocabulary ?? "";
  s.encrypt.checked = st.encryptData !== false;
  s.vaultKeyRow.hidden = true;
  renderVaultStatus();
  renderShellInfo();
  if (!s.updateStatus.textContent) s.updateStatus.textContent = `Version installée : ${state.platform.version}`;
  renderUsageTab();
  s.mcp.value = await api.mcpGetConfig().catch(() => "");
  s.error.hidden = true;
  $("#data-dir").textContent = await api.dataDir().catch(() => "?");
  state.workspaces = await api.listWorkspaces().catch(() => state.workspaces);
  renderWsList();
  renderMcpStatus();
  showTab(tab);
  el.settings.showModal();
  pollMcp();
}

$("#settings-cancel").addEventListener("click", () => el.settings.close());
s.theme.addEventListener("change", () => applyTheme(s.theme.value as Settings["theme"]));
el.settings.addEventListener("close", () => applyTheme(state.settings.theme));

$("#settings-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const num = (v: string) => (v.trim() === "" ? null : Number(v));
  if (!commitServerForm()) return showTab("general");
  const next: Settings = {
    ...state.settings,
    ...serversDraft(),
    defaultModel: s.model.value,
    systemPrompt: s.system.value,
    temperature: num(s.temp.value),
    maxTokens: num(s.max.value),
    numCtx: Number(s.ctx.value) || 0,
    keepAlive: s.keep.value.trim(),
    thinking: s.thinking.checked,
    theme: s.theme.value as Settings["theme"],
    sendOnEnter: s.enter.checked,
    searxngUrl: s.searx.value.trim(),
    webSearchDefault: s.webDefault.checked,
    codeModel: s.codeModel.value,
    commandTimeout: Math.max(5, Number(s.timeout.value) || 120),
    maxSteps: Math.max(5, Number(s.maxSteps.value) || 100),
    autoCompactPercent: Math.min(95, Math.max(0, Number(s.compact.value) || 0)),
    autoContinue: s.autoContinue.checked,
    checkUpdates: s.updates.checked,
    shell: s.shell.value as Settings["shell"],
    transcriptionModel: s.sttModel.value.trim(),
    transcriptionLanguage: s.sttLang.value,
    transcriptionUrl: s.sttUrl.value.trim(),
    transcriptionVocabulary: s.sttVocab.value,
    encryptData: s.encrypt.checked,
  };
  if (next.searxngUrl && !/^https?:\/\//.test(next.searxngUrl)) {
    s.error.textContent = "L'URL de l'instance SearXNG doit commencer par http:// ou https:// (onglet Outils).";
    s.error.hidden = false;
    return showTab("tools");
  }
  if (next.webSearchDefault && !next.searxngUrl) {
    s.error.textContent = "La recherche web par défaut nécessite une instance SearXNG (onglet Outils).";
    s.error.hidden = false;
    return showTab("tools");
  }
  try {
    const serverKey = (x: Settings) => JSON.stringify([x.servers, x.activeServer]);
    const baseChanged = serverKey(next) !== serverKey(state.settings);
    await api.saveSettings(next);
    if (next.encryptData !== state.settings.encryptData || next.encryptData) {
      const v = await api.vaultStatus().catch(() => null);
      if (v?.error && next.encryptData) toast(`Chiffrement inactif : ${v.error}`, true);
      else if (next.encryptData !== state.settings.encryptData) toast(next.encryptData ? "Données chiffrées" : "Chiffrement désactivé : données déchiffrées");
    }
    const shellChanged = next.shell !== state.settings.shell;
    state.settings = next;
    if (shellChanged) state.platform = await api.platform().catch(() => state.platform);
    applyTheme(next.theme);
    if (!state.current) {
      state.selectedModel = defaultModelFor(state.mode);
      state.draft.webSearch = next.webSearchDefault;
    }
    el.settings.close();
    if (baseChanged) {
      state.server = null;
      state.models = [];
      state.modelsLoaded = false;
      await loadModels();
    }
    renderServerChip();
    renderChrome();
    if (!state.current) renderWelcome();
  } catch (err) {
    s.error.textContent = errMsg(err);
    s.error.hidden = false;
  }
});

$("#open-settings").addEventListener("click", () => openSettings());
$("#models-btn").addEventListener("click", () => openModelsDialog());
$("#new-chat").addEventListener("click", newChat);
$("#toggle-sidebar").addEventListener("click", () => el.app.classList.toggle("sidebar-hidden"));

// Liens : toujours dans le navigateur du système, jamais dans la WebView.
document.addEventListener("click", (e) => {
  const a = (e.target as HTMLElement).closest("a");
  if (!a) return;
  e.preventDefault();
  const href = a.getAttribute("href") ?? "";
  if (/^(https?:|mailto:)/i.test(href)) openUrl(href).catch((err) => toast(errMsg(err), true));
});

window.addEventListener("keydown", (e) => {
  const mod = e.ctrlKey || e.metaKey;
  const k = e.key.toLowerCase();
  if (mod && k === "n") {
    e.preventDefault();
    newChat();
  } else if (mod && e.key === ",") {
    e.preventDefault();
    openSettings();
  } else if (mod && k === "b") {
    e.preventDefault();
    el.app.classList.toggle("sidebar-hidden");
  } else if (mod && k === "k") {
    e.preventDefault();
    openPalette();
  } else if (mod && k === "g" && opts().kind === "code" && opts().workspace) {
    e.preventDefault();
    toggleGitPanel();
  } else if (mod && (e.key === "1" || e.key === "2")) {
    e.preventDefault();
    setMode(e.key === "1" ? "chat" : "code");
  } else if (mod && k === "m") {
    e.preventDefault();
    openModelsDialog();
  } else if (mod && e.shiftKey && k === "d") {
    e.preventDefault();
    toggleDictation();
  } else if (e.key === "Escape" && !el.wsMenu.hidden) {
    e.preventDefault();
    toggleWsMenu(false);
  } else if (e.key === "Escape" && document.querySelector("dialog[open]")) {
    // WebKitGTK ne ferme pas toujours le <dialog> natif avec Échap.
    e.preventDefault();
    document.querySelector<HTMLDialogElement>("dialog[open]")!.close();
  } else if (e.key === "Escape" && cancelDictation()) {
    e.preventDefault();
  } else if (e.key === "Escape" && currentRun()) {
    stop();
  }
});

// ---------------------------------------------------------------- palette de commandes

function setTheme(theme: Settings["theme"]) {
  state.settings.theme = theme;
  applyTheme(theme);
  api.saveSettings(state.settings).catch((e) => toast(errMsg(e), true));
}

function paletteItems(q: string): PaletteItem[] {
  const o = opts();
  const actions: PaletteItem[] = [
    { group: "Actions", label: "Nouvelle conversation", hint: "Ctrl+N", keywords: "chat discuter", run: () => { state.mode = "chat"; newChat(); } },
    { group: "Actions", label: "Nouvelle session de code", hint: "Ctrl+2", keywords: "agent projet dossier", run: () => { state.mode = "code"; newChat(); } },
    { group: "Actions", label: "Gérer les modèles", hint: "Ctrl+M", keywords: "télécharger installer supprimer décharger pull", run: () => openModelsDialog() },
    { group: "Actions", label: "Réglages", hint: "Ctrl+,", keywords: "préférences configuration serveur", run: () => openSettings() },
    { group: "Actions", label: "Serveurs SSH…", keywords: "distant remote connexion clé", run: () => openSshDialog() },
    { group: "Actions", label: "Nouveau projet…", keywords: "consignes fichiers", run: () => newProject() },
    { group: "Actions", label: "Assistants…", keywords: "persona rôle consignes préréglage", run: () => openAssistantsDialog() },
    { group: "Actions", label: "Afficher ou masquer la barre latérale", hint: "Ctrl+B", run: () => el.app.classList.toggle("sidebar-hidden") },
    { group: "Actions", label: "Thème clair", keywords: "apparence light", run: () => setTheme("light") },
    { group: "Actions", label: "Thème sombre", keywords: "apparence dark nuit", run: () => setTheme("dark") },
    { group: "Actions", label: "Thème du système", keywords: "apparence automatique", run: () => setTheme("system") },
  ];
  if (state.current) {
    actions.push(
      { group: "Actions", label: "Réglages de cette conversation", keywords: "température contexte consignes raisonnement", run: () => el.convSettingsBtn.click() },
      { group: "Actions", label: "Exporter la conversation en Markdown", run: () => el.exportBtn.click() },
      { group: "Actions", label: "Compacter le contexte", keywords: "résumer historique", run: () => el.ctxMeter.click() },
    );
  }
  if (o.kind === "code" && o.workspace) actions.push({ group: "Actions", label: "Panneau Git", hint: "Ctrl+G", keywords: "commit branche diff", run: () => toggleGitPanel() });

  const items = actions.filter((a) => matches(q, `${a.label} ${a.keywords ?? ""}`));
  const convs = state.convs.filter((c) => matches(q, c.title)).slice(0, q ? 8 : 6);
  for (const c of convs) {
    items.push({
      group: q ? "Conversations" : "Conversations récentes",
      label: c.title,
      hint: [c.kind === "code" ? "Code" : "", state.runs.has(c.id) ? "en cours" : dayBucket(c.updatedAt)].filter(Boolean).join(" · "),
      run: () => openConversation(c.id),
    });
  }
  for (const v of state.settings.servers.filter((v) => v.id !== state.settings.activeServer && matches(q, `serveur ${v.name}`))) {
    if (q) items.push({ group: "Serveurs", label: `Serveur : ${v.name}`, hint: "basculer", run: () => applyServers(state.settings.servers, v.id).catch((e) => toast(errMsg(e), true)) });
  }
  for (const a of assistants().filter((a) => matches(q, `assistant ${a.name} ${a.description}`))) {
    if (q) items.push({ group: "Assistants", label: a.name, hint: a.model || "modèle par défaut", run: () => useAssistant(a) });
  }
  if (q) {
    for (const m of state.models.filter((m) => matches(q, m.id)).slice(0, 6)) {
      items.push({ group: "Modèles", label: m.id, hint: "utiliser pour cette conversation", run: () => pickModel(m.id) });
    }
    for (const p of projectList().filter((p) => matches(q, p.name)).slice(0, 5)) {
      items.push({
        group: "Projets",
        label: p.name,
        hint: "afficher le projet",
        run: () => {
          el.projectSelect.value = p.id;
          el.projectSelect.dispatchEvent(new Event("change"));
        },
      });
    }
  }
  return items;
}

/** Conversations dont le contenu (et pas seulement le titre) correspond à la requête. */
async function paletteSearch(q: string): Promise<PaletteItem[]> {
  const hits = await api.searchConversations(q.toLowerCase());
  return hits.slice(0, 8).flatMap((h) => {
    const c = state.convs.find((x) => x.id === h.id);
    return c ? [{ group: "Conversations", label: c.title, hint: h.snippet ?? undefined, run: () => openConversation(c.id) }] : [];
  });
}

// ---------------------------------------------------------------- démarrage

async function init() {
  try {
    state.settings = await api.getSettings();
  } catch (e) {
    toast(errMsg(e), true);
    return;
  }
  applyTheme(state.settings.theme);
  state.platform = await api.platform().catch(() => state.platform);
  state.workspaces = await api.listWorkspaces().catch(() => []);
  await loadHosts();
  await loadProjects();
  initSshUi({ onOpen: openRemoteWorkspace, toast });
  initProjectsUi({
    onChange: onProjectChange,
    toast,
    onWorkspaceAdded: () => api.listWorkspaces().then((w) => (state.workspaces = w)).catch(() => {}),
  });
  initDictation({ input: el.input, toast });
  initPopover();
  initModelPicker({
    button: el.modelBtn,
    models: () => state.models,
    loaded: () => state.loaded,
    setLoaded: (names) => (state.loaded = names),
    selected: () => state.selectedModel,
    kind: () => opts().kind,
    pick: pickModel,
    manage: () => openModelsDialog(),
    assistants,
    pickAssistant: useAssistant,
    manageAssistants: () => openAssistantsDialog(),
    assistant: () => opts().assistant,
  });
  await loadAssistants();
  initAssistantsUi({
    toast,
    models: () => state.models,
    changed: () => {
      if (!state.current) renderWelcome();
    },
    use: useAssistant,
  });
  initConvSettings({
    button: el.convSettingsBtn,
    settings: () => state.settings,
    get: () => {
      const o = opts();
      const model = state.selectedModel;
      return { systemPrompt: o.systemPrompt, params: o.params, thinkingModel: !!modelInfo(model)?.thinking, maxContext: modelInfo(model)?.maxContextLength ?? null };
    },
    set: (systemPrompt, params) => {
      if (state.current) Object.assign(state.current, { systemPrompt, params });
      else Object.assign(state.draft, { systemPrompt: systemPrompt ?? undefined, params });
      if (state.current) saveCurrent();
      renderChrome();
    },
  });
  initPalette({ items: paletteItems, search: paletteSearch });
  initServersUi({
    toast,
    settings: () => state.settings,
    status: () => state.server,
    running: () => state.running,
    modelCount: () => state.models.length,
    apply: async (servers, active) => {
      await applyServers(servers, active);
      // Réglages ouverts : les listes de modèles suivent le serveur testé.
      if (el.settings.open) {
        fillModelSelect(s.model, s.model.value, "Automatique (premier modèle installé)");
        fillModelSelect(s.codeModel, s.codeModel.value, "Automatique (premier modèle gérant les outils)");
      }
    },
    openSettings: () => openSettings(),
    openModels: () => openModelsDialog(),
    openSsh: () => openSshDialog(),
    refreshRunning,
  });
  initModelsUi({
    toast,
    models: () => state.models,
    reload: loadModels,
    numCtx: contextFor,
    use: pickModel,
  });
  initGitUi({
    workspace: () => (opts().kind === "code" ? opts().workspace : undefined),
    model: () => state.selectedModel || defaultModelFor("code"),
    chatOptions: (model, think) => chatOptions({ model }, think),
    toast,
    recordUsage: (model, p, c) => {
      api.recordUsage(today(), model, p, c).catch(() => {});
      if (state.current) {
        state.current.extraUsage ??= {};
        const u = (state.current.extraUsage[model] ??= { promptTokens: 0, completionTokens: 0 });
        u.promptTokens += p;
        u.completionTokens += c;
        renderUsage();
      }
    },
  });
  newChat();
  await refreshConvs().catch((e) => toast(errMsg(e), true));
  pollMcp();
  el.input.focus();
  await loadModels();
  // Ollama lancé après l'application : nouvelle tentative régulière tant qu'il est injoignable.
  window.setInterval(() => {
    if (document.hidden) return;
    if (!connected() && !state.runs.size) loadModels();
    else refreshRunning();
  }, 15000);
  renderUsage();
  updaterSupported = await api.updaterSupported().catch(() => false);
  if (state.settings.checkUpdates) {
    checkUpdate();
    window.setInterval(() => state.settings.checkUpdates && checkUpdate(), 6 * 3600 * 1000);
  }
}

init();
