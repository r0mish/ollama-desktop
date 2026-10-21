// Projets : conversations regroupées partageant des consignes et des fichiers de référence.
import { api, type Project, type ProjectFile } from "./api";
import { escapeHtml } from "./markdown";
import { openSshDialog, workspaceLabel } from "./ssh-ui";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

/** Même limite que le stockage (storage::MAX_PROJECT_CHARS). */
const MAX_CHARS = 10_000_000;
/** Au-delà, seuls des extraits pertinents sont envoyés (knowledge::FULL_CONTEXT_CHARS). */
const FULL_CHARS = 60_000;
const MAX_TEXT_FILE = 20 * 1024 * 1024;
const MAX_PDF = 50 * 1024 * 1024;

let projects: Project[] = [];
let editing: Project | null = null;
let onChange: (saved: Project | null, deletedId?: string) => void = () => {};
let notify: (msg: string, error?: boolean) => void = () => {};

const dlg = () => $<HTMLDialogElement>("#project-dialog");
const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

export const projectList = () => projects;
export const findProject = (id?: string | null) => (id ? (projects.find((p) => p.id === id) ?? null) : null);

export async function loadProjects() {
  projects = await api.listProjects().catch(() => []);
  return projects;
}

export const projectChars = (p: Project) => p.instructions.length + p.files.reduce((n, f) => n + f.text.length, 0);
const fmtSize = (chars: number) => (chars < 1000 ? `${chars} car.` : `${(chars / 1000).toFixed(chars < 10_000 ? 1 : 0)} k car.`);

function renderFiles() {
  if (!editing) return;
  const files = editing.files;
  $("#p-files").innerHTML = files.length
    ? files
        .map(
          (f, i) => `<div class="p-file"><span>${escapeHtml(f.name)}</span><small>${fmtSize(f.text.length)}</small>
            <button type="button" class="icon-btn sm" data-rm-file="${i}" title="Retirer">×</button></div>`,
        )
        .join("")
    : '<p class="muted small">Aucun fichier.</p>';
  const chars = $<HTMLTextAreaElement>("#p-instructions").value.length + files.reduce((n, f) => n + f.text.length, 0);
  const over = chars > MAX_CHARS;
  const fileChars = files.reduce((n, f) => n + f.text.length, 0);
  $("#p-size").textContent = over
    ? `Trop volumineux (max ${MAX_CHARS / 1_000_000} M caractères)`
    : fileChars > FULL_CHARS
      ? `${fmtSize(fileChars)} : recherche locale, seuls les extraits pertinents sont envoyés (≈ 4 k tokens par message)`
      : `≈ ${Math.round(chars / 3.5).toLocaleString("fr-FR")} tokens ajoutés à chaque message`;
  $("#p-size").classList.toggle("error", over);
}

const readAsBase64 = (f: Blob) =>
  new Promise<string>((res, rej) => {
    const r = new FileReader();
    r.onload = () => res((r.result as string).split(",")[1] ?? "");
    r.onerror = () => rej(r.error);
    r.readAsDataURL(f);
  });

async function readFile(file: File): Promise<ProjectFile> {
  if (file.type === "application/pdf" || /\.pdf$/i.test(file.name)) {
    if (file.size > MAX_PDF) throw new Error(`trop volumineux (max ${MAX_PDF / 1048576} Mo)`);
    const { text } = await api.processDocument(await readAsBase64(file));
    if (!text.trim()) throw new Error("aucun texte extrait");
    return { name: file.name, text };
  }
  if (file.size > MAX_TEXT_FILE) throw new Error(`trop volumineux (max ${MAX_TEXT_FILE / 1048576} Mo)`);
  const buf = new Uint8Array(await file.arrayBuffer());
  if (buf.subarray(0, 8000).includes(0)) throw new Error("format non pris en charge (texte ou PDF uniquement)");
  return { name: file.name, text: new TextDecoder().decode(buf) };
}

async function addFiles(list: Iterable<File>) {
  const btn = $<HTMLButtonElement>("#p-add");
  btn.disabled = true;
  try {
    for (const file of list) {
      btn.textContent = `Lecture de ${file.name}…`;
      try {
        const f = await readFile(file);
        if (!editing) return;
        editing.files = editing.files.filter((x) => x.name !== f.name);
        editing.files.push(f);
        renderFiles();
      } catch (e) {
        notify(`${file.name} : ${errMsg(e)}`, true);
      }
    }
  } finally {
    btn.disabled = false;
    btn.textContent = "+ Ajouter des fichiers";
  }
}

function renderWorkspace() {
  const ws = editing?.workspace;
  const cur = $("#p-ws-current");
  if (ws) {
    const l = workspaceLabel(ws);
    cur.innerHTML = `${l.remote ? "🖥" : "📁"} <strong>${escapeHtml(l.name)}</strong> <small>${escapeHtml(l.detail)}</small>`;
    cur.title = l.detail;
  } else {
    cur.textContent = "Aucun : chaque session de code choisit son dossier";
    cur.title = "";
  }
  cur.classList.toggle("muted", !ws);
  $("#p-ws-clear").hidden = !ws;
}

function setWorkspace(ws: string | null) {
  if (!editing) return;
  editing.workspace = ws;
  renderWorkspace();
  onWorkspaceAdded();
}

let onWorkspaceAdded: () => void = () => {};

export function openProjectDialog(p: Project | null, defaults: Partial<Project> = {}) {
  const now = Date.now();
  editing = p
    ? structuredClone(p)
    : { id: crypto.randomUUID(), name: "", instructions: "", files: [], createdAt: now, updatedAt: now, ...defaults };
  $("#project-title").textContent = p ? `Projet « ${p.name} »` : "Nouveau projet";
  $<HTMLInputElement>("#p-name").value = editing.name;
  $<HTMLTextAreaElement>("#p-instructions").value = editing.instructions;
  const del = $("#p-delete");
  del.hidden = !p;
  del.classList.remove("confirm");
  del.textContent = "Supprimer le projet";
  $("#project-error").hidden = true;
  renderWorkspace();
  renderFiles();
  dlg().showModal();
  $<HTMLInputElement>("#p-name").focus();
}

export function initProjectsUi(opts: {
  onChange: (saved: Project | null, deletedId?: string) => void;
  toast: (msg: string, error?: boolean) => void;
  /** Un dossier vient d'être autorisé (liste des dossiers à rafraîchir). */
  onWorkspaceAdded: () => void;
}) {
  onChange = opts.onChange;
  notify = opts.toast;
  onWorkspaceAdded = opts.onWorkspaceAdded;
  $("#p-ws-local").addEventListener("click", async () => {
    try {
      const ws = await api.pickWorkspace();
      if (ws) setWorkspace(ws);
    } catch (e) {
      notify(errMsg(e), true);
    }
  });
  $("#p-ws-ssh").addEventListener("click", () => openSshDialog((ws) => setWorkspace(ws)));
  $("#p-ws-clear").addEventListener("click", () => setWorkspace(null));
  $("#p-cancel").addEventListener("click", () => dlg().close());
  $("#p-instructions").addEventListener("input", renderFiles);
  $("#p-add").addEventListener("click", () => $<HTMLInputElement>("#p-file-input").click());
  $<HTMLInputElement>("#p-file-input").addEventListener("change", async (e) => {
    const input = e.currentTarget as HTMLInputElement;
    const files = [...(input.files ?? [])];
    input.value = "";
    await addFiles(files);
  });
  $("#p-files").addEventListener("click", (e) => {
    const i = (e.target as HTMLElement).closest<HTMLElement>("[data-rm-file]")?.dataset.rmFile;
    if (i === undefined || !editing) return;
    editing.files.splice(Number(i), 1);
    renderFiles();
  });
  $("#project-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!editing) return;
    editing.name = $<HTMLInputElement>("#p-name").value.trim();
    editing.instructions = $<HTMLTextAreaElement>("#p-instructions").value;
    editing.updatedAt = Date.now();
    try {
      await api.saveProject(editing);
      const saved = editing;
      await loadProjects();
      dlg().close();
      onChange(saved);
    } catch (err) {
      $("#project-error").textContent = errMsg(err);
      $("#project-error").hidden = false;
    }
  });
  $("#p-delete").addEventListener("click", async (e) => {
    const btn = e.currentTarget as HTMLElement;
    if (!editing) return;
    if (!btn.classList.contains("confirm")) {
      btn.classList.add("confirm");
      btn.textContent = "Confirmer : les conversations sont conservées, sans projet";
      return;
    }
    const id = editing.id;
    try {
      await api.deleteProject(id);
      await loadProjects();
      dlg().close();
      onChange(null, id);
    } catch (err) {
      notify(errMsg(err), true);
    }
  });
  dlg().addEventListener("close", () => (editing = null));
}
