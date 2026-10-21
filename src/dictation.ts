// Dictée vocale : capture native du micro, transcription par un serveur compatible
// /v1/audio/transcriptions (Speaches/faster-whisper, whisper.cpp, Voxtral via vLLM…).
import { api } from "./api";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

let phase: "idle" | "recording" | "transcribing" = "idle";
let started = 0;
let timer = 0;
let input: HTMLTextAreaElement;
let notify: (msg: string, error?: boolean) => void = () => {};

const errMsg = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e));

export const dictationActive = () => phase !== "idle";

function render() {
  const btn = $<HTMLButtonElement>("#mic-btn");
  btn.classList.toggle("recording", phase === "recording");
  btn.classList.toggle("transcribing", phase === "transcribing");
  btn.disabled = phase === "transcribing";
  btn.title =
    phase === "recording"
      ? "Terminer et transcrire (Ctrl+Maj+D) · Échap pour annuler"
      : phase === "transcribing"
        ? "Transcription en cours…"
        : "Dictée vocale (Ctrl+Maj+D)";
  $("#dictation").hidden = phase !== "recording";
  $("#hint").hidden = phase === "recording";
}

function renderTime() {
  const s = Math.floor((Date.now() - started) / 1000);
  $("#dictation-time").textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function setLevel(level: number) {
  const pct = Math.min(100, Math.round(Math.sqrt(Math.max(0, level)) * 140));
  $<HTMLElement>("#dictation .rec-level i").style.width = `${pct}%`;
}

/** Insère le texte au curseur, avec une espace de séparation si besoin. */
function insertText(text: string) {
  const { selectionStart: a, selectionEnd: b, value } = input;
  const before = value.slice(0, a);
  const sep = before && !/\s$/.test(before) ? " " : "";
  input.value = before + sep + text + value.slice(b);
  const pos = a + sep.length + text.length;
  input.focus();
  input.setSelectionRange(pos, pos);
  input.dispatchEvent(new Event("input"));
}

/** Démarrage en cours : l'arrêt l'attend pour ne jamais laisser le micro ouvert. */
let starting: Promise<boolean> = Promise.resolve(true);

async function start() {
  phase = "recording";
  setLevel(0);
  render();
  starting = api
    .dictationStart((level) => {
      if (level < 0) {
        notify("Durée maximale de dictée atteinte : transcription…");
        stop(true);
      } else {
        setLevel(level);
      }
    })
    .then(
      () => true,
      (e) => {
        notify(errMsg(e), true);
        return false;
      },
    );
  const ok = await starting;
  if (phase !== "recording") return;
  if (!ok) {
    phase = "idle";
    render();
    return;
  }
  started = Date.now();
  renderTime();
  clearInterval(timer);
  timer = window.setInterval(renderTime, 250);
}

async function stop(transcribe: boolean) {
  if (phase !== "recording") return;
  clearInterval(timer);
  // Bouton désactivé jusqu'à la fin, même en cas d'annulation.
  phase = "transcribing";
  render();
  if (!(await starting)) {
    phase = "idle";
    render();
    return;
  }
  try {
    const text = await api.dictationStop(transcribe);
    if (text) insertText(text);
    else if (transcribe) notify("Rien n'a été reconnu.", true);
  } catch (e) {
    notify(errMsg(e), true);
  } finally {
    phase = "idle";
    render();
  }
}

export function toggleDictation() {
  if (phase === "idle") start();
  else if (phase === "recording") stop(true);
}

/** Annule l'enregistrement en cours (Échap). Renvoie false s'il n'y en avait pas. */
export function cancelDictation(): boolean {
  if (phase !== "recording") return false;
  stop(false);
  return true;
}

export function initDictation(opts: { input: HTMLTextAreaElement; toast: (msg: string, error?: boolean) => void }) {
  input = opts.input;
  notify = opts.toast;
  $("#mic-btn").addEventListener("click", toggleDictation);
  render();
}
