// Mini-fenêtre de saisie rapide : Entrée envoie la question à la fenêtre principale,
// Échap ou la perte du focus la masque.
import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { api } from "./api";

const input = document.querySelector<HTMLInputElement>("#quick-input")!;
const hide = (blurred = false) => invoke("quick_hide", { blurred }).catch(() => {});

async function applyAppearance() {
  const st = await api.getSettings().catch(() => null);
  if (!st) return;
  const root = document.documentElement;
  if (st.theme === "system") root.removeAttribute("data-theme");
  else root.dataset.theme = st.theme;
  if (!st.accent || st.accent === "teal") root.removeAttribute("data-accent");
  else root.dataset.accent = st.accent;
}

document.querySelector("#quick-form")!.addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = input.value.trim();
  if (!text) return hide();
  input.value = "";
  await invoke("quick_submit", { text }).catch(() => {});
});
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    input.value = "";
    hide();
  }
});
window.addEventListener("blur", () => hide(true));
// À chaque réaffichage : apparence à jour (elle a pu changer dans les réglages) et champ prêt.
window.addEventListener("focus", () => {
  applyAppearance();
  input.focus();
  input.select();
});
applyAppearance();
