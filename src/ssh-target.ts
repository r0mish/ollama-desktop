// Analyse d'une cible SSH collée dans le champ « Hôte » :
// `user@hôte`, `user@hôte:2222`, `ssh://user@hôte:2222`, `ssh -p 2222 user@hôte`…

export interface SshTarget {
  host: string;
  user?: string;
  port?: number;
}

const validPort = (p: string | undefined) => {
  const n = Number(p);
  return p && /^\d+$/.test(p) && n > 0 && n < 65536 ? n : undefined;
};

/** Renvoie null si le texte est déjà un simple nom d'hôte (rien à répartir). */
export function parseTarget(input: string): SshTarget | null {
  let text = input.trim();
  let user: string | undefined;
  let port: number | undefined;

  // Ligne de commande : ssh [-p port] [-l user] [autres options] cible
  if (/^ssh\s/.test(text)) {
    const words = text.split(/\s+/).slice(1);
    let target = "";
    for (let i = 0; i < words.length; i++) {
      const w = words[i];
      if (w === "-p") port = validPort(words[++i]) ?? port;
      else if (/^-p\d+$/.test(w)) port = validPort(w.slice(2)) ?? port;
      else if (w === "-l") user = words[++i] ?? user;
      else if (/^-[iJoFbcDELmOQRSWw]$/.test(w)) i++; // option avec argument
      else if (!w.startsWith("-") && !target) target = w;
    }
    if (!target) return null;
    text = target;
  }

  text = text.replace(/^ssh:\/\//, "").replace(/\/$/, "");
  const at = text.lastIndexOf("@");
  if (at > 0) {
    user = text.slice(0, at);
    text = text.slice(at + 1);
  }
  // IPv6 entre crochets : [::1]:2222
  const bracket = text.match(/^\[([^\]]+)\](?::(\d+))?$/);
  if (bracket) {
    text = bracket[1];
    port = validPort(bracket[2]) ?? port;
  } else if ((text.match(/:/g) ?? []).length === 1) {
    const [h, p] = text.split(":");
    const n = validPort(p);
    if (n) {
      text = h;
      port = n;
    }
  }
  if (!text || /\s/.test(text)) return null;
  if (text === input.trim() && !user && !port) return null;
  return { host: text, ...(user ? { user } : {}), ...(port ? { port } : {}) };
}
