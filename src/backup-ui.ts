// Sauvegarde chiffrée vers un dossier (Réglages → Général). Ces réglages s'appliquent
// aussitôt, sans attendre « Enregistrer » : « Sauvegarder maintenant » lit ce qui est sur le disque.
import { api, type Settings } from "./api";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

interface Deps {
  toast: (msg: string, error?: boolean) => void;
  settings: () => Settings;
  /** Enregistre des réglages modifiés et met à jour l'état de l'application. */
  patch: (p: Partial<Settings>) => Promise<void>;
  /** Recharge l'application après une restauration. */
  reload: () => void;
}

let deps: Deps;
const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

const dir = () => $<HTMLInputElement>("#s-backup-dir");
const auto = () => $<HTMLInputElement>("#s-backup-auto");
const keep = () => $<HTMLInputElement>("#s-backup-keep");
const pass = () => $<HTMLInputElement>("#backup-pass");

async function renderStatus() {
  const st = await api.backupStatus().catch(() => null);
  if (!st) return;
  $("#backup-pass-status").textContent = st.hasPassphrase
    ? "✓ Phrase secrète enregistrée dans le trousseau du système. Saisissez-en une autre pour la remplacer."
    : "Aucune phrase secrète : choisissez-en une pour pouvoir sauvegarder.";
  $("#backup-status").textContent = st.last ? `Dernière sauvegarde : ${new Date(st.last).toLocaleString("fr-FR", { dateStyle: "medium", timeStyle: "short" })}` : "Aucune sauvegarde pour l'instant.";
  $<HTMLButtonElement>("#backup-now").disabled = !st.hasPassphrase || !deps.settings().backupDir;
  auto().disabled = !st.hasPassphrase || !deps.settings().backupDir;
}

/** Remplit la section à l'ouverture des réglages. */
export function renderBackup() {
  const st = deps.settings();
  dir().value = st.backupDir ?? "";
  auto().checked = !!st.backupAuto;
  keep().value = String(st.backupKeep || 7);
  pass().value = "";
  renderStatus();
}

async function busy<T>(btn: HTMLButtonElement, label: string, work: () => Promise<T>): Promise<T | undefined> {
  const before = btn.textContent;
  btn.disabled = true;
  btn.textContent = label;
  try {
    return await work();
  } catch (e) {
    deps.toast(errMsg(e), true);
  } finally {
    btn.textContent = before;
    btn.disabled = false;
    renderStatus();
  }
}

export function initBackupUi(d: Deps) {
  deps = d;
  $("#backup-pick").addEventListener("click", async () => {
    try {
      const picked = await api.backupPickDir();
      if (!picked) return;
      await deps.patch({ backupDir: picked });
      dir().value = picked;
      renderStatus();
    } catch (e) {
      deps.toast(errMsg(e), true);
    }
  });
  auto().addEventListener("change", () => deps.patch({ backupAuto: auto().checked }).catch((e) => deps.toast(errMsg(e), true)));
  keep().addEventListener("change", () => {
    const n = Math.min(365, Math.max(1, Math.round(Number(keep().value) || 7)));
    keep().value = String(n);
    deps.patch({ backupKeep: n }).catch((e) => deps.toast(errMsg(e), true));
  });
  const savePass = async () => {
    try {
      await api.backupSetPassphrase(pass().value);
      pass().value = "";
      deps.toast("Phrase secrète enregistrée dans le trousseau");
      renderStatus();
    } catch (e) {
      deps.toast(errMsg(e), true);
    }
  };
  $("#backup-pass-save").addEventListener("click", savePass);
  pass().addEventListener("keydown", (e) => {
    if (e.key !== "Enter") return;
    e.preventDefault();
    savePass();
  });
  $<HTMLButtonElement>("#backup-now").addEventListener("click", (e) =>
    busy(e.currentTarget as HTMLButtonElement, "Sauvegarde…", async () => {
      const path = await api.backupNow();
      deps.toast(`Sauvegarde écrite : ${path}`);
    }),
  );
  $<HTMLButtonElement>("#backup-restore").addEventListener("click", (e) =>
    busy(e.currentTarget as HTMLButtonElement, "Restauration…", async () => {
      // Phrase saisie dans le champ (sauvegarde faite ailleurs) ou, à défaut, celle du trousseau.
      const sum = await api.backupRestore(pass().value);
      if (!sum) return;
      pass().value = "";
      deps.toast(`Restauré : ${sum.conversations} conversation(s), ${sum.projects} projet(s), ${sum.others} fichier(s) de réglages`);
      setTimeout(deps.reload, 1500);
    }),
  );
}
