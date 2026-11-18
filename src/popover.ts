// Panneau flottant ancré à un bouton (sélecteur de modèle, réglages de la conversation…).
// Un seul est ouvert à la fois ; il se ferme au clic extérieur ou avec Échap.

const panel = () => document.querySelector("#popover") as HTMLElement;

let anchor: HTMLElement | null = null;
let above = false;
let onClose: (() => void) | null = null;

function place() {
  if (!anchor) return;
  const p = panel();
  const r = anchor.getBoundingClientRect();
  const w = p.offsetWidth;
  // Aligné à droite du bouton, sans sortir de la fenêtre.
  const left = Math.max(12, Math.min(r.right - w, window.innerWidth - w - 12));
  p.style.left = `${left}px`;
  // Trop peu de place en dessous : le panneau s'ouvre vers le haut.
  const flip = !above && window.innerHeight - r.bottom < 300 && r.top > window.innerHeight - r.bottom;
  if (above || flip) {
    // Au-dessus du bouton, aligné sur son bord gauche (barre du bas) ou droit.
    if (above) p.style.left = `${Math.max(12, Math.min(r.left, window.innerWidth - w - 12))}px`;
    p.style.top = "";
    p.style.bottom = `${window.innerHeight - r.top + 6}px`;
    p.style.maxHeight = `${r.top - 24}px`;
    return;
  }
  p.style.bottom = "";
  p.style.top = `${r.bottom + 6}px`;
  p.style.maxHeight = `${window.innerHeight - r.bottom - 24}px`;
}

export const popoverAnchor = () => anchor;

export function closePopover() {
  if (!anchor) return;
  const cb = onClose;
  anchor.classList.remove("open");
  anchor = null;
  onClose = null;
  panel().hidden = true;
  cb?.();
}

/** Ouvre le panneau sous `button` (ou le ferme s'il y est déjà ouvert). Renvoie l'élément à remplir. */
export function togglePopover(button: HTMLElement, className: string, closed?: () => void, placement: "bottom" | "top" = "bottom"): HTMLElement | null {
  const same = anchor === button;
  closePopover();
  if (same) return null;
  const p = panel();
  p.className = `popover ${className}`;
  p.innerHTML = "";
  p.onclick = null;
  above = placement === "top";
  p.hidden = false;
  anchor = button;
  onClose = closed ?? null;
  button.classList.add("open");
  requestAnimationFrame(place);
  return p;
}

/** À rappeler quand le contenu change de taille. */
export const repositionPopover = () => requestAnimationFrame(place);

export function initPopover() {
  document.addEventListener("mousedown", (e) => {
    const t = e.target as HTMLElement;
    if (anchor && !t.closest("#popover") && !anchor.contains(t)) closePopover();
  });
  window.addEventListener("resize", place);
  // Capture : Échap ferme le panneau avant les autres raccourcis (arrêt de la génération…).
  window.addEventListener(
    "keydown",
    (e) => {
      if (e.key === "Escape" && anchor) {
        e.preventDefault();
        e.stopPropagation();
        const a = anchor;
        closePopover();
        a.focus();
      }
    },
    true,
  );
}
