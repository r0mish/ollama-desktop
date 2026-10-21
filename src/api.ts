import { Channel, invoke } from "@tauri-apps/api/core";

export interface Settings {
  /** Serveur Ollama (local, réseau privé, ou distant en HTTPS). */
  baseUrl: string;
  /** Vide = premier modèle installé. */
  defaultModel: string;
  systemPrompt: string;
  temperature: number | null;
  maxTokens: number | null;
  /** Fenêtre de contexte demandée au serveur (0 = défaut d'Ollama). */
  numCtx: number;
  /** Maintien en mémoire après une requête (« 30m », « -1m »…) ; vide = défaut d'Ollama. */
  keepAlive: string;
  /** Raisonnement des modèles qui le gèrent. */
  thinking: boolean;
  theme: "system" | "light" | "dark";
  sendOnEnter: boolean;
  searxngUrl: string;
  webSearchDefault: boolean;
  /** Vide = premier modèle gérant les outils. */
  codeModel: string;
  commandTimeout: number;
  mcpAlwaysAllow: string[];
  maxSteps: number;
  autoContinue: boolean;
  autoCompactPercent: number;
  checkUpdates: boolean;
  skippedVersion: string;
  shell: "auto" | "bash" | "pwsh" | "powershell";
  /** Serveur compatible /v1/audio/transcriptions ; vide = dictée désactivée. */
  transcriptionUrl: string;
  transcriptionModel: string;
  /** Code langue ISO (« fr »…) ; vide = détection automatique. */
  transcriptionLanguage: string;
  /** Termes à favoriser (un par ligne ou séparés par des virgules). */
  transcriptionVocabulary: string;
  encryptData: boolean;
  settingsVersion?: number;
}

export interface ModelInfo {
  id: string;
  family: string | null;
  parameterSize: string | null;
  quantization: string | null;
  /** Octets sur disque. */
  size: number;
  modifiedAt: string | null;
  vision: boolean;
  tools: boolean;
  thinking: boolean;
  maxContextLength: number | null;
  remote: boolean;
}

export interface RunningModel {
  name: string;
  size: number;
  sizeVram: number;
  expiresAt: string | null;
  contextLength: number | null;
}

export interface ServerStatus {
  url: string;
  version: string | null;
  error: string | null;
  /** HTTP en clair hors de cette machine. */
  insecure: boolean;
}

export interface PullProgress {
  status: string;
  digest: string | null;
  total: number | null;
  completed: number | null;
}

export interface TextFile {
  name: string;
  text: string;
}

export type ToolStatus = "queued" | "pending" | "running" | "done" | "error" | "denied";

export interface ToolCallRec {
  id: string;
  name: string;
  arguments: string;
  status: ToolStatus;
  result?: string;
  diff?: string;
  /** Chemin absolu hors du dossier de projet (accès système). */
  outside?: string;
}

export interface Message {
  id: string;
  role: "user" | "assistant";
  content: string;
  createdAt: number;
  images?: string[];
  files?: TextFile[];
  reasoning?: string;
  model?: string;
  usage?: { promptTokens: number; completionTokens: number; tokensPerSec?: number | null };
  error?: string;
  cancelled?: boolean;
  toolCalls?: ToolCallRec[];
  /** Relance envoyée automatiquement par l'application. */
  auto?: boolean;
}

export interface Compaction {
  summary: string;
  /** Les messages d'indice < upto sont remplacés par le résumé dans les requêtes. */
  upto: number;
  at: number;
}

export interface Usage {
  promptTokens: number;
  completionTokens: number;
}

export type Kind = "chat" | "code";
export type PermissionMode = "ask" | "acceptEdits" | "auto";

export interface Conversation {
  id: string;
  title: string;
  kind: Kind;
  model: string;
  createdAt: number;
  updatedAt: number;
  systemPrompt: string | null;
  messages: Message[];
  workspace?: string;
  permissionMode?: PermissionMode;
  webSearch?: boolean;
  mcp?: boolean;
  /** Mode Code : fichiers hors du dossier de projet autorisés (avec approbation). */
  systemAccess?: boolean;
  compaction?: Compaction;
  /** Consommation hors messages (compactions), par modèle. */
  extraUsage?: Record<string, Usage>;
  projectId?: string;
}

export interface ConversationSummary {
  id: string;
  title: string;
  updatedAt: number;
  kind: Kind;
  workspace: string | null;
  projectId: string | null;
}

export interface ProjectFile {
  name: string;
  text: string;
}

export interface Project {
  id: string;
  name: string;
  instructions: string;
  files: ProjectFile[];
  /** Dossier de travail des sessions de code : chemin local ou `ssh:<hôte>:<chemin>`. */
  workspace?: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface SearchHit {
  id: string;
  snippet: string | null;
}

export interface ProjectContext {
  mode: "full" | "excerpts";
  text: string;
}

export interface VaultStatus {
  enabled: boolean;
  hasKey: boolean;
  error: string | null;
}

export interface InstructionFile {
  name: string;
  content: string;
  truncated: boolean;
}

export interface KeyStatus {
  configured: boolean;
  source: "keyring" | "env" | null;
}

export type StreamEvent =
  | { type: "delta"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "usage"; promptTokens: number; completionTokens: number; tokensPerSec: number | null }
  | { type: "toolCalls"; calls: { id: string; name: string; arguments: string }[] }
  | { type: "retry"; attempt: number; maxAttempts: number; delaySecs: number; reason: string }
  | { type: "done"; finishReason: string | null; cancelled: boolean };

export interface ApiMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  images?: string[];
  toolCalls?: { id: string; name: string; arguments: string }[];
  toolCallId?: string;
  name?: string;
}

export interface ToolContext {
  workspace?: string;
  web: boolean;
  mcp: boolean;
  system?: boolean;
  /** Projet volumineux : outil search_project_files. */
  project?: string;
}

export type Risk = "read" | "write" | "exec" | "mcp";

export interface ToolDef {
  definition: { type: "function"; function: { name: string; description: string; parameters: unknown } };
  risk: Risk;
}

export interface ToolOutput {
  content: string;
  isError: boolean;
  diff: string | null;
}

export interface McpServerStatus {
  name: string;
  state: "starting" | "ready" | "error" | "disabled";
  error: string | null;
  transport: "stdio" | "http";
  protocol: string | null;
  tools: { exposed: string; name: string; description: string }[];
}

export interface UpdateInfo {
  current: string;
  latest: string;
  available: boolean;
  releaseUrl: string;
  assetName: string | null;
  assetUrl: string | null;
  notes: string;
}

export interface SshHost {
  id: string;
  label: string;
  host: string;
  port?: number | null;
  user?: string | null;
  identityFile?: string | null;
  system?: string | null;
}

export interface SshTestResult {
  ok: boolean;
  error: string | null;
  needsTrust: boolean;
  fingerprints: string[];
  system: string | null;
  user: string | null;
  home: string | null;
  git: string | null;
}

export interface GitFile {
  path: string;
  origPath: string | null;
  index: string;
  worktree: string;
}

export interface GitStatus {
  isRepo: boolean;
  branch: string | null;
  upstream: string | null;
  ahead: number;
  behind: number;
  files: GitFile[];
}

export interface GitCommit {
  hash: string;
  author: string;
  date: string;
  subject: string;
}

/** jour → modèle → [entrée, sortie, requêtes] */
export type UsageLog = Record<string, Record<string, [number, number, number]>>;

export const api = {
  keyStatus: () => invoke<KeyStatus>("api_key_status"),
  setKey: (key: string) => invoke<void>("set_api_key", { key }),
  deleteKey: () => invoke<void>("delete_api_key"),
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
  serverStatus: () => invoke<ServerStatus>("server_status"),
  listModels: () => invoke<ModelInfo[]>("list_models"),
  runningModels: () => invoke<RunningModel[]>("running_models"),
  pullModel(requestId: string, model: string, onProgress: (p: PullProgress) => void): Promise<boolean> {
    const channel = new Channel<PullProgress>();
    channel.onmessage = onProgress;
    return invoke<boolean>("pull_model", { requestId, model, onProgress: channel });
  },
  deleteModel: (model: string) => invoke<void>("delete_model", { model }),
  unloadModel: (model: string) => invoke<void>("unload_model", { model }),
  preloadModel: (model: string, numCtx: number | null) => invoke<void>("preload_model", { model, numCtx }),
  listConversations: () => invoke<ConversationSummary[]>("list_conversations"),
  loadConversation: (id: string) => invoke<Conversation>("load_conversation", { id }),
  saveConversation: (conversation: Conversation) => invoke<void>("save_conversation", { conversation }),
  deleteConversation: (id: string) => invoke<void>("delete_conversation", { id }),
  exportConversation: (id: string) => invoke<boolean>("export_conversation", { id }),
  dataDir: () => invoke<string>("data_dir"),
  cancel: (requestId: string) => invoke<void>("cancel_stream", { requestId }),
  platform: () =>
    invoke<{ os: string; shell: string; shellPath: string; shellGuidance: string; version: string }>("platform_info"),
  recordUsage: (day: string, model: string, promptTokens: number, completionTokens: number) =>
    invoke<void>("record_usage", { day, model, promptTokens, completionTokens }),
  getUsage: () => invoke<UsageLog>("get_usage"),
  checkUpdate: () => invoke<UpdateInfo>("check_update"),

  sshHosts: () => invoke<SshHost[]>("ssh_hosts"),
  sshSaveHost: (host: SshHost) => invoke<SshHost>("ssh_save_host", { host }),
  sshDeleteHost: (id: string) => invoke<void>("ssh_delete_host", { id }),
  sshTest: (host: SshHost) => invoke<SshTestResult>("ssh_test", { host }),
  sshTrust: (host: SshHost) => invoke<void>("ssh_trust", { host }),
  sshListDirs: (hostId: string, path: string) => invoke<{ path: string; dirs: string[] }>("ssh_list_dirs", { hostId, path }),
  sshOpenWorkspace: (hostId: string, path: string) => invoke<string>("ssh_open_workspace", { hostId, path }),

  gitStatus: (workspace: string) => invoke<GitStatus>("git_status", { workspace }),
  gitDiff: (workspace: string, path: string | null, staged: boolean, untracked: boolean) =>
    invoke<string>("git_diff", { workspace, path, staged, untracked }),
  gitStage: (workspace: string, paths: string[]) => invoke<void>("git_stage", { workspace, paths }),
  gitUnstage: (workspace: string, paths: string[]) => invoke<void>("git_unstage", { workspace, paths }),
  gitDiscard: (workspace: string, tracked: string[], untracked: string[]) => invoke<void>("git_discard", { workspace, tracked, untracked }),
  gitCommit: (workspace: string, message: string) => invoke<string>("git_commit", { workspace, message }),
  gitSync: (workspace: string, action: "push" | "pull" | "fetch") => invoke<string>("git_sync", { workspace, action }),
  gitBranches: (workspace: string) => invoke<string[]>("git_branches", { workspace }),
  gitCheckout: (workspace: string, branch: string, create: boolean) => invoke<void>("git_checkout", { workspace, branch, create }),
  gitLog: (workspace: string, count: number) => invoke<GitCommit[]>("git_log", { workspace, count }),
  gitInit: (workspace: string) => invoke<void>("git_init", { workspace }),

  workspaceInstructions: (workspace: string) => invoke<InstructionFile[]>("workspace_instructions", { workspace }),

  listProjects: () => invoke<Project[]>("list_projects"),
  saveProject: (project: Project) => invoke<void>("save_project", { project }),
  deleteProject: (id: string) => invoke<void>("delete_project", { id }),

  dictationStart(onLevel: (level: number) => void): Promise<void> {
    const channel = new Channel<number>();
    channel.onmessage = onLevel;
    return invoke<void>("dictation_start", { onLevel: channel });
  },
  dictationStop: (transcribe: boolean) => invoke<string | null>("dictation_stop", { transcribe }),

  searchConversations: (query: string) => invoke<SearchHit[]>("search_conversations", { query }),
  projectContext: (id: string, query: string) => invoke<ProjectContext>("project_context", { id, query }),

  vaultStatus: () => invoke<VaultStatus>("vault_status"),
  vaultExportKey: () => invoke<string>("vault_export_key"),
  vaultImportKey: (key: string) => invoke<void>("vault_import_key", { key }),

  updaterSupported: () => invoke<boolean>("updater_supported"),
  installUpdate(onProgress: (p: { done: number; total: number | null }) => void): Promise<void> {
    const channel = new Channel<{ done: number; total: number | null }>();
    channel.onmessage = onProgress;
    return invoke<void>("install_update", { onProgress: channel });
  },

  toolDefinitions: (ctx: ToolContext) => invoke<ToolDef[]>("tool_definitions", { ctx }),
  toolOutside: (ctx: ToolContext, name: string, args: string) =>
    invoke<string | null>("tool_outside", { ctx, name, arguments: args }),
  previewTool: (ctx: ToolContext, name: string, args: string) =>
    invoke<string | null>("preview_tool", { ctx, name, arguments: args }),
  runTool: (requestId: string, ctx: ToolContext, name: string, args: string) =>
    invoke<ToolOutput>("run_tool", { requestId, ctx, name, arguments: args }),

  pickWorkspace: () => invoke<string | null>("pick_workspace"),
  listWorkspaces: () => invoke<string[]>("list_workspaces"),
  forgetWorkspace: (path: string) => invoke<string[]>("forget_workspace", { path }),
  touchWorkspace: (path: string) => invoke<void>("touch_workspace", { path }),

  processDocument: (dataBase64: string) => invoke<{ text: string; method: "local" }>("process_document", { dataBase64 }),

  mcpGetConfig: () => invoke<string>("mcp_get_config"),
  mcpSaveConfig: (text: string) => invoke<void>("mcp_save_config", { text }),
  mcpRestart: () => invoke<void>("mcp_restart"),
  mcpStatus: () => invoke<McpServerStatus[]>("mcp_status"),

  chat(
    requestId: string,
    request: {
      model: string;
      messages: ApiMessage[];
      temperature: number | null;
      maxTokens: number | null;
      tools: ToolDef["definition"][];
      toolChoice?: "auto" | "none";
      numCtx?: number | null;
      think?: boolean | null;
    },
    onEvent: (e: StreamEvent) => void,
  ): Promise<void> {
    const channel = new Channel<StreamEvent>();
    channel.onmessage = onEvent;
    return invoke<void>("chat_stream", { requestId, request, onEvent: channel });
  },
};
