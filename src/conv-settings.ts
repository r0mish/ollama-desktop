// Réglages propres à une conversation : consignes, température, contexte, raisonnement.
import type { ConvParams, Settings } from "./api";
import { escapeHtml } from "./markdown";
import { togglePopover } from "./popover";

interface Deps {
  button: HTMLButtonElement;
  settings: () => Settings;
  get: () => { systemPrompt: string | null | undefined; params: ConvParams | undefined; thinkingModel: boolean; maxContext: number | null };
  set: (systemPrompt: string | null, params: ConvParams | undefined) => void;
}

let deps: Deps;

const CTX = [4096, 8192, 16384, 32768, 65536, 131072];
const fmtCtx = (n: number) => (n ? `${Math.round(n / 1024)} k` : "défaut du serveur");

/** Des réglages propres à la conversation sont-ils définis ? */
export function isCustomized(systemPrompt: string | null | undefined, params: ConvParams | undefined, s: Settings): boolean {
  const p = params ?? {};
  return p.temperature != null || p.numCtx != null || p.think != null || (!!systemPrompt && systemPrompt !== s.systemPrompt);
}

function open() {
  const cur = deps.get();
  const s = deps.settings();
  const el = togglePopover(deps.button, "cs", () => commit());
  if (!el) return;
  const p = cur.params ?? {};
  const max = cur.maxContext;
  el.innerHTML = `<div class="cs-head"><strong>Réglages de cette conversation</strong>
      <button type="button" class="btn-ghost sm" data-reset title="Revenir aux réglages globaux">Réinitialiser</button></div>
    <label>Instructions système
      <textarea id="cs-system" rows="4" placeholder="${escapeHtml(s.systemPrompt ? `Réglage global : ${s.systemPrompt.slice(0, 120)}` : "Aucune (réglage global)")}"></textarea>
    </label>
    <div class="grid2">
      <label>Température
        <input id="cs-temp" type="number" min="0" max="2" step="0.1" placeholder="${s.temperature == null ? "défaut du modèle" : `global : ${s.temperature}`}" />
      </label>
      <label>Fenêtre de contexte
        <select id="cs-ctx">
          <option value="">Réglage global (${fmtCtx(s.numCtx)})</option>
          ${CTX.filter((n) => !max || n <= max || n === p.numCtx).map((n) => `<option value="${n}">${fmtCtx(n)}</option>`).join("")}
        </select>
      </label>
    </div>
    <label${cur.thinkingModel ? "" : ' class="disabled" title="Le modèle choisi ne gère pas le raisonnement"'}>Raisonnement
      <select id="cs-think"${cur.thinkingModel ? "" : " disabled"}>
        <option value="">Réglage global (${s.thinking ? "activé" : "désactivé"})</option>
        <option value="on">Activé</option>
        <option value="off">Désactivé</option>
      </select>
    </label>
    <p class="muted small">S'applique aux prochains messages de cette conversation uniquement. Plus de contexte consomme plus de mémoire.</p>`;
  const q = <T extends HTMLElement>(sel: string) => el.querySelector(sel) as T;
  const system = q<HTMLTextAreaElement>("#cs-system");
  const temp = q<HTMLInputElement>("#cs-temp");
  const ctx = q<HTMLSelectElement>("#cs-ctx");
  const think = q<HTMLSelectElement>("#cs-think");
  system.value = cur.systemPrompt && cur.systemPrompt !== s.systemPrompt ? cur.systemPrompt : "";
  temp.value = p.temperature != null ? String(p.temperature) : "";
  ctx.value = p.numCtx != null ? String(p.numCtx) : "";
  think.value = p.think == null ? "" : p.think ? "on" : "off";

  let last = "";
  function commit() {
    const params: ConvParams = {};
    const t = temp.value.trim() === "" ? NaN : Number(temp.value.replace(",", "."));
    if (Number.isFinite(t)) params.temperature = Math.min(2, Math.max(0, t));
    if (ctx.value) params.numCtx = Number(ctx.value);
    if (think.value) params.think = think.value === "on";
    const prompt = system.value.trim() || null;
    const key = JSON.stringify([prompt, params]);
    if (key === last) return;
    last = key;
    deps.set(prompt, Object.keys(params).length ? params : undefined);
  }
  last = JSON.stringify([system.value.trim() || null, cur.params ?? {}]);
  for (const input of [system, temp, ctx, think]) input.addEventListener("change", commit);
  q<HTMLButtonElement>("[data-reset]").addEventListener("click", () => {
    system.value = temp.value = ctx.value = think.value = "";
    commit();
  });
}

export function initConvSettings(d: Deps) {
  deps = d;
  d.button.addEventListener("click", open);
}
