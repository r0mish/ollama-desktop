// Panneau Git du mode Code : état, diff, indexation, commit, synchronisation, branches.
import { api, type GitFile, type GitStatus } from "./api";
import { diffHtml } from "./agent";
import { escapeHtml } from "./markdown";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;
const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

interface Deps {
  workspace: () => string | undefined;
  model: () => string;
  toast: (msg: string, error?: boolean) => void;
  /** Consommation des requêtes de génération de message. */
  recordUsage: (model: string, prompt: number, completion: number) => void;
  /** Options Ollama pour ce modèle (fenêtre de contexte, raisonnement). */
  chatOptions: (model: string, think?: boolean) => { numCtx: number | null; think: boolean | null };
}

let deps: Deps;
let status: GitStatus | null = null;
let busy = false;
let lastWs = "";
let refreshTimer = 0;

const panel = () => $("#git-panel");
export const isGitPanelOpen = () => !panel().hidden;

const LABEL: Record<string, string> = { M: "modifié", A: "ajouté", D: "supprimé", R: "renommé", C: "copié", U: "conflit", "?": "nouveau", T: "type modifié" };

const staged = (f: GitFile) => f.index !== " " && f.index !== "?";
const unstaged = (f: GitFile) => f.worktree !== " " || f.index === "?";
const untracked = (f: GitFile) => f.index === "?";

/** Badge de la barre du haut : branche et nombre de fichiers modifiés. */
function renderBadge() {
  const btn = $<HTMLButtonElement>("#git-btn");
  const ws = deps.workspace();
  btn.hidden = !ws;
  const span = btn.querySelector("span")!;
  if (!status) {
    span.textContent = "git";
  } else if (!status.isRepo) {
    span.textContent = "pas de dépôt";
  } else {
    const n = status.files.length;
    const sync = `${status.ahead ? ` ↑${status.ahead}` : ""}${status.behind ? ` ↓${status.behind}` : ""}`;
    span.textContent = `${status.branch ?? "?"}${n ? ` ±${n}` : ""}${sync}`;
  }
  btn.classList.toggle("dirty", !!status?.files.length);
}

function fileRow(f: GitFile, inIndex: boolean): string {
  const code = inIndex ? f.index : untracked(f) ? "?" : f.worktree;
  const name = f.path.split("/").pop() ?? f.path;
  const dir = f.path.slice(0, f.path.length - name.length);
  const actions = inIndex
    ? `<button class="icon-btn sm" data-git="unstage" title="Retirer de l'index">−</button>`
    : `<button class="icon-btn sm" data-git="discard" title="Annuler les modifications (cliquer deux fois)">↺</button>
       <button class="icon-btn sm" data-git="stage" title="Indexer">+</button>`;
  return `<div class="git-file" data-path="${escapeHtml(f.path)}" data-staged="${inIndex}" data-untracked="${untracked(f)}" title="${escapeHtml(f.origPath ? `${f.origPath} → ${f.path}` : f.path)}">
    <span class="git-code c-${escapeHtml(code === "?" ? "N" : code)}" title="${LABEL[code] ?? code}">${code === "?" ? "N" : escapeHtml(code)}</span>
    <span class="git-name">${escapeHtml(name)} <small>${escapeHtml(dir)}</small></span>
    <span class="git-actions">${actions}</span>
  </div>`;
}

function render() {
  renderBadge();
  const body = $("#git-body");
  if (!status) {
    body.innerHTML = '<p class="muted small"><span class="spinner"></span> Chargement…</p>';
    return;
  }
  if (!status.isRepo) {
    body.innerHTML = `<div class="git-empty"><p>Ce dossier n'est pas un dépôt git.</p><button class="btn-primary sm" data-git="init">Initialiser un dépôt</button></div>`;
    $(".git-sync").hidden = true;
    $(".git-commit").hidden = true;
    $("#git-history").hidden = true;
    return;
  }
  $(".git-sync").hidden = false;
  $(".git-commit").hidden = false;
  $("#git-history").hidden = false;
  $("#git-ahead").textContent = status.ahead ? `↑${status.ahead}` : "";
  $("#git-behind").textContent = status.behind ? `↓${status.behind}` : "";
  $("#git-upstream").textContent = status.upstream ? `→ ${status.upstream}` : "aucune branche distante suivie";
  const inIndex = status.files.filter(staged);
  const changes = status.files.filter(unstaged);
  const section = (title: string, files: GitFile[], idx: boolean, all: string) =>
    files.length
      ? `<div class="git-section"><div class="git-section-head"><span>${title} <small>${files.length}</small></span>
           <button class="btn-ghost sm" data-git="${all}">${idx ? "Tout retirer" : "Tout indexer"}</button></div>
           ${files.map((f) => fileRow(f, idx)).join("")}</div>`
      : "";
  body.innerHTML =
    section("Indexés", inIndex, true, "unstage-all") + section("Modifications", changes, false, "stage-all") ||
    '<p class="muted small git-clean">✓ Arbre de travail propre.</p>';
}

export async function refreshGit(quiet = true) {
  const ws = deps.workspace();
  if (!ws) {
    status = null;
    renderBadge();
    return;
  }
  if (ws !== lastWs) {
    status = null;
    lastWs = ws;
    $("#git-diff").hidden = true;
  }
  try {
    status = await api.gitStatus(ws);
  } catch (e) {
    status = null;
    if (!quiet) deps.toast(`Git : ${errMsg(e)}`, true);
    $("#git-body").innerHTML = `<p class="error small">${escapeHtml(errMsg(e))}</p>`;
    renderBadge();
    return;
  }
  render();
  if (isGitPanelOpen() && status.isRepo) {
    loadBranches(ws);
    loadLog(ws);
  }
}

/** Actualisation différée (après une modification de l'agent, par ex.). */
export function scheduleGitRefresh(delay = 600) {
  clearTimeout(refreshTimer);
  refreshTimer = window.setTimeout(() => refreshGit(), delay);
}

async function loadBranches(ws: string) {
  const sel = $<HTMLSelectElement>("#git-branch");
  try {
    const list = await api.gitBranches(ws);
    const cur = status?.branch ?? "";
    const opts = [...new Set([cur, ...list].filter(Boolean))];
    sel.innerHTML =
      opts.map((b) => `<option value="${escapeHtml(b)}">${escapeHtml(b)}</option>`).join("") + '<option value="__new__">+ Nouvelle branche…</option>';
    sel.value = cur;
  } catch {
    sel.innerHTML = "";
  }
}

async function loadLog(ws: string) {
  try {
    const commits = await api.gitLog(ws, 20);
    $("#git-log").innerHTML =
      commits
        .map(
          (c) => `<li><code>${escapeHtml(c.hash)}</code> ${escapeHtml(c.subject)} <small>${escapeHtml(c.author)}, ${escapeHtml(c.date)}</small></li>`,
        )
        .join("") || '<li class="muted">Aucun commit.</li>';
  } catch (e) {
    $("#git-log").innerHTML = `<li class="error">${escapeHtml(errMsg(e))}</li>`;
  }
}

async function op(label: string, fn: () => Promise<unknown>, success?: string) {
  if (busy) return;
  busy = true;
  panel().classList.add("busy");
  try {
    const out = await fn();
    if (success) deps.toast(typeof out === "string" && out.trim() ? `${success} — ${out.trim().split("\n").pop()}` : success);
  } catch (e) {
    deps.toast(`${label} : ${errMsg(e)}`, true);
  } finally {
    busy = false;
    panel().classList.remove("busy");
    await refreshGit();
  }
}

async function showDiff(path: string, isStaged: boolean, isUntracked: boolean) {
  const ws = deps.workspace();
  if (!ws) return;
  const box = $("#git-diff");
  box.hidden = false;
  box.innerHTML = `<div class="git-diff-head"><code>${escapeHtml(path)}</code>${isStaged ? " <small>(indexé)</small>" : ""}<button class="icon-btn sm" data-git="close-diff">×</button></div><p class="muted small"><span class="spinner"></span></p>`;
  try {
    const d = await api.gitDiff(ws, path, isStaged, isUntracked);
    box.querySelector("p")!.outerHTML = d.trim() ? diffHtml(d) : '<p class="muted small">Aucune différence textuelle (fichier binaire ou mode modifié).</p>';
  } catch (e) {
    box.querySelector("p")!.outerHTML = `<p class="error small">${escapeHtml(errMsg(e))}</p>`;
  }
}

async function generateMessage() {
  const ws = deps.workspace();
  if (!ws || !status) return;
  const btn = $<HTMLButtonElement>("#git-generate");
  const area = $<HTMLTextAreaElement>("#git-message");
  const hasStaged = status.files.some(staged);
  btn.disabled = true;
  btn.textContent = "Rédaction…";
  try {
    let diff = await api.gitDiff(ws, null, hasStaged, false);
    if (!diff.trim()) throw new Error("rien à décrire (aucune modification suivie)");
    if (diff.length > 40000) diff = `${diff.slice(0, 40000)}\n[… diff tronqué]`;
    const history = (await api.gitLog(ws, 8).catch(() => [])).map((c) => `- ${c.subject}`).join("\n");
    const model = deps.model();
    area.value = "";
    let text = "";
    await api.chat(
      crypto.randomUUID(),
      {
        model,
        temperature: 0.2,
        maxTokens: 400,
        tools: [],
        ...deps.chatOptions(model, false),
        messages: [
          {
            role: "system",
            content:
              "Tu rédiges des messages de commit git. Réponds UNIQUEMENT par le message, sans bloc de code ni commentaire : " +
              "une ligne de titre de 72 caractères maximum, à l'impératif, au format Conventional Commits (feat:, fix:, refactor:, docs:, chore:…), " +
              "puis si utile une ligne vide et un court corps expliquant le pourquoi. Utilise la langue des commits précédents (français par défaut).",
          },
          { role: "user", content: `Commits précédents :\n${history || "(aucun)"}\n\nDiff ${hasStaged ? "indexé" : "de l'arbre de travail"} :\n${diff}` },
        ],
      },
      (ev) => {
        if (ev.type === "delta") {
          text += ev.text;
          area.value = text.replace(/^```\w*\n?|```$/g, "").trimStart();
        } else if (ev.type === "usage") {
          deps.recordUsage(model, ev.promptTokens, ev.completionTokens);
        }
      },
    );
    area.value = text.replace(/^```\w*\n?|```\s*$/g, "").trim();
    if (!hasStaged) deps.toast("Aucun fichier indexé : le message décrit toutes les modifications. Indexez-les avant de committer.");
  } catch (e) {
    deps.toast(`Génération impossible : ${errMsg(e)}`, true);
  } finally {
    btn.disabled = false;
    btn.textContent = "✨ Générer";
  }
}

export function toggleGitPanel(open?: boolean) {
  const p = panel();
  p.hidden = open === undefined ? !p.hidden : !open;
  if (!p.hidden) refreshGit(false);
}

export function initGitUi(d: Deps) {
  deps = d;
  $("#git-btn").addEventListener("click", () => toggleGitPanel());
  $("#git-close").addEventListener("click", () => toggleGitPanel(false));
  $("#git-refresh").addEventListener("click", () => refreshGit(false));

  panel().addEventListener("click", async (e) => {
    const t = e.target as HTMLElement;
    const ws = deps.workspace();
    if (!ws) return;
    const syncBtn = t.closest<HTMLElement>("[data-git-sync]");
    if (syncBtn) {
      const action = syncBtn.dataset.gitSync as "push" | "pull" | "fetch";
      const label = { push: "Push", pull: "Pull", fetch: "Fetch" }[action];
      return op(label, () => api.gitSync(ws, action), `${label} terminé`);
    }
    const act = t.closest<HTMLElement>("[data-git]")?.dataset.git;
    const row = t.closest<HTMLElement>(".git-file");
    const path = row?.dataset.path;
    switch (act) {
      case "init":
        return op("Initialisation", () => api.gitInit(ws), "Dépôt initialisé");
      case "stage":
        return path && op("Indexation", () => api.gitStage(ws, [path]));
      case "unstage":
        return path && op("Retrait de l'index", () => api.gitUnstage(ws, [path]));
      case "stage-all":
        return op("Indexation", () => api.gitStage(ws, []));
      case "unstage-all":
        return op("Retrait de l'index", () => api.gitUnstage(ws, []));
      case "close-diff":
        $("#git-diff").hidden = true;
        return;
      case "discard": {
        const btn = t.closest<HTMLElement>("[data-git]")!;
        if (!btn.classList.contains("confirm")) {
          btn.classList.add("confirm");
          btn.title = "Cliquer à nouveau : les modifications seront perdues";
          setTimeout(() => btn.classList.remove("confirm"), 3000);
          return;
        }
        const isUntracked = row?.dataset.untracked === "true";
        return path && op("Annulation", () => api.gitDiscard(ws, isUntracked ? [] : [path], isUntracked ? [path] : []), "Modifications annulées");
      }
    }
    if (row && path && !t.closest("button")) {
      return showDiff(path, row.dataset.staged === "true", row.dataset.untracked === "true");
    }
  });

  $<HTMLSelectElement>("#git-branch").addEventListener("change", async (e) => {
    const sel = e.target as HTMLSelectElement;
    const ws = deps.workspace();
    if (!ws) return;
    if (sel.value === "__new__") {
      // Saisie en ligne (window.prompt n'existe pas dans toutes les WebView).
      const input = $<HTMLInputElement>("#git-new-branch");
      sel.hidden = true;
      input.hidden = false;
      input.value = "";
      input.focus();
      return;
    }
    return op("Changement de branche", () => api.gitCheckout(ws, sel.value, false), `Branche ${sel.value}`);
  });

  const newBranch = $<HTMLInputElement>("#git-new-branch");
  const closeNewBranch = () => {
    newBranch.hidden = true;
    $("#git-branch").hidden = false;
  };
  newBranch.addEventListener("keydown", (e) => {
    const ws = deps.workspace();
    if (e.key === "Escape") {
      closeNewBranch();
      refreshGit();
    } else if (e.key === "Enter" && ws && newBranch.value.trim()) {
      const name = newBranch.value.trim();
      closeNewBranch();
      op("Création de branche", () => api.gitCheckout(ws, name, true), `Branche ${name} créée`);
    }
  });
  newBranch.addEventListener("blur", () => {
    if (!newBranch.hidden) {
      closeNewBranch();
      refreshGit();
    }
  });

  $("#git-generate").addEventListener("click", generateMessage);
  $("#git-commit").addEventListener("click", () => {
    const ws = deps.workspace();
    const area = $<HTMLTextAreaElement>("#git-message");
    if (!ws || !area.value.trim()) return deps.toast("Écrivez ou générez un message de commit.", true);
    if (!status?.files.some(staged)) return deps.toast("Aucun fichier indexé : utilisez « + » ou « Tout indexer ».", true);
    return op(
      "Commit",
      async () => {
        const res = await api.gitCommit(ws, area.value.trim());
        area.value = "";
        return res;
      },
      "Commit créé",
    );
  });
}
