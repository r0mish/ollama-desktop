// Serveurs Ollama : liste dans les réglages (adresse directe ou tunnel SSH, clé API par serveur),
// et indicateur permanent de la barre latérale (serveur actif, modèles en mémoire).
import { api, type RunningModel, type Server, type ServerStatus, type Settings } from "./api";
import { escapeHtml } from "./markdown";
import { fmtBytes } from "./models-ui";
import { closePopover, togglePopover } from "./popover";
import { sshHosts } from "./ssh-ui";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

interface Deps {
  toast: (msg: string, error?: boolean) => void;
  settings: () => Settings;
  status: () => ServerStatus | null;
  running: () => RunningModel[];
  modelCount: () => number;
  /** Enregistre les serveurs (et le serveur actif) puis recharge l'état du serveur et ses modèles. */
  apply: (servers: Server[], active: string) => Promise<void>;
  openSettings: () => void;
  openModels: () => void;
  openSsh: () => void;
  /** Relit /api/ps (après un déchargement). */
  refreshRunning: () => Promise<void>;
}

let deps: Deps;
/** Copie de travail des réglages (validée par « Enregistrer » ou « Tester »). */
let servers: Server[] = [];
let active = "";
/** Serveur en cours d'édition (null = formulaire fermé). */
let editing: Server | null = null;

const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

export const serversDraft = () => ({ servers, activeServer: active });

const address = (v: Server) => {
  if (!v.sshHost) return v.url;
  const h = sshHosts().find((x) => x.id === v.sshHost);
  return `tunnel SSH · ${h?.label ?? "serveur SSH supprimé"}${v.remotePort && v.remotePort !== 11434 ? ` : ${v.remotePort}` : ""}`;
};

function renderList() {
  $("#srv-list").innerHTML = servers
    .map(
      (v) => `<div class="srv-row${v.id === active ? " active" : ""}" data-id="${escapeHtml(v.id)}">
        <label class="srv-pick"><input type="radio" name="srv-active" ${v.id === active ? "checked" : ""} />
          <span><strong>${escapeHtml(v.name)}</strong><small>${escapeHtml(address(v))}</small></span></label>
        <button type="button" class="btn-ghost sm" data-edit>Modifier</button>
        ${servers.length > 1 ? '<button type="button" class="icon-btn sm danger" data-remove title="Retirer ce serveur">×</button>' : ""}
      </div>`,
    )
    .join("");
}

export function renderServerStatus(st: ServerStatus | null) {
  const el = $("#server-status");
  el.className = st?.error ? "small error" : "muted small";
  const n = deps.modelCount();
  el.textContent = !st
    ? ""
    : st.version
      ? `✓ ${st.name} : connecté à Ollama ${st.version} · ${n} modèle${n > 1 ? "s" : ""} installé${n > 1 ? "s" : ""}` +
        (st.tunnel ? " · via tunnel SSH chiffré" : "") +
        (st.insecure ? " · ⚠ HTTP en clair hors de cette machine : réservez-le à un réseau de confiance, ou passez par un tunnel SSH." : "")
      : `✗ ${st.name} : ${st.error ?? "injoignable"}`;
}

async function renderKeyStatus(id: string) {
  const st = await api.keyStatus(id).catch(() => ({ configured: false, source: null }));
  $("#key-status").textContent = !st.configured
    ? "Aucune clé : inutile pour un serveur Ollama classique."
    : st.source === "keyring"
      ? "✓ Clé enregistrée dans le trousseau du système."
      : "✓ Clé lue depuis la variable d'environnement OLLAMA_API_KEY.";
}

function renderForm() {
  const form = $("#srv-form");
  form.hidden = !editing;
  if (!editing) return;
  const v = editing;
  const hosts = sshHosts();
  $<HTMLInputElement>("#srv-name").value = v.name;
  $<HTMLSelectElement>("#srv-kind").value = v.sshHost != null ? "ssh" : "direct";
  $<HTMLInputElement>("#srv-url").value = v.url;
  $<HTMLSelectElement>("#srv-ssh").innerHTML = hosts.length
    ? hosts.map((h) => `<option value="${escapeHtml(h.id)}">${escapeHtml(h.label)} (${escapeHtml(h.user ? `${h.user}@` : "")}${escapeHtml(h.host)})</option>`).join("")
    : '<option value="">Aucun serveur SSH enregistré</option>';
  if (v.sshHost) $<HTMLSelectElement>("#srv-ssh").value = v.sshHost;
  $<HTMLInputElement>("#srv-port").value = v.remotePort ? String(v.remotePort) : "";
  $<HTMLInputElement>("#api-key").value = "";
  syncKind();
  renderKeyStatus(v.id);
  $<HTMLInputElement>("#srv-name").focus();
}

function syncKind() {
  const ssh = $<HTMLSelectElement>("#srv-kind").value === "ssh";
  $("#srv-direct").hidden = ssh;
  $("#srv-tunnel").hidden = !ssh;
}

/** Valide le formulaire dans la copie de travail. */
function commitForm(): boolean {
  if (!editing) return true;
  const ssh = $<HTMLSelectElement>("#srv-kind").value === "ssh";
  const name = $<HTMLInputElement>("#srv-name").value.trim();
  const url = $<HTMLInputElement>("#srv-url").value.trim();
  const host = $<HTMLSelectElement>("#srv-ssh").value;
  const port = Number($<HTMLInputElement>("#srv-port").value);
  const fail = (msg: string) => {
    deps.toast(msg, true);
    return false;
  };
  if (ssh && !host) return fail("Enregistrez d'abord un serveur SSH (bouton « Serveurs SSH… »).");
  if (!ssh && !/^https?:\/\//.test(url)) return fail("L'adresse doit commencer par http:// ou https://");
  const next: Server = {
    id: editing.id,
    name: name || (ssh ? (sshHosts().find((h) => h.id === host)?.label ?? "Tunnel SSH") : url.replace(/^https?:\/\//, "")),
    url: ssh ? "" : url,
    sshHost: ssh ? host : null,
    remotePort: ssh && port > 0 && port < 65536 ? port : null,
  };
  const i = servers.findIndex((x) => x.id === next.id);
  if (i >= 0) servers[i] = next;
  else servers.push(next);
  if (!active) active = next.id;
  editing = null;
  renderForm();
  renderList();
  return true;
}

/** À l'ouverture des réglages. */
export function renderServers() {
  const s = deps.settings();
  servers = s.servers.map((v) => ({ ...v }));
  active = s.activeServer;
  editing = null;
  renderList();
  renderForm();
  renderServerStatus(deps.status());
}

// ---------------------------------------------------------------- indicateur de la barre latérale

function placement(r: RunningModel): string {
  if (!r.size) return "";
  const gpu = Math.round((r.sizeVram / r.size) * 100);
  return gpu >= 100 ? "100 % GPU" : gpu <= 0 ? "CPU" : `${gpu} % GPU`;
}

export function renderServerChip() {
  const st = deps.status();
  const chip = $("#server-chip");
  const run = deps.running();
  chip.dataset.state = !st ? "connecting" : st.version ? "ok" : "error";
  const mem = run.reduce((n, r) => n + r.size, 0);
  chip.querySelector(".sc-name")!.textContent = st?.name ?? deps.settings().servers.find((v) => v.id === deps.settings().activeServer)?.name ?? "Ollama";
  chip.querySelector(".sc-detail")!.textContent = !st
    ? "connexion…"
    : !st.version
      ? "injoignable"
      : run.length
        ? `${run.length === 1 ? run[0].name : `${run.length} modèles`} · ${fmtBytes(mem)}`
        : "aucun modèle en mémoire";
  chip.title = st?.version
    ? `${st.name} — Ollama ${st.version}\n${st.url}${run.length ? `\nEn mémoire : ${run.map((r) => `${r.name} (${fmtBytes(r.size)}, ${placement(r)})`).join(", ")}` : ""}`
    : (st?.error ?? "Connexion à Ollama…");
}

function openChipMenu() {
  const p = togglePopover($("#server-chip"), "sm", undefined, "top");
  if (!p) return;
  const draw = () => {
    const s = deps.settings();
    const st = deps.status();
    const run = deps.running();
    p.innerHTML = `<div class="group">Serveurs Ollama</div>
      ${s.servers
        .map(
          (v) => `<button type="button" class="sm-server${v.id === s.activeServer ? " active" : ""}" data-server="${escapeHtml(v.id)}">
            <i class="sm-dot" data-state="${v.id === s.activeServer ? (st?.version ? "ok" : st ? "error" : "connecting") : ""}"></i>
            <span><strong>${escapeHtml(v.name)}</strong><small>${escapeHtml(address(v))}</small></span></button>`,
        )
        .join("")}
      ${st && !st.version ? `<p class="error small sm-error">${escapeHtml(st.error ?? "")}</p>` : ""}
      <div class="group">En mémoire</div>
      ${
        run.length
          ? run
              .map(
                (r) => `<div class="sm-model"><span><code>${escapeHtml(r.name)}</code><small>${[fmtBytes(r.size), placement(r), r.contextLength ? `contexte ${Math.round(r.contextLength / 1024)} k` : ""].filter(Boolean).join(" · ")}</small></span>
                  <button type="button" class="btn-ghost sm" data-unload="${escapeHtml(r.name)}">Décharger</button></div>`,
              )
              .join("")
          : '<p class="muted small sm-empty">Aucun modèle chargé.</p>'
      }
      <div class="sm-foot"><button type="button" class="btn-ghost sm" data-settings>Gérer les serveurs…</button><button type="button" class="btn-ghost sm" data-models>Modèles…</button></div>`;
  };
  draw();
  p.onclick = async (e) => {
    const t = e.target as HTMLElement;
    const id = t.closest<HTMLElement>("[data-server]")?.dataset.server;
    const unload = t.closest<HTMLElement>("[data-unload]")?.dataset.unload;
    try {
      if (id && id !== deps.settings().activeServer) {
        closePopover();
        await deps.apply(deps.settings().servers, id);
      } else if (unload) {
        await api.unloadModel(unload);
        await deps.refreshRunning();
        draw();
      } else if (t.closest("[data-settings]")) {
        closePopover();
        deps.openSettings();
      } else if (t.closest("[data-models]")) {
        closePopover();
        deps.openModels();
      }
    } catch (err) {
      deps.toast(errMsg(err), true);
    }
  };
  deps.refreshRunning().then(() => {
    if (!p.hidden && p.classList.contains("sm")) draw();
  });
}

export function initServersUi(d: Deps) {
  deps = d;
  $("#server-chip").addEventListener("click", openChipMenu);
  $("#srv-kind").addEventListener("change", syncKind);
  $("#srv-add").addEventListener("click", () => {
    editing = { id: crypto.randomUUID().slice(0, 8), name: "", url: "http://", sshHost: null, remotePort: null };
    renderForm();
  });
  $("#srv-cancel").addEventListener("click", () => {
    editing = null;
    renderForm();
  });
  $("#srv-ok").addEventListener("click", () => commitForm());
  $("#srv-open-ssh").addEventListener("click", () => deps.openSsh());
  $("#srv-list").addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    const id = t.closest<HTMLElement>("[data-id]")?.dataset.id;
    const v = servers.find((x) => x.id === id);
    if (!v) return;
    if (t.closest("[data-edit]")) {
      editing = { ...v };
      renderForm();
    } else if (t.closest("[data-remove]")) {
      servers = servers.filter((x) => x.id !== id);
      if (active === id) active = servers[0].id;
      if (editing?.id === id) editing = null;
      api.deleteKey(v.id).catch(() => {});
      renderList();
      renderForm();
    } else if (t.closest(".srv-pick")) {
      active = v.id;
      renderList();
    }
  });
  $("#test-server").addEventListener("click", async () => {
    if (!commitForm()) return;
    const el = $("#server-status");
    el.className = "muted small";
    el.textContent = "Test en cours…";
    try {
      await deps.apply(servers, active);
      renderServerStatus(deps.status());
    } catch (e) {
      el.className = "small error";
      el.textContent = `✗ ${errMsg(e)}`;
    }
  });
  $("#save-key").addEventListener("click", async () => {
    if (!editing) return;
    try {
      await api.setKey(editing.id, $<HTMLInputElement>("#api-key").value);
      $<HTMLInputElement>("#api-key").value = "";
      await renderKeyStatus(editing.id);
      deps.toast("Clé API enregistrée");
    } catch (e) {
      deps.toast(errMsg(e), true);
    }
  });
  $("#delete-key").addEventListener("click", async () => {
    if (!editing) return;
    await api.deleteKey(editing.id).catch((e) => deps.toast(errMsg(e), true));
    await renderKeyStatus(editing.id);
  });
}

/** Valide un éventuel formulaire ouvert avant l'enregistrement des réglages. */
export const commitServerForm = () => commitForm();
