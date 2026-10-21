// Comptage des tokens et estimation du contexte.
import type { ApiMessage, Conversation, Usage, UsageLog } from "./api";

/** Tokens cumulés d'une conversation (toutes les requêtes, compactions comprises). */
export function conversationUsage(conv: Conversation) {
  let promptTokens = 0;
  let completionTokens = 0;
  const add = (u: Usage) => {
    promptTokens += u.promptTokens;
    completionTokens += u.completionTokens;
  };
  for (const m of conv.messages) if (m.role === "assistant" && m.usage) add(m.usage);
  for (const u of Object.values(conv.extraUsage ?? {})) add(u);
  return { promptTokens, completionTokens };
}

/** Taille approximative (en tokens) d'une liste de messages : ~3,5 caractères par token. */
export const estimateTokens = (messages: ApiMessage[]) =>
  Math.round(messages.reduce((n, m) => n + m.content.length + JSON.stringify(m.toolCalls ?? "").length + 12, 0) / 3.5);

/** Tokens occupés dans la fenêtre de contexte au prochain appel. */
export function contextTokens(conv: Conversation, apiMessages: () => ApiMessage[]): { tokens: number; exact: boolean } {
  const upto = conv.compaction?.upto ?? 0;
  for (let i = conv.messages.length - 1; i >= upto; i--) {
    const m = conv.messages[i];
    if (m.role === "assistant" && m.usage) {
      // Messages ajoutés depuis (résultats d'outils, nouvelle question) : estimés.
      const tail = conv.messages.slice(i + 1).reduce((n, x) => n + x.content.length + (x.toolCalls ?? []).reduce((k, t) => k + (t.result?.length ?? 0), 0), 0);
      const own = (m.toolCalls ?? []).reduce((k, t) => k + (t.result?.length ?? 0), 0);
      return { tokens: m.usage.promptTokens + m.usage.completionTokens + Math.round((tail + own) / 3.5), exact: tail + own === 0 };
    }
  }
  return { tokens: estimateTokens(apiMessages()), exact: false };
}

export const fmtTokens = (n: number) =>
  n >= 1e6 ? `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1).replace(".", ",")} M` : n >= 1000 ? `${(n / 1000).toFixed(n >= 1e5 ? 0 : 1).replace(".", ",")} k` : String(n);

export const fmtSpeed = (tps: number | null | undefined) => (tps ? `${tps.toFixed(tps < 10 ? 1 : 0).replace(".", ",")} tok/s` : "");

export function fmtDuration(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${String(s % 60).padStart(2, "0")} s`;
  return `${Math.floor(m / 60)} h ${String(m % 60).padStart(2, "0")}`;
}

export const today = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
};

/** Agrège le journal sur les `days` derniers jours (0 = tout). */
export function aggregate(log: UsageLog, days: number) {
  const since = days ? new Date(Date.now() - (days - 1) * 86400000) : null;
  const sinceKey = since
    ? `${since.getFullYear()}-${String(since.getMonth() + 1).padStart(2, "0")}-${String(since.getDate()).padStart(2, "0")}`
    : "";
  const byModel = new Map<string, [number, number, number]>();
  for (const [day, models] of Object.entries(log)) {
    if (day < sinceKey) continue;
    for (const [model, [p, c, r]] of Object.entries(models)) {
      const cur = byModel.get(model) ?? [0, 0, 0];
      byModel.set(model, [cur[0] + p, cur[1] + c, cur[2] + r]);
    }
  }
  const rows = [...byModel.entries()]
    .sort((a, b) => b[1][0] + b[1][1] - (a[1][0] + a[1][1]))
    .map(([model, [p, c, r]]) => ({ model, prompt: p, completion: c, requests: r }));
  const total = rows.reduce((n, r) => n + r.prompt + r.completion, 0);
  return { rows, total };
}
