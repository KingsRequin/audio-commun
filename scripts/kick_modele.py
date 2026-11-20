"""Apprentissage des détecteurs de kick et de caisse claire (`src/kick.rs`) : écrit `src/kick_modele.rs`.

Données : dossiers de `<nom>.f32` (mono, f32, 48 kHz) + `<nom>.kicks` et `<nom>.caisses` (instants
des vrais kicks et des vraies caisses claires ; un morceau sans `.caisses` ne sert qu'au kick),
préparés par `kick_bench.py` (MDB Drums, morceaux transcrits par ADTOF) ou par ce script :

    python kick_modele.py synth <sortie> [n=120]           boîte à rythmes synthétique, étiquettes exactes
    python kick_modele.py variantes <dossier MDB> <sortie>  4 remixages de chaque morceau de MDB (pistes séparées)
    python kick_modele.py valider <dossier>... [--leger <dossier>...]
    python kick_modele.py apprendre <kick_modele.rs> <dossier>... [--leger <dossier>...]

`--leger dossier[@poids[/poids_caisse]]` : dossiers qui servent seulement à l'apprentissage (variantes
de MDB, synthétique), chaque morceau pesant `poids` (0,25 par défaut) d'un vrai morceau, `poids_caisse`
(par défaut le même) pour la caisse claire ; `valider` ne les évalue jamais et range les variantes d'un morceau
dans le même groupe que lui. `valider` fait une validation croisée en 5 groupes de morceaux.

Les deux détecteurs jugent les mêmes attaques candidates avec les mêmes mesures ; chacun a ses trois
réseaux. Le Rust reproduit au pas de 5 ms exactement les mesures calculées ici (`levels`, `macro`,
`prep`, `candidates`, `features`) : tout changement doit être fait des deux côtés.

Dépendances : numpy, scipy, torch (GPU si possible) ; ffmpeg pour `variantes`.
"""

import pathlib
import re
import subprocess
import sys
import warnings
from multiprocessing import Pool

import numpy as np
from scipy import signal

SR = 48000
HOP_N = 240  # 5 ms
HOP = HOP_N / SR
# 10 bandes fines ; les 5 larges (sub, 90–200 Hz, 200 Hz–1 kHz, 1–4 kHz, aigus) en sont des paires.
BANDS = [(None, 50), (50, 90), (90, 140), (140, 200), (200, 450), (450, 1000), (1000, 2000), (2000, 4000), (4000, 8000), (8000, None)]
STAGES = [2, 4, 6]  # décisions 10, 20, 30 ms après l'attaque
PRE = 8
P0 = [-8.0, -11.0, -12.0, -16.0, -18.0]  # références de départ (= START_PEAK en Rust)
FPK0 = 35.0
SUBPK0 = 0.015
SILENCE_DB = -60.0
H = 32
EPOCHS = 20
LIGHT = 0.25
THRESHOLDS = [0.8, 0.7, 0.5]  # à sensibilité 5 (= STAGE_LOGIT en Rust) ; les mêmes pour la caisse claire
GAP = 0.1  # kick ; caisse claire : SNARE_GAP
SNARE_GAP = 0.05
QUIET_S, QUIET_RAISE = 4.0, 1.5  # sans kick depuis 4 s, seuils relevés (logit) ; kick seulement
KINDS = {"kick": ".kicks", "caisse": ".caisses"}


# --- mesures (identiques au Rust) ------------------------------------------------------------


def rbj(f0, high):
    w = 2 * np.pi * f0 / SR
    c, a = np.cos(w), np.sin(w) / (2 * np.sqrt(0.5))
    a0 = 1 + a
    b = [(1 + c) / 2, -(1 + c), (1 + c) / 2] if high else [(1 - c) / 2, 1 - c, (1 - c) / 2]
    return np.array(b) / a0, np.array([1, -2 * c / a0, (1 - a) / a0])


def levels(x):
    """Niveau (dB) de chaque bande fine par pas de 5 ms, fenêtre 10 ms : (10, n)."""
    n = len(x) // HOP_N
    out = []
    for lo, hi in BANDS:
        y = x
        for _ in range(2):
            if lo:
                y = signal.lfilter(*rbj(lo, True), y)
            if hi:
                y = signal.lfilter(*rbj(hi, False), y)
        blk = (y[: n * HOP_N] ** 2).reshape(n, HOP_N).mean(1)
        e = np.concatenate([[blk[0]], (blk[1:] + blk[:-1]) / 2])
        out.append(10 * np.log10(e + 1e-12))
    return np.array(out)


def macro(Lf):
    """Niveaux des 5 bandes larges (dB), somme des puissances de deux bandes fines voisines."""
    e = 10 ** (Lf / 10)
    return 10 * np.log10(e[0::2] + e[1::2])


def prep(L):
    n = L.shape[1]
    d = np.zeros_like(L)
    d[:, 2:] = np.maximum(L[:, 2:] - L[:, :-2], 0)
    F = d.sum(0)
    tot = 10 * np.log10((10 ** (L / 10)).sum(0))
    silent = tot < SILENCE_DB
    Fpk = np.empty(n)
    P = np.empty_like(L)
    cur, c = FPK0, np.array(P0)
    a = np.exp(-HOP / 2.0)
    for i in range(n):
        if not silent[i]:
            cur = max(F[i], cur * a)
            c = np.maximum(L[:, i], c - HOP * 2.0)
        Fpk[i] = cur
        P[:, i] = c
    e = 10 ** (L[0] / 10)
    mid = 10 ** (L[1] / 10) + 10 ** (L[2] / 10)
    slow, pk = 0.0, SUBPK0
    O, G = np.empty(n), np.empty(n)
    a1, a2 = min(HOP / 0.35, 1), np.exp(-HOP / 3.0)
    for i in range(n):
        exc = max(e[i] - slow, 0)
        slow += (e[i] - slow) * a1
        pk = max(exc, pk if silent[i] else pk * a2)
        O[i] = exc / pk
        G[i] = 10 * np.log10(e[i] / (mid[max(0, i - 24) : i + 1].max() + 1e-12) + 1e-9)
    return F, Fpk, P, tot, O, G


def candidates(F, Fpk, tot):
    p = np.arange(PRE + 1, len(F) - max(STAGES) - 1)
    m = (F[p] > F[p - 1]) & (F[p] >= F[p + 1]) & (F[p] >= 0.15 * Fpk[p - 1]) & (tot[p] > -70)
    return p[m]


def features(Lf, L, F, Fpk, P, tot, O, G, p, s):
    pre = np.stack([L[:, q - PRE : q - 1].min(1) for q in p], 1)
    pref = np.stack([Lf[:, q - PRE : q - 1].min(1) for q in p], 1)
    k = p + s
    f = [L[:, p + j] - pre for j in (1, 2, 4, 6) if j <= s]
    f += [L[:, k] - P[:, p - 1], L[:, k] - tot[k]]
    f += [Lf[:, p + j] - pref for j in (1, 2, 4, 6) if j <= s]  # montée de chaque bande fine
    f += [Lf[:, k] - tot[k]]  # spectre fin
    f = np.vstack(f).T
    extra = np.stack(
        [np.log(F[p] / Fpk[p - 1] + 1e-3), tot[k] - P[:, p - 1].max(0), pre[0] - P[0, p - 1], pre[1] - P[1, p - 1], O[k], G[k]], 1
    )
    return np.hstack([f, extra]).astype(np.float32)


def track(base):
    """Candidats d'un morceau : pas, mesures par étape, et pour chaque détecteur dont le morceau a
    la référence (`KINDS`) : (étiquettes des candidats, instants de référence)."""
    x = np.fromfile(base.with_suffix(".f32"), dtype="<f4").astype(float)
    Lf = levels(x)
    L = macro(Lf)
    F, Fpk, P, tot, O, G = prep(L)
    p = candidates(F, Fpk, tot)
    X = {s: features(Lf, L, F, Fpk, P, tot, O, G, p, s) for s in STAGES}
    ref = {}
    for kind, suffix in KINDS.items():
        if base.with_suffix(suffix).exists():
            with warnings.catch_warnings():  # morceau sans kick : fichier vide
                warnings.simplefilter("ignore")
                r = np.loadtxt(base.with_suffix(suffix), ndmin=1)
            ref[kind] = (np.array([np.any(np.abs(r - t) <= 0.025) for t in p * HOP], dtype=bool), r)
    return p, X, ref


def load(dirs, light):
    """`light` : {dossier: {détecteur: poids}} des dossiers d'appoint (jamais évalués)."""
    jobs = [(d, k.with_suffix(""), light.get(d)) for d in dirs for k in sorted(pathlib.Path(d).glob("*.kicks"))]
    with Pool(8) as pool:
        tracks = pool.map(track, [b for _, b, _ in jobs])
    return [(f"{pathlib.Path(d).name}/{b.name}", is_light, t) for (d, b, is_light), t in zip(jobs, tracks)]


# --- apprentissage -----------------------------------------------------------------------------


def fit(X, y, w):
    import torch

    dev = "cuda" if torch.cuda.is_available() else "cpu"
    lo, hi = np.percentile(X, 0.1, axis=0), np.percentile(X, 99.9, axis=0)
    X = np.clip(X, lo, hi)
    mu, sd = X.mean(0), X.std(0) + 1e-6
    Xt = torch.tensor((X - mu) / sd, device=dev)
    yt = torch.tensor(y, dtype=torch.float32, device=dev)
    wt = torch.tensor(w, dtype=torch.float32, device=dev)
    torch.manual_seed(0)
    net = torch.nn.Sequential(torch.nn.Linear(X.shape[1], H), torch.nn.Tanh(), torch.nn.Linear(H, 1)).to(dev)
    opt = torch.optim.Adam(net.parameters(), lr=3e-3, weight_decay=1e-5)
    for _ in range(EPOCHS):
        perm = torch.randperm(len(y), device=dev)
        for i in range(0, len(y), 4096):
            b = perm[i : i + 4096]
            loss = torch.nn.functional.binary_cross_entropy_with_logits(net(Xt[b])[:, 0], yt[b], weight=wt[b])
            opt.zero_grad()
            loss.backward()
            opt.step()
    W1 = net[0].weight.detach().cpu().numpy().astype(np.float64)
    b1 = net[0].bias.detach().cpu().numpy().astype(np.float64)
    W1f = W1 / sd[None, :]  # normalisation intégrée à la première couche
    return {
        "w": W1f,
        "b": b1 - W1f @ mu,
        "v": net[2].weight.detach().cpu().numpy()[0].astype(np.float64),
        "c": float(net[2].bias.detach().cpu().numpy()[0]),
        "lo": lo,
        "hi": hi,
    }


def logit(m, X):
    X = np.clip(X, m["lo"], m["hi"])
    return np.tanh(X @ m["w"].T + m["b"]) @ m["v"] + m["c"]


def train_all(tracks, kind):
    tracks = [t for t in tracks if kind in t[2][2]]
    y = np.concatenate([t[2][kind][0] for _, _, t in tracks]).astype(np.float32)
    # chaque morceau pèse autant, quelle que soit sa longueur ; les données légères, un quart
    w = np.concatenate([np.full(len(t[0]), (lt[kind] if lt is not None else 1.0) / (len(t[0]) + 1.0)) for _, lt, t in tracks])
    w = (w / w.mean()).astype(np.float32)
    return {s: fit(np.vstack([t[1][s] for _, _, t in tracks]), y, w) for s in STAGES}


def simulate(p, scores, refs, kind):
    det, last = [], -9.0
    thr = [np.log(t / (1 - t)) for t in THRESHOLDS]
    gap = GAP if kind == "kick" else SNARE_GAP
    for i, pi in enumerate(p):
        t = pi * HOP
        if t - last <= gap:
            continue
        quiet = QUIET_RAISE if kind == "kick" and t - last > QUIET_S else 0.0
        for k, s in enumerate(STAGES):
            if scores[s][i] > thr[k] + quiet:
                det.append((pi + s + 1) * HOP)
                last = t
                break
    used, tp = np.zeros(len(refs), bool), 0
    for h in det:
        c = np.where(~used & (h >= refs - 0.02) & (h <= refs + 0.07))[0]
        if len(c):
            used[c[np.argmin(abs(refs[c] - h))]] = True
            tp += 1
    return np.array([tp, len(det), len(refs)])


def validate(tracks):
    base = lambda n: re.sub(r"_v\d$", "", n.split("/")[1])
    evald = [n for n, lt, _ in tracks if lt is None]
    fold = dict(zip(sorted(base(n) for n in evald), np.random.default_rng(0).permutation(len(evald)) % 5))
    tot = {}
    for f in range(5):
        train = [t for t in tracks if fold.get(base(t[0]), -1) != f]
        for kind in KINDS:
            models = train_all(train, kind)
            for n, lt, (p, X, ref) in tracks:
                if lt is None and fold[base(n)] == f and kind in ref:
                    r = simulate(p, {s: logit(models[s], X[s]) for s in STAGES}, ref[kind][1], kind)
                    tot.setdefault((kind, n.split("/")[0]), np.zeros(3))
                    tot[(kind, n.split("/")[0])] += r
    for (kind, d), r in sorted(tot.items()):
        pr, rc = r[0] / max(r[1], 1), r[0] / max(r[2], 1)
        print(f"{kind:7} {d:12} précision {pr:.3f} rappel {rc:.3f} F1 {2 * pr * rc / max(pr + rc, 1e-9):.3f}")


def write_rust(models, path):
    fmt = lambda v: np.format_float_scientific(np.float32(v), unique=True, trim="0")  # plus court f32 exact
    out = [
        "// Généré par scripts/kick_modele.py (apprendre) : ne pas modifier à la main.",
        f"// Par détecteur (kick, caisse claire), trois petits réseaux (entrées → {H} tanh → 1), un par",
        "// instant de décision après l'attaque ; la normalisation des entrées est intégrée à la première couche.",
        "",
        "use crate::kick::Net;",
        "",
        f"pub(crate) const H: usize = {H};",
    ]
    for s in STAGES:
        out.append(f"pub(crate) const N{s}: usize = {models['kick'][s]['w'].shape[1]};")
    out.append("")
    for kind, s in [(k, s) for k in KINDS for s in STAGES]:
        m = models[kind][s]
        name = {"kick": "KICK", "caisse": "CAISSE"}[kind]
        out.append(f"/// {'Kick' if kind == 'kick' else 'Caisse claire'} : décision {s * 5} ms après l'attaque.")
        out.append(f"pub(crate) static {name}{s}: Net<N{s}> = Net {{")
        out.append("    w: [")
        out += ["        [" + ", ".join(fmt(v) for v in row) + "]," for row in m["w"]]
        out.append("    ],")
        for k in ("b", "v"):
            out.append(f"    {k}: [" + ", ".join(fmt(v) for v in m[k]) + "],")
        out.append(f"    c: {fmt(m['c'])},")
        for k in ("lo", "hi"):
            out.append(f"    {k}: [" + ", ".join(fmt(v) for v in m[k]) + "],")
        out.append("};")
        out.append("")
    pathlib.Path(path).write_text("\n".join(out), encoding="utf-8", newline="\n")
    print("écrit", path)


# --- données d'appoint ---------------------------------------------------------------------------


def synth_clip(rng, dur=30.0):
    """Extrait de boîte à rythmes aux paramètres tirés au hasard ; renvoie (signal, kicks, caisses).

    Pour la caisse claire, ces extraits pèsent peu (`synth@0.25/0.1`) : à 0,25 ils faisaient baisser
    sa précision sur la vraie musique (F1 0,645 → 0,621) ; sans eux, une caisse claire de boîte à
    rythmes toute simple n'était plus reconnue."""
    n = int(SR * dur)
    t = np.arange(n) / SR
    out = rng.normal(0, 10 ** rng.uniform(-3.5, -2), n)
    beat = 60 / rng.uniform(80, 175)
    step = beat / 4
    steps = int(dur / step)
    style = rng.choice(["four", "rock", "trap", "none"], p=[0.25, 0.2, 0.15, 0.4])
    kicks = []
    # kick : sinus qui descend, avec ou sans clic, saturé ou non
    f0, f1 = rng.uniform(70, 220), rng.uniform(38, 65)
    sweep, decay = rng.uniform(12, 45), rng.uniform(3, 16)
    click = rng.choice([0.0, rng.uniform(0.05, 0.6)])
    drive = rng.choice([1.0, rng.uniform(2, 8)], p=[0.7, 0.3])
    klen = int(SR * min(0.6, 5 / decay))
    tk = np.arange(klen) / SR
    kick = np.sin(2 * np.pi * (f1 * tk + (f0 - f1) * (1 - np.exp(-tk * sweep)) / sweep)) * np.exp(-tk * decay)
    if click:
        c = rng.normal(0, 1, int(SR * 0.004)) * np.exp(-np.arange(int(SR * 0.004)) / (SR * 0.001))
        kick[: len(c)] += click * np.diff(c, prepend=0)
    kick = np.tanh(kick * drive) / np.tanh(drive) * rng.uniform(0.4, 0.95)
    pattern = {"four": lambda q: q % 4 == 0, "rock": lambda q: q in (0, 8, 10), "trap": lambda q: q in (0, 6, 11)}
    for s in range(steps):
        hit = style != "none" and (pattern[style](s % 16) and rng.random() < 0.9 or rng.random() < 0.04)
        if hit:
            i = int(s * step * SR)
            j = min(n, i + klen)
            out[i:j] += kick[: j - i]
            kicks.append(i / SR)
    # caisse claire sur 2 et 4, notes fantômes, et parfois un roulement (montée d'EDM)
    body, sdec = rng.uniform(130, 260), rng.uniform(12, 35)
    noise_amt, sgain = rng.uniform(0.2, 0.8), rng.uniform(0.3, 0.9)
    slen = int(SR * 0.25)
    ts = np.arange(slen) / SR

    snares = []

    def snare(at, g):
        snares.append(at)
        i = int(at * SR)
        j = min(n, i + slen)
        snd = (rng.normal(0, noise_amt, slen) + np.sin(2 * np.pi * body * ts) * (1 - noise_amt * 0.5)) * np.exp(-ts * sdec)
        out[i:j] += g * snd[: j - i]

    roll_at, roll_every = (int(rng.uniform(0, steps * 0.6)), rng.choice([1, 2])) if rng.random() < 0.5 else (-1, 1)
    roll_len = int(rng.uniform(16, 64))
    for s in range(steps):
        if roll_at <= s < roll_at + roll_len:
            if s % roll_every == 0:
                for sub in range(2 if roll_every == 1 and rng.random() < 0.3 else 1):
                    snare((s + sub * 0.5) * step, sgain * rng.uniform(0.5, 1.0))
        elif s % 16 in (4, 12):
            snare(s * step, sgain)
        elif rng.random() < 0.04:
            snare(s * step, sgain * 0.3)
    # charleston
    hgain, hlen = rng.uniform(0.03, 0.2), int(SR * 0.05)
    th = np.arange(hlen) / SR
    for s in range(0, steps, rng.choice([1, 2])):
        i = int(s * step * SR)
        j = min(n, i + hlen)
        out[i:j] += hgain * (np.diff(rng.normal(0, 1, hlen + 1)) * np.exp(-th * rng.uniform(50, 120)))[: j - i]
    # basse tenue, parfois creusée par le kick (sidechain)
    if rng.random() < 0.7:
        notes = rng.uniform(35, 85, 8)
        nlen = beat * rng.choice([0.5, 1, 2, 3])
        att = rng.uniform(0.005, 0.08)
        tn = t % nlen
        ph = np.cumsum(2 * np.pi * notes[(t / nlen).astype(int) % 8] / SR)
        wave = np.sin(ph) if rng.random() < 0.6 else np.tanh(3 * np.sin(ph))
        env = np.minimum(tn / att, 1) * np.exp(-tn * rng.uniform(0.3, 3))
        if rng.random() < 0.4 and kicks:
            env *= 1 - 0.9 * np.exp(-(t % beat) * 12)
        out += rng.uniform(0.1, 0.5) * wave * env
    out *= 0.9 / (np.abs(out).max() + 1e-9)
    return out.astype("<f4"), kicks, sorted(snares)


def save(dest, name, x, kicks, snares=None):
    x.astype("<f4").tofile(dest / f"{name}.f32")
    (dest / f"{name}.kicks").write_text("".join(f"{v:.4f}\n" for v in kicks))
    if snares is not None:
        (dest / f"{name}.caisses").write_text("".join(f"{v:.4f}\n" for v in snares))


def variants(root, dest):
    """4 remixages de chaque morceau de MDB Drums : batterie de -8 à 0 dB, autres pistes de -3 à +6 dB."""
    rng = np.random.default_rng(1)
    dec = lambda p: np.frombuffer(
        subprocess.run(["ffmpeg", "-v", "error", "-i", str(p), "-f", "f32le", "-ac", "1", "-ar", str(SR), "-"], check=True, capture_output=True).stdout,
        dtype="<f4",
    ).astype(float)
    for d in sorted((root / "audio" / "multi-tracks").iterdir()):
        meta = (d / f"{d.name}_METADATA.yaml").read_text()
        stems = [(dec(d / f"{d.name}_STEMS" / f), "drum set" in inst) for f, inst in re.findall(r"filename: (\S+_STEM_\d+\.wav)\n    instrument: ([^\n]+)", meta)]
        n = min(len(s) for s, _ in stems)
        ann = (root / "annotations" / "class" / f"{d.name}_class.txt").read_text().splitlines()
        kicks = sorted({float(l.split()[0]) for l in ann if l.split()[1:] == ["KD"]})
        snares = sorted({float(l.split()[0]) for l in ann if l.split()[1:] == ["SD"]})
        for v in range(4):
            mix = sum(s[:n] * 10 ** ((rng.uniform(-8, 0) if drum else rng.uniform(-3, 6)) / 20) for s, drum in stems)
            save(dest, f"{d.name}_v{v}", mix * 0.9 / (np.abs(mix).max() + 1e-9), kicks, snares)
        print(d.name, flush=True)


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    args = sys.argv[1:]
    light = {}
    if "--leger" in args:
        i = args.index("--leger")
        for a in args[i + 1 :]:
            d, _, w = a.partition("@")
            wk, _, ws = w.partition("/")
            wk = float(wk) if wk else LIGHT
            light[d] = {"kick": wk, "caisse": float(ws) if ws else wk}
        args = args[:i]
    if not args:
        sys.exit(__doc__)
    mode = args[0]
    if mode == "synth":
        dest = pathlib.Path(args[1])
        dest.mkdir(parents=True, exist_ok=True)
        rng = np.random.default_rng(12345)
        for k in range(int(args[2]) if len(args) > 2 else 120):
            save(dest, f"synth_{k:03}", *synth_clip(rng))
    elif mode == "variantes":
        dest = pathlib.Path(args[2])
        dest.mkdir(parents=True, exist_ok=True)
        variants(pathlib.Path(args[1]), dest)
    elif mode == "valider":
        validate(load(args[1:] + list(light), light))
    elif mode == "apprendre":
        tracks = load(args[2:] + list(light), light)
        write_rust({kind: train_all(tracks, kind) for kind in KINDS}, args[1])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
