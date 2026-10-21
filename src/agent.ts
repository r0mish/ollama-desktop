// Logique « agent » indépendante du DOM : invites système, conversion vers l'API,
// politique d'approbation et rendu des cartes d'outils.
import type { ApiMessage, Conversation, InstructionFile, PermissionMode, Project, Risk, Settings, ToolCallRec } from "./api";
import { escapeHtml } from "./markdown";

const today = () =>
  new Date().toLocaleDateString("fr-FR", { weekday: "long", day: "numeric", month: "long", year: "numeric" });

export interface Platform {
  os: string;
  shell: string;
  shellGuidance?: string;
  /** Serveur SSH du dossier de projet, le cas échéant. */
  remote?: string;
}

export function codeSystemPrompt(workspace: string, p: Platform): string {
  return `You are an expert software engineering agent working directly in the user's project.
Project root: ${workspace}${p.remote ? ` — on the REMOTE server ${p.remote} (all tools operate on that server over SSH, not on the user's computer)` : ""}
Operating system: ${p.os}. Today: ${today()}.
Shell used by run_command: ${p.shell}. ${p.shellGuidance ?? ""}

How to work:
- Explore before changing anything: use list_files, search_files and read_file. Never guess file contents.
- Make focused, minimal changes with edit_file. old_string must be copied exactly from read_file output, WITHOUT the line-number prefix. Use write_file only for new files or complete rewrites.
- Follow the project's existing style, conventions and libraries.
- After modifying code, run the relevant build, linter or tests with run_command when the project has them, and fix what fails.
- Commands are non-interactive and time-limited: never start servers, watchers or REPLs; pass flags such as --yes when needed.
- Never run destructive commands (rm -rf, git reset --hard, git push --force, dropping databases, restarting services…) unless the user explicitly asked for it.
- Use git_status, git_diff and git_log to inspect version control; commit or push only when the user asks.
- If the user refuses an action, stop and ask how to proceed instead of trying a workaround.
- Work autonomously until the task is fully done. Never end your turn by announcing what you are about to do ("I will now…", "Je vais…"): call the tool directly in the same turn. Only end your turn when the task is complete or when you genuinely need the user's input.
- Be concise. When finished, summarize what you changed and how you verified it.
- Always answer in the user's language (French by default).`;
}

const WEB_PROMPT = () =>
  `Tu disposes des outils web_search (recherche sur le web) et fetch_url (lecture d'une page). ` +
  `Utilise-les pour toute information récente, chiffrée ou dont tu n'es pas sûr, puis cite tes sources sous forme de liens Markdown [titre](url). ` +
  `Date du jour : ${today()}.`;

const SYSTEM_ACCESS_PROMPT = `System access is enabled for this session: list_files, read_file, search_files, write_file and edit_file also accept ABSOLUTE paths outside the project root (e.g. /etc/fail2ban/jail.local), on the same machine as the project. The user approves every change outside the project.
- Read a system file before changing it and keep the existing format and comments.
- Prefer drop-in/override files when the software supports them (e.g. jail.local, conf.d/, *.d/ directories) rather than editing package-managed defaults.
- After changing a service configuration, validate it if a check command exists (nginx -t, sshd -t, fail2ban-client -t, visudo -c…) and tell the user how to reload the service instead of restarting it yourself, unless asked.
- Writing root-owned files requires the connected user to have write permission (e.g. connected as root); if access is denied, explain it to the user.`;

/** Contexte additionnel : consignes du dossier (AGENTS.md…) et projet de la conversation. */
export interface PromptExtras {
  instructions?: InstructionFile[];
  project?: Project | null;
  /** Fichiers de référence préparés (extraits pertinents pour les gros projets). */
  projectFiles?: string;
}

export function instructionsPrompt(files: InstructionFile[]): string {
  if (!files.length) return "";
  const body = files.map((f) => `<instructions file="${f.name}">\n${f.content}\n</instructions>`).join("\n\n");
  return `Project instructions, loaded automatically from the project root. Follow them: they take precedence over your defaults (but not over the user's explicit requests). Sub-directories may contain their own AGENTS.md: read it before working there.\n\n${body}`;
}

/**
 * `files` : contenu des fichiers de référence préparé côté Rust (en entier, ou extraits pertinents
 * pour les gros projets). À défaut, les fichiers sont joints en entier.
 */
export function projectPrompt(p: Project, files?: string): string {
  const parts = [`Cette conversation fait partie du projet « ${p.name} ».`];
  if (p.instructions.trim()) parts.push(`Consignes du projet :\n${p.instructions.trim()}`);
  if (p.files.length) {
    parts.push(
      "Fichiers de référence du projet (base de connaissances : appuie-toi dessus et cite le nom du fichier quand tu t'en sers) :\n\n" +
        (files ?? p.files.map((f) => `<fichier nom="${f.name}">\n${f.text}\n</fichier>`).join("\n\n")),
    );
  }
  return parts.join("\n\n");
}

export function systemPrompt(conv: Conversation, settings: Settings, platform: Platform, extras: PromptExtras = {}): string {
  const parts: string[] = [];
  if (conv.kind === "code" && conv.workspace) {
    parts.push(codeSystemPrompt(conv.workspace, platform));
    const instr = instructionsPrompt(extras.instructions ?? []);
    if (instr) parts.push(instr);
    if (conv.systemAccess) parts.push(SYSTEM_ACCESS_PROMPT);
  }
  if (extras.project) parts.push(projectPrompt(extras.project, extras.projectFiles));
  const user = (conv.systemPrompt ?? settings.systemPrompt)?.trim();
  if (user) parts.push(user);
  if (conv.webSearch) parts.push(WEB_PROMPT());
  return parts.join("\n\n");
}

export const INIT_AGENTS_PROMPT =
  "Analyse ce projet (structure, langages, outils de build, tests, conventions) puis crée à la racine un fichier AGENTS.md concis destiné aux agents de code : " +
  "présentation en quelques lignes, commandes pour installer, compiler, lancer les tests et le linter, conventions de code et d'architecture à respecter, pièges connus. " +
  "N'invente rien : uniquement ce que tu as vérifié dans le dépôt.";

/** Au-delà de ces derniers tours d'outils, les anciens résultats sont tronqués dans les requêtes. */
const KEEP_FULL_TOOL_TURNS = 8;
const OLD_RESULT_CHARS = 1500;

export const COMPACT_PROMPT =
  "Le contexte de cette conversation devient trop long. Rédige un résumé complet qui permettra de reprendre le travail sans l'historique : " +
  "objectif de l'utilisateur et contraintes exprimées, décisions prises, fichiers lus et modifiés (chemins exacts) avec la nature des changements, " +
  "commandes lancées et leurs résultats importants, état actuel, problèmes en suspens et prochaines étapes. " +
  "Sois factuel et précis, en Markdown, sans appeler d'outil.";

export function toApiMessages(conv: Conversation, system: string): ApiMessage[] {
  const out: ApiMessage[] = [];
  if (system) out.push({ role: "system", content: system });
  const start = conv.compaction ? Math.min(conv.compaction.upto, conv.messages.length) : 0;
  let pendingSummary = conv.compaction
    ? `[Contexte compacté : résumé de la conversation précédente]\n\n${conv.compaction.summary}`
    : "";
  const msgs = conv.messages.slice(start);
  // Indice du premier tour d'outils dont les résultats restent complets.
  const toolTurns = msgs.map((m, i) => (m.toolCalls?.length ? i : -1)).filter((i) => i >= 0);
  const keepFrom = toolTurns.length > KEEP_FULL_TOOL_TURNS ? toolTurns[toolTurns.length - KEEP_FULL_TOOL_TURNS] : 0;
  if (pendingSummary && msgs[0]?.role !== "user") {
    // Compaction en cours de tâche : le résumé tient lieu de dernière consigne.
    out.push({ role: "user", content: `${pendingSummary}\n\nReprends la tâche en cours là où tu t'es arrêté.` });
    pendingSummary = "";
  }
  for (const [idx, m] of msgs.entries()) {
    if (m.role === "user") {
      // Certains gabarits de modèles refusent un message « user » juste après des résultats d'outils.
      if (out.length && out[out.length - 1].role === "tool") {
        out.push({ role: "assistant", content: "(Exécution interrompue.)" });
      }
      let content = m.content;
      for (const f of m.files ?? []) content += `\n\n<fichier nom="${f.name}">\n${f.text}\n</fichier>`;
      if (pendingSummary) {
        content = `${pendingSummary}\n\n---\n\n${content}`;
        pendingSummary = "";
      }
      out.push({ role: "user", content, images: m.images ?? [] });
      continue;
    }
    const calls = m.toolCalls ?? [];
    if (calls.length) {
      out.push({
        role: "assistant",
        content: m.content,
        toolCalls: calls.map(({ id, name, arguments: a }) => ({ id, name, arguments: a })),
      });
      for (const c of calls) {
        let content = c.result ?? "Non exécuté (interrompu).";
        if (idx < keepFrom && content.length > OLD_RESULT_CHARS) {
          content = `${content.slice(0, OLD_RESULT_CHARS)}\n[… résultat ancien tronqué pour économiser le contexte ; relance l'outil si besoin]`;
        }
        out.push({ role: "tool", toolCallId: c.id, name: c.name, content });
      }
    } else if (m.content.trim()) {
      out.push({ role: "assistant", content: m.content });
    }
  }
  return out;
}

/**
 * Faut-il demander à l'utilisateur avant d'exécuter cet appel ?
 * `outside` : le chemin sort du dossier de projet (accès système) — écritures toujours approuvées
 * (sauf mode Autonome), lectures approuvées en mode « Tout demander ».
 */
export function needsApproval(risk: Risk, name: string, mode: PermissionMode, settings: Settings, outside = false): boolean {
  if (mode === "auto") return false;
  if (outside && (risk === "write" || (risk === "read" && mode === "ask"))) return true;
  switch (risk) {
    case "read":
      return false;
    case "write":
      return mode === "ask";
    case "exec":
      return true;
    case "mcp":
      return !settings.mcpAlwaysAllow.includes(name);
  }
}

// ---------------------------------------------------------------- rendu

const parse = (s: string): Record<string, unknown> => {
  try {
    const v = JSON.parse(s || "{}");
    return v && typeof v === "object" ? v : {};
  } catch {
    return {};
  }
};

const str = (v: unknown) => (typeof v === "string" ? v : v == null ? "" : JSON.stringify(v));
const code = (s: string, max = 120) => `<code>${escapeHtml(s.length > max ? s.slice(0, max) + "…" : s)}</code>`;

const ICONS: Record<string, string> = {
  search: '<svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>',
  globe: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/></svg>',
  file: '<svg viewBox="0 0 24 24"><path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><path d="M14 3v6h6"/></svg>',
  folder: '<svg viewBox="0 0 24 24"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>',
  edit: '<svg viewBox="0 0 24 24"><path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/></svg>',
  term: '<svg viewBox="0 0 24 24"><path d="m4 17 6-5-6-5M12 19h8"/></svg>',
  plug: '<svg viewBox="0 0 24 24"><path d="M9 2v6M15 2v6M6 8h12v4a6 6 0 0 1-12 0zM12 18v4"/></svg>',
};

/** L'agent s'arrête-t-il en annonçant une action au lieu de l'exécuter ? */
export function looksUnfinished(text: string): boolean {
  const tail = text.trim().slice(-400);
  if (!tail || tail.endsWith("?")) return false;
  if (tail.endsWith(":")) return true;
  const lastPara = tail.split(/\n\s*\n/).pop() ?? tail;
  return /\b(je vais|je m'en occupe|laisse[- ]moi|commençons|procédons|passons (maintenant )?à|let me|i'll|i will|now i|next,? i|let's)\b/i.test(lastPara);
}

/** Libellé texte (sans HTML) d'un appel d'outil, pour la barre d'activité. */
export function toolLabel(tc: ToolCallRec): string {
  const div = document.createElement("div");
  div.innerHTML = toolTitle(tc).html;
  return div.textContent ?? tc.name;
}

export function toolTitle(tc: ToolCallRec): { icon: string; html: string } {
  const a = parse(tc.arguments);
  switch (tc.name) {
    case "web_search":
      return { icon: ICONS.search, html: `Recherche web ${code(str(a.query))}` };
    case "fetch_url":
      return { icon: ICONS.globe, html: `Lecture de ${code(str(a.url))}` };
    case "list_files":
      return { icon: ICONS.folder, html: `Liste des fichiers ${code(str(a.path) || ".")}` };
    case "read_file":
      return { icon: ICONS.file, html: `Lecture ${code(str(a.path))}` };
    case "search_files":
      return { icon: ICONS.search, html: `Recherche ${code(str(a.pattern))}${a.glob ? ` dans ${code(str(a.glob))}` : ""}` };
    case "write_file":
      return { icon: ICONS.edit, html: `Écriture ${code(str(a.path))}` };
    case "edit_file":
      return { icon: ICONS.edit, html: `Modification ${code(str(a.path))}` };
    case "run_command":
      return { icon: ICONS.term, html: code(`$ ${str(a.command)}`, 200) };
    default: {
      const label = tc.name.replace(/^mcp_/, "").replace(/_/, " › ");
      return { icon: ICONS.plug, html: `MCP ${code(label)}` };
    }
  }
}

export function diffHtml(diff: string): string {
  const lines = diff.split("\n").map((l) => {
    const cls = l.startsWith("+++") || l.startsWith("---") ? "meta" : l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : l.startsWith("@@") ? "hunk" : "";
    return `<span class="${cls}">${escapeHtml(l) || " "}</span>`;
  });
  return `<pre class="diff">${lines.join("\n")}</pre>`;
}

const STATUS_LABEL: Record<ToolCallRec["status"], string> = {
  queued: "",
  pending: "En attente d'approbation",
  running: "En cours…",
  done: "",
  error: "Erreur",
  denied: "Refusé",
};

export function toolCardHtml(tc: ToolCallRec, risk: Risk | undefined): string {
  const { icon, html } = toolTitle(tc);
  const args = parse(tc.arguments);
  const showArgs = tc.name.startsWith("mcp_") || !["read_file", "list_files", "run_command"].includes(tc.name);
  const argsBlock =
    showArgs && Object.keys(args).length && tc.name !== "write_file" && tc.name !== "edit_file"
      ? `<div class="tool-label">Arguments</div><pre>${escapeHtml(JSON.stringify(args, null, 2))}</pre>`
      : "";
  const result = tc.result ? `<div class="tool-label">Résultat</div><pre>${escapeHtml(tc.result.length > 20000 ? tc.result.slice(0, 20000) + "\n…" : tc.result)}</pre>` : "";
  const details = argsBlock || result ? `<details${tc.status === "error" ? " open" : ""}><summary>Détails</summary>${argsBlock}${result}</details>` : "";
  const diff = tc.diff ? diffHtml(tc.diff) : "";
  let approval = "";
  if (tc.status === "pending") {
    const always =
      risk === "mcp"
        ? `<button class="btn-ghost sm" data-approve="always">Toujours autoriser cet outil</button>`
        : risk === "write" && !tc.outside
          ? `<button class="btn-ghost sm" data-approve="always">Accepter toutes les modifications</button>`
          : "";
    const question = tc.outside
      ? `${risk === "write" ? "Modifier" : "Lire"} <strong>hors du projet</strong> : ${code(tc.outside, 200)} ?`
      : risk === "exec" ? "Exécuter cette commande ?" : risk === "write" ? "Appliquer cette modification ?" : "Autoriser cet appel ?";
    approval = `<div class="tool-approve${tc.outside ? " outside" : ""}">
      <span>${question}</span>
      <span class="row">
        <button class="btn-ghost sm danger" data-approve="deny">Refuser</button>
        ${always}
        <button class="btn-primary sm" data-approve="allow">Autoriser</button>
      </span>
    </div>`;
  }
  const spinner = tc.status === "running" ? '<span class="spinner"></span>' : "";
  return `<div class="tool ${tc.status}" data-tc="${escapeHtml(tc.id)}">
    <div class="tool-head">${icon}<span class="tool-title">${html}</span>${tc.outside ? '<span class="badge-outside" title="Fichier en dehors du dossier de projet (accès système)">hors projet</span>' : ""}${spinner}<span class="tool-status">${STATUS_LABEL[tc.status]}</span></div>
    ${diff}${details}${approval}
  </div>`;
}
