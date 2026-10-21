// Gestionnaire de modèles Ollama : modèles chargés en mémoire, modèles installés,
// téléchargement depuis le registre (avec progression), suppression.
import { api, type ModelInfo, type RunningModel } from "./api";
import { escapeHtml } from "./markdown";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

interface Deps {
  toast: (msg: string, error?: boolean) => void;
  models: () => ModelInfo[];
  /** Recharge la liste des modèles dans l'application. */
  reload: () => Promise<void>;
  /** Choisit ce modèle pour la conversation en cours. */
  use: (model: string) => void;
  numCtx: (model: string) => number | null;
}

/** Suggestions : modèles open-weight européens d'abord. */
const SUGGESTIONS: { id: string; by: string; desc: string }[] = [
  { id: "ministral-3:3b", by: "Mistral AI · France", desc: "3 B, très léger · vision, outils" },
  { id: "ministral-3:8b", by: "Mistral AI · France", desc: "8 B, bon compromis · vision, outils" },
  { id: "ministral-3:14b", by: "Mistral AI · France", desc: "14 B · vision, outils" },
  { id: "mistral-small3.2", by: "Mistral AI · France", desc: "24 B, polyvalent · vision, outils" },
  { id: "devstral-small-2", by: "Mistral AI · France", desc: "24 B, spécialisé agent de code" },
  { id: "magistral", by: "Mistral AI · France", desc: "24 B · raisonnement" },
  { id: "qwen3:8b", by: "Alibaba", desc: "8 B · outils, raisonnement" },
  { id: "qwen3-coder:30b", by: "Alibaba", desc: "30 B MoE, code · outils" },
  { id: "gpt-oss:20b", by: "OpenAI (open-weight)", desc: "20 B MoE · outils, raisonnement" },
  { id: "gemma3:12b", by: "Google", desc: "12 B · vision" },
];

let deps: Deps;
let running: RunningModel[] = [];
let pull: { name: string; requestId: string; status: string; completed: number | null; total: number | null; layers: Map<string, [number, number]> } | null = null;
let poll = 0;

const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

export function fmtBytes(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1).replace(".", ",")} Go`;
  if (n >= 1e6) return `${Math.round(n / 1e6)} Mo`;
  return `${Math.round(n / 1e3)} ko`;
}

const fmtCtx = (n: number) => (n >= 1024 ? `${Math.round(n / 1024)} k` : String(n));

/** Lettres de capacités d'un modèle (outils, vision, raisonnement). */
export function capsHtml(m: ModelInfo): string {
  return [
    m.tools ? '<span class="cap" title="Appels d\'outils : agent de code, recherche web, MCP">outils</span>' : "",
    m.vision ? '<span class="cap" title="Comprend les images">vision</span>' : "",
    m.thinking ? '<span class="cap" title="Raisonnement avant la réponse">raisonnement</span>' : "",
    m.remote ? '<span class="cap" title="Exécuté sur un serveur distant">cloud</span>' : "",
  ].join("");
}

function placement(r: RunningModel): string {
  if (!r.size) return "";
  const gpu = Math.round((r.sizeVram / r.size) * 100);
  if (gpu >= 100) return "100 % GPU";
  if (gpu <= 0) return "100 % CPU";
  return `${100 - gpu} % CPU / ${gpu} % GPU`;
}

function expires(r: RunningModel): string {
  if (!r.expiresAt) return "";
  const ms = new Date(r.expiresAt).getTime() - Date.now();
  if (!Number.isFinite(ms) || ms > 365 * 86400000) return "maintenu en mémoire";
  if (ms <= 0) return "déchargement…";
  const min = Math.round(ms / 60000);
  return min < 1 ? "libéré dans < 1 min" : `libéré dans ${min} min`;
}

function renderRunning() {
  $("#md-running").innerHTML = running.length
    ? running
        .map(
          (r) => `<div class="md-row">
            <div class="md-main"><code>${escapeHtml(r.name)}</code>
              <small class="muted">${[fmtBytes(r.size), placement(r), r.contextLength ? `contexte ${fmtCtx(r.contextLength)}` : "", expires(r)].filter(Boolean).join(" · ")}</small></div>
            <button type="button" class="btn-ghost sm" data-unload="${escapeHtml(r.name)}">Décharger</button>
          </div>`,
        )
        .join("")
    : '<p class="muted small">Aucun modèle chargé : le premier message charge le modèle en mémoire.</p>';
}

function renderInstalled() {
  const models = deps.models();
  const loaded = new Set(running.map((r) => r.name));
  $("#md-installed").innerHTML = models.length
    ? models
        .map((m) => {
          const meta = [m.parameterSize, m.quantization, m.size ? fmtBytes(m.size) : "", m.maxContextLength ? `contexte max ${fmtCtx(m.maxContextLength)}` : ""].filter(Boolean).join(" · ");
          const id = escapeHtml(m.id);
          return `<div class="md-row">
            <div class="md-main"><span><code>${id}</code>${capsHtml(m)}</span><small class="muted">${escapeHtml(meta)}</small></div>
            <span class="row">
              <button type="button" class="btn-ghost sm" data-use="${id}" title="Utiliser pour la conversation en cours">Utiliser</button>
              ${loaded.has(m.id) ? "" : `<button type="button" class="btn-ghost sm" data-load="${id}" title="Charger en mémoire maintenant">Charger</button>`}
              <button type="button" class="icon-btn sm" data-update="${id}" title="Mettre à jour (retélécharge les couches modifiées)">↻</button>
              <button type="button" class="icon-btn sm danger" data-delete="${id}" title="Supprimer du disque">×</button>
            </span>
          </div>`;
        })
        .join("")
    : '<p class="muted small">Aucun modèle installé. Choisissez-en un ci-dessous.</p>';
}

function renderSuggestions() {
  const have = new Set(deps.models().map((m) => m.id.replace(/:latest$/, "")));
  $("#md-suggest").innerHTML = SUGGESTIONS.map(
    (s) => `<button type="button" class="md-suggestion" data-pull="${escapeHtml(s.id)}" ${have.has(s.id) ? "disabled" : ""}>
      <code>${escapeHtml(s.id)}</code><small>${escapeHtml(s.desc)}</small><small class="muted">${escapeHtml(s.by)}${have.has(s.id) ? " · installé" : ""}</small>
    </button>`,
  ).join("");
}

function renderProgress() {
  const box = $("#md-progress");
  $<HTMLButtonElement>("#md-pull-btn").disabled = !!pull;
  if (!pull) {
    box.hidden = true;
    return;
  }
  const pct = pull.total && pull.completed != null ? Math.min(100, Math.round((pull.completed / pull.total) * 100)) : null;
  const detail = pull.total && pull.completed != null ? ` · ${fmtBytes(pull.completed)} / ${fmtBytes(pull.total)}` : "";
  const status = pull.status.startsWith("pulling") ? "Téléchargement" : pull.status === "verifying sha256 digest" ? "Vérification" : pull.status === "writing manifest" ? "Finalisation" : pull.status || "Connexion…";
  box.innerHTML = `<div class="row"><strong>${escapeHtml(pull.name)}</strong><span class="muted small">${escapeHtml(status)}${pct !== null ? ` ${pct} %` : ""}${detail}</span>
      <span class="spacer"></span><button type="button" class="btn-ghost sm danger" data-cancel-pull>Annuler</button></div>
    <span class="bar"><i style="width:${pct ?? 0}%"></i></span>`;
  box.hidden = false;
}

async function refresh() {
  running = await api.runningModels().catch(() => []);
  renderRunning();
  renderInstalled();
  renderSuggestions();
  renderProgress();
}

async function startPull(name: string) {
  name = name.trim().replace(/^ollama\s+(pull|run)\s+/, "");
  if (!name || pull) return;
  pull = { name, requestId: crypto.randomUUID(), status: "", completed: null, total: null, layers: new Map() };
  renderProgress();
  try {
    const done = await api.pullModel(pull.requestId, name, (p) => {
      if (!pull) return;
      pull.status = p.status;
      // Progression globale : somme des couches (poids, gabarit, licence…).
      if (p.digest && p.total) {
        pull.layers.set(p.digest, [p.completed ?? 0, p.total]);
        let [done, total] = [0, 0];
        for (const [c, t] of pull.layers.values()) {
          done += c;
          total += t;
        }
        pull.completed = done;
        pull.total = total;
      }
      renderProgress();
    });
    if (done) {
      deps.toast(`Modèle ${name} installé`);
      $<HTMLInputElement>("#md-pull-name").value = "";
    } else {
      deps.toast("Téléchargement annulé (il reprendra là où il s'est arrêté)");
    }
  } catch (e) {
    deps.toast(errMsg(e), true);
  } finally {
    pull = null;
    await deps.reload();
    await refresh();
  }
}

export async function openModelsDialog() {
  const dlg = $<HTMLDialogElement>("#models-dialog");
  const st = await api.serverStatus().catch(() => null);
  $("#md-server").textContent = st?.version ? `Ollama ${st.version} · ${st.url}` : st?.error ?? "";
  await deps.reload();
  await refresh();
  if (!dlg.open) dlg.showModal();
  clearInterval(poll);
  poll = window.setInterval(() => {
    if (!dlg.open) return clearInterval(poll);
    api.runningModels().then((r) => {
      running = r;
      renderRunning();
    }).catch(() => {});
  }, 5000);
}

export function initModelsUi(d: Deps) {
  deps = d;
  const dlg = $<HTMLDialogElement>("#models-dialog");
  $("#md-close").addEventListener("click", () => dlg.close());
  $("#md-pull-form").addEventListener("submit", (e) => {
    e.preventDefault();
    startPull($<HTMLInputElement>("#md-pull-name").value);
  });
  dlg.addEventListener("click", async (e) => {
    const t = e.target as HTMLElement;
    const btn = t.closest<HTMLButtonElement>("button");
    if (!btn) return;
    const d = btn.dataset;
    try {
      if (d.pull) return startPull(d.pull);
      if (d.update) return startPull(d.update);
      if ("cancelPull" in d && pull) return api.cancel(pull.requestId);
      if (d.use) {
        deps.use(d.use);
        return dlg.close();
      }
      if (d.unload) {
        btn.disabled = true;
        await api.unloadModel(d.unload);
        deps.toast(`${d.unload} déchargé de la mémoire`);
        return refresh();
      }
      if (d.load) {
        btn.disabled = true;
        btn.textContent = "Chargement…";
        await api.preloadModel(d.load, deps.numCtx(d.load));
        return refresh();
      }
      if (d.delete) {
        if (!btn.classList.contains("confirm")) {
          btn.classList.add("confirm");
          btn.title = "Cliquer à nouveau pour supprimer définitivement";
          setTimeout(() => btn.classList.remove("confirm"), 3000);
          return;
        }
        await api.deleteModel(d.delete);
        deps.toast(`${d.delete} supprimé`);
        await deps.reload();
        return refresh();
      }
    } catch (err) {
      deps.toast(errMsg(err), true);
      refresh();
    }
  });
}
