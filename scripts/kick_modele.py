"""Apprentissage des détecteurs de kick et de caisse claire (`src/kick.rs`) : écrit `src/kick_modele.rs`.

Données : dossiers de `<nom>.f32` (mono, f32, 48 kHz) + `<nom>.kicks` et `<nom>.caisses` (instants
des vrais kicks et des vraies caisses claires ; un morceau sans `.caisses` ne sert qu'au kick) et,
facultatifs, `<nom>.kicks_doute` et `<nom>.caisses_doute` (instants où la référence est incertaine :
ni coup ni non-coup, ignorés à l'apprentissage comme à l'évaluation). Préparés par `kick_bench.py`
(MDB Drums, morceaux transcrits par ADTOF), `kick_remix.py` (batterie refaite, instants exacts) ou
par ce script :

    python kick_modele.py synth <sortie> [n=300]           boîte à rythmes synthétique, étiquettes exactes
    python kick_modele.py variantes <dossier MDB> <sortie>  4 remixages de chaque morceau de MDB (pistes séparées)
    python kick_modele.py valider <dossier>... [--leger <dossier>...] [--essai <dossier>...] [--reprise <fichier>]
    python kick_modele.py apprendre <kick_modele.rs> <dossier>... [--leger <dossier>...]
    python kick_modele.py detecter <kick_modele.rs> <morceau.f32>   coups trouvés par le modèle écrit (calcul en Python)

`dossier[@poids[/poids_caisse]]` : chaque morceau du dossier pèse `poids` (1 par défaut ; 0,25 après
`--leger`) d'un vrai morceau, `poids_caisse` (par défaut le même) pour la caisse claire. Les dossiers
`--leger` servent seulement à l'apprentissage, les dossiers `--essai` seulement à l'évaluation ;
`valider` fait une validation croisée en 5 groupes de morceaux et range `<nom>_v<k>` (variante,
remixage) dans le groupe de `<nom>`. Y sont évalués les dossiers de poids 1 et ceux de `--essai` ; un
dossier d'apprentissage d'un autre poids répété après `--essai` (`neg@.5 ... --essai neg`) est évalué
de même, chaque morceau par le modèle qui ne l'a pas appris. Un dossier sans aucun coup de référence
(voix, musique sans batterie) est noté en faux coups par minute. Avec `--reprise <fichier>`, chaque
groupe fini (5 minutes) y est noté : relancée avec les mêmes arguments, une validation interrompue
reprend au groupe suivant ; le fichier est effacé à la fin.

Modèle : un petit réseau récurrent (GRU) lit à chaque pas de 5 ms les niveaux des 10 bandes et garde
le contexte (rythme, coups récents, nature du son) ; chaque attaque candidate est jugée 10, 20 et
30 ms après par un réseau par détecteur, sur les mesures de l'attaque et l'état du GRU. Le Rust
reproduit exactement les mesures calculées ici (`levels`, `macro`, `prep`, `candidates`, `features`,
`frame_inputs`) : tout changement doit être fait des deux côtés.

`detecter` relit les poids de `kick_modele.rs` et refait tout le calcul en Python : sa sortie doit être
celle de `cargo run --release --example kick_mesures -- <morceau.f32>` (mêmes coups, mêmes instants).

Les niveaux par bande de chaque morceau sont gardés dans `<dossier>/.mesures/` (à effacer si `levels`
change). Dépendances : numpy, scipy, torch (GPU conseillé) ; ffmpeg pour `variantes`.
"""

import ast
import json
import pathlib
import re
import subprocess
import sys
import warnings
from multiprocessing import Pool

import numpy as np
from scipy import signal

SR = 48000
HOP_S = 0.005
# 10 bandes fines ; les 5 larges (sub, 90–200 Hz, 200 Hz–1 kHz, 1–4 kHz, aigus) en sont des paires.
BANDS = [(None, 50), (50, 90), (90, 140), (140, 200), (200, 450), (450, 1000), (1000, 2000), (2000, 4000), (4000, 8000), (8000, None)]
# Étage de calcul de chaque bande : 0 = cadence d'origine, 1 = ÷D1 (~12 kHz), 2 = ÷2·D1 (~6 kHz).
BAND_STAGE = [2, 2, 2, 2, 2, 2, 1, 1, 0, 0]
Q6 = [0.5176380902050415, 0.7071067811865476, 1.9318516525781366]  # Butterworth d'ordre 6
AA1, AA2 = 0.40, 0.37  # coupure des passe-bas avant sous-échantillonnage, en part de la cadence d'arrivée
HOP_N = 240  # pas à 48 kHz
HOP = HOP_N / SR
STAGES = [2, 4, 6]  # décisions 10, 20, 30 ms après l'attaque
PRE = 8
P0 = [-8.0, -11.0, -12.0, -16.0, -18.0]  # références de départ (= START_PEAK en Rust)
FPK0 = 35.0
SUBPK0 = 0.015
SILENCE_DB = -60.0
H = 32  # couche cachée des réseaux de décision
G = 48  # taille du GRU
EPOCHS, STEPS, BATCH, SEG = 400, 24, 48, 3000  # apprentissage : 400 × 24 lots de 48 extraits de 15 s (au-delà, plus de gain)
LIGHT = 0.25
THRESHOLDS = [0.8, 0.7, 0.5]  # à sensibilité 5 (= STAGE_LOGIT en Rust) ; les mêmes pour la caisse claire
GAP = 0.1  # kick ; caisse claire : SNARE_GAP
SNARE_GAP = 0.05
QUIET_S, QUIET_RAISE = 4.0, 1.5  # sans kick depuis 4 s, seuils relevés (logit) ; kick seulement
TOL = 0.025  # un candidat à ±25 ms d'un coup de référence est positif
KINDS = {"kick": ".kicks", "caisse": ".caisses"}


# --- mesures (identiques au Rust) ------------------------------------------------------------


def rbj(sr, f0, high, q=Q6[1]):
    w = 2 * np.pi * min(f0, 0.45 * sr) / sr
    c, a = np.cos(w), np.sin(w) / (2 * q)
    a0 = 1 + a
    b = [(1 + c) / 2, -(1 + c), (1 + c) / 2] if high else [(1 - c) / 2, 1 - c, (1 - c) / 2]
    return np.array(b) / a0, np.array([1, -2 * c / a0, (1 - a) / a0])


def levels(x, sr=SR):
    """Niveau (dB) de chaque bande fine par pas de 5 ms, fenêtre 10 ms : (10, n).

    Les bandes sous 4 kHz sont calculées après sous-échantillonnage (÷D1 → ~12 kHz pour 1–4 kHz,
    encore ÷2 → ~6 kHz en dessous de 1 kHz), chacun précédé d'un passe-bas de Butterworth d'ordre 6."""
    d1 = max(1, round(sr / 12000))
    hop = 2 * d1 * max(1, round(sr * HOP_S / (2 * d1)))
    x = np.asarray(x, dtype=float)
    y = x
    for q in Q6:
        y = signal.lfilter(*rbj(sr, AA1 * sr / d1, False, q), y)
    x1 = y[d1 - 1 :: d1]
    y = x1
    for q in Q6:
        y = signal.lfilter(*rbj(sr / d1, AA2 * sr / d1 / 2, False, q), y)
    xs, srs, ds = [x, x1, y[1::2]], [sr, sr / d1, sr / d1 / 2], [1, d1, 2 * d1]
    n = len(x) // hop
    out = []
    for (lo, hi), r in zip(BANDS, BAND_STAGE):
        y, fs, h = xs[r], srs[r], hop // ds[r]
        for _ in range(2):
            if lo:
                y = signal.lfilter(*rbj(fs, lo, True), y)
            if hi:
                y = signal.lfilter(*rbj(fs, hi, False), y)
        blk = (y[: n * h] ** 2).reshape(n, h).mean(1)
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
    O, Gm = np.empty(n), np.empty(n)
    a1, a2 = min(HOP / 0.35, 1), np.exp(-HOP / 3.0)
    for i in range(n):
        exc = max(e[i] - slow, 0)
        slow += (e[i] - slow) * a1
        pk = max(exc, pk if silent[i] else pk * a2)
        O[i] = exc / pk
        Gm[i] = 10 * np.log10(e[i] / (mid[max(0, i - 24) : i + 1].max() + 1e-12) + 1e-9)
    return F, Fpk, P, tot, O, Gm


def candidates(F, Fpk, tot):
    p = np.arange(PRE + 1, len(F) - max(STAGES) - 1)
    m = (F[p] > F[p - 1]) & (F[p] >= F[p + 1]) & (F[p] >= 0.15 * Fpk[p - 1]) & (tot[p] > -70)
    return p[m]


def features(Lf, L, F, Fpk, P, tot, O, Gm, p, s):
    pre = np.stack([L[:, q - PRE : q - 1].min(1) for q in p], 1) if len(p) else np.zeros((5, 0))
    pref = np.stack([Lf[:, q - PRE : q - 1].min(1) for q in p], 1) if len(p) else np.zeros((len(BANDS), 0))
    k = p + s
    f = [L[:, p + j] - pre for j in (1, 2, 4, 6) if j <= s]
    f += [L[:, k] - P[:, p - 1], L[:, k] - tot[k]]
    f += [Lf[:, p + j] - pref for j in (1, 2, 4, 6) if j <= s]  # montée de chaque bande fine
    f += [Lf[:, k] - tot[k]]  # spectre fin
    f = np.vstack(f).T
    extra = np.stack(
        [np.log(F[p] / Fpk[p - 1] + 1e-3), tot[k] - P[:, p - 1].max(0), pre[0] - P[0, p - 1], pre[1] - P[1, p - 1], O[k], Gm[k]], 1
    )
    return np.hstack([f, extra]).astype(np.float32)


def frame_inputs(Lf, P, O, Gm):
    """Entrées du GRU à chaque pas : niveau de chaque bande fine sous le plus fort pic du morceau,
    montée de chaque bande sur 10 ms, excès du sub, rapport sub / médiums : (n, 22)."""
    a = np.clip((Lf - P.max(0)) / 20.0, -4.0, 0.5)
    d = np.zeros_like(Lf)
    d[:, 2:] = np.minimum(np.maximum(Lf[:, 2:] - Lf[:, :-2], 0) / 10.0, 4.0)
    return np.vstack([a, d, np.minimum(O, 1.0)[None], np.clip(Gm / 20.0, -3, 3)[None]]).T.astype(np.float32)


def times(path):
    if not path.exists():
        return None
    with warnings.catch_warnings():  # fichier vide : aucun coup
        warnings.simplefilter("ignore")
        return np.loadtxt(path, ndmin=1)


def track(base):
    """Mesures d'un morceau : entrées du GRU, candidats, mesures par étape et, par détecteur dont le
    morceau a la référence : étiquettes des candidats, masque (faux = référence douteuse), instants."""
    base = pathlib.Path(base)
    cache = base.parent / ".mesures" / (base.name + ".npy")
    if cache.exists():
        Lf = np.load(cache).astype(float)
    else:
        Lf = levels(np.fromfile(str(base) + ".f32", dtype="<f4"))
        cache.parent.mkdir(exist_ok=True)
        np.save(cache, Lf.astype(np.float32))
    L = macro(Lf)
    F, Fpk, P, tot, O, Gm = prep(L)
    p = candidates(F, Fpk, tot)
    t = p * HOP
    near = lambda r: np.array([np.any(np.abs(r - x) <= TOL) for x in t], dtype=bool) if len(r) else np.zeros(len(t), bool)
    ref, y, mask, doubt = {}, {}, {}, {}
    for kind, suffix in KINDS.items():
        r = times(pathlib.Path(str(base) + suffix))
        if r is not None:
            u = times(pathlib.Path(str(base) + suffix + "_doute"))
            ref[kind], y[kind], doubt[kind] = r, near(r), u if u is not None else np.array([])
            mask[kind] = ~near(doubt[kind]) | y[kind]
    return dict(name=base.name, n=Lf.shape[1], fin=frame_inputs(Lf, P, O, Gm), p=p, X=[features(Lf, L, F, Fpk, P, tot, O, Gm, p, s) for s in STAGES], y=y, mask=mask, ref=ref, doubt=doubt)


def load(dirs):
    """{dossier: [morceaux]}"""
    jobs = [(d, k.with_suffix("")) for d in dirs for k in sorted(pathlib.Path(d).glob("*.kicks"))]
    out = {d: [] for d in dirs}
    with Pool(6) as pool:  # au fil de l'eau : les mesures de 1 500 morceaux pèsent plusieurs Go
        for (d, _), t in zip(jobs, pool.imap(track, [b for _, b in jobs], chunksize=4)):
            out[d].append(t)
    return out


# --- apprentissage -----------------------------------------------------------------------------


class Corpus:
    """Tous les morceaux bout à bout sur le GPU. Les mesures y sont gardées en demi-précision
    (f16, converties à l'usage) et retirées de `data` au fur et à mesure : elles pèsent plusieurs Go."""

    def __init__(self, data):
        import torch

        self.dev = dev = "cuda" if torch.cuda.is_available() else "cpu"
        ts = [(d, t) for d, group in data.items() for t in group]
        half = lambda arrays: torch.tensor(np.concatenate(arrays).astype(np.float16), device=dev)
        self.fin = half([t.pop("fin") for _, t in ts])
        self.cx = []
        for si in range(len(STAGES)):
            self.cx.append(half([t["X"][si] for _, t in ts]))
            for _, t in ts:
                t["X"][si] = None
        self.tracks, cps, cts, cys, cms, off, nc = [], [], [], [], [], 0, 0
        for d, t in ts:
            del t["X"]
            self.tracks.append(dict(t, dir=d, off=off, c0=nc))
            cps.append(t["p"] + off)
            cts.append(np.full(len(t["p"]), len(self.tracks) - 1))
            zero = np.zeros(len(t["p"]), bool)
            cys.append(np.stack([t["y"].get(k, zero) for k in KINDS], 1))
            cms.append(np.stack([t["mask"].get(k, zero) for k in KINDS], 1))
            off += t["n"]
            nc += len(t["p"])
        self.cp_np, self.ct = np.concatenate(cps), np.concatenate(cts)
        self.cp = torch.tensor(self.cp_np, device=dev)
        self.cy = torch.tensor(np.concatenate(cys), dtype=torch.float32, device=dev)
        self.cm = torch.tensor(np.concatenate(cms), dtype=torch.float32, device=dev)


def build(nin, nx, norm):
    import torch

    class Model(torch.nn.Module):
        def __init__(self):
            super().__init__()
            self.gru = torch.nn.GRU(nin, G, batch_first=True)
            head = lambda n: torch.nn.Sequential(torch.nn.Linear(G + n, H), torch.nn.Tanh(), torch.nn.Linear(H, 1))
            self.heads = torch.nn.ModuleList([torch.nn.ModuleList([head(n) for _ in KINDS]) for n in nx])
            for i, vals in enumerate(norm):
                for name, v in zip(("lo", "hi", "mu", "sd"), vals):
                    self.register_buffer(f"{name}{i}", torch.tensor(v, dtype=torch.float32))

        def states(self, fin):
            out, h = [], None
            for a in range(0, fin.shape[1], 4000):  # par tranches : cuDNN refuse les très longues séquences
                o, h = self.gru(fin[:, a : a + 4000].contiguous(), h)
                out.append(o)
            return torch.cat(out, 1)

        def out(self, si, h, x):
            """h : état du GRU au pas de la décision (m, G) ; x : mesures brutes (m, n) → logits (m, 2)."""
            g = lambda name: getattr(self, f"{name}{si}")
            x = (torch.minimum(torch.maximum(x, g("lo")), g("hi")) - g("mu")) / g("sd")
            z = torch.cat([h, x], 1)
            return torch.cat([hd(z) for hd in self.heads[si]], 1)

    return Model()


def train(C, weights, seed=0):
    """weights : {indice de morceau: (poids kick, poids caisse)} des morceaux d'apprentissage."""
    import torch

    dev = C.dev
    torch.manual_seed(seed)
    rng = np.random.default_rng(seed)
    ids = np.array(sorted(weights))
    wk, ws = np.array([weights[i][0] for i in ids]), np.array([weights[i][1] for i in ids])
    wtab = torch.ones(len(C.tracks), 2, device=dev)
    wtab[torch.tensor(ids, device=dev), 1] = torch.tensor(ws / wk, dtype=torch.float32, device=dev)
    ctab = torch.tensor(C.ct, device=dev)
    sub = np.flatnonzero(np.isin(C.ct, ids))
    sub = torch.tensor(sub[np.random.default_rng(0).permutation(len(sub))[:400000]], device=dev)
    norm = []
    for x in C.cx:  # plage et normalisation des mesures, d'après les morceaux d'apprentissage
        X = x[sub].float().cpu().numpy()
        lo, hi = np.percentile(X, 0.1, axis=0), np.percentile(X, 99.9, axis=0)
        X = np.clip(X, lo, hi)
        norm.append((lo, hi, X.mean(0), X.std(0) + 1e-6))
    model = build(C.fin.shape[1], [x.shape[1] for x in C.cx], norm).to(dev)
    opt = torch.optim.AdamW(model.parameters(), lr=3e-3, weight_decay=1e-4)
    sched = torch.optim.lr_scheduler.OneCycleLR(opt, 4e-3, total_steps=EPOCHS * STEPS)
    off, ns = np.array([C.tracks[i]["off"] for i in ids]), np.array([C.tracks[i]["n"] for i in ids])
    prob = wk / wk.sum()  # chaque morceau pèse son poids, quelle que soit sa longueur
    ar = torch.arange(SEG, device=dev)
    for _ in range(EPOCHS * STEPS):
        j = rng.choice(len(ids), BATCH, p=prob)
        a = (rng.random(BATCH) * np.maximum(1, ns[j] - SEG)).astype(np.int64)
        g0, g1 = off[j] + a, off[j] + np.minimum(a + SEG, ns[j])
        idx = torch.minimum(torch.tensor(g0, device=dev)[:, None] + ar[None, :], torch.tensor(g1 - 1, device=dev)[:, None])
        h = model.states(C.fin[idx].float())
        lo, hi = np.searchsorted(C.cp_np, g0 + PRE + 1), np.searchsorted(C.cp_np, g1 - STAGES[-1] - 1)
        ci = torch.cat([torch.arange(l, max(l, u), device=dev) for l, u in zip(lo, hi)])
        bi = torch.cat([torch.full((max(0, u - l),), b, device=dev) for b, (l, u) in enumerate(zip(lo, hi))])
        q = C.cp[ci] - torch.tensor(g0, device=dev)[bi]
        w, y, loss = C.cm[ci] * wtab[ctab[ci]], C.cy[ci], 0
        for si, s in enumerate(STAGES):
            l = torch.nn.functional.binary_cross_entropy_with_logits(model.out(si, h[bi, q + s], C.cx[si][ci].float()), y, reduction="none")
            loss = loss + (l * w).sum() / w.sum().clamp(min=1)
        opt.zero_grad()
        loss.backward()
        torch.nn.utils.clip_grad_norm_(model.parameters(), 2.0)
        opt.step()
        sched.step()
    return model.eval()


def predict(model, C, i):
    """Logits des candidats du morceau `i` : (candidats, étapes, détecteurs)."""
    import torch

    t = C.tracks[i]
    with torch.no_grad():
        h = model.states(C.fin[t["off"] : t["off"] + t["n"]][None].float())[0]
        p = torch.tensor(t["p"], device=C.dev)
        ci = slice(t["c0"], t["c0"] + len(t["p"]))
        return torch.stack([model.out(si, h[p + s], C.cx[si][ci].float()) for si, s in enumerate(STAGES)], 1).cpu().numpy()


def decisions(p, scores, kind):
    """Instants des coups annoncés ; scores : (candidats, étapes). Comme `decide` en Rust, chaque
    étape d'un candidat est jugée à son pas (attaque + étape), dans l'ordre du temps et, au même
    pas, le candidat le plus ancien d'abord : la décision à 10 ms d'une attaque peut précéder celle
    à 30 ms de l'attaque d'avant."""
    thr = np.array([np.log(t / (1 - t)) for t in THRESHOLDS])
    gap = GAP if kind == "kick" else SNARE_GAP
    events = sorted((p[i] + s, p[i], k, i) for i in np.flatnonzero((scores > thr).any(1)) for k, s in enumerate(STAGES))
    det, last, done = [], -9.0, set()
    for when, attack, k, i in events:
        since = attack * HOP - last
        quiet = QUIET_RAISE if kind == "kick" and since > QUIET_S else 0.0
        if i not in done and since > gap and scores[i, k] > thr[k] + quiet:
            det.append((when + 1) * HOP)
            last = attack * HOP
            done.add(i)
    return det


def simulate(p, scores, refs, kind, doubt=()):
    """Coups annoncés comparés à la référence → (vrais, détectés, référence). Une détection sur un
    instant douteux n'est comptée ni juste ni fausse."""
    det = decisions(p, scores, kind)
    refs, doubt = np.asarray(refs), np.asarray(doubt)
    hit = lambda h, r: len(r) > 0 and bool(np.any((h >= r - 0.02) & (h <= r + 0.07)))
    det = [h for h in det if hit(h, refs) or not hit(h, doubt)]
    used, tp = np.zeros(len(refs), bool), 0
    for h in det:
        c = np.where(~used & (h >= refs - 0.02) & (h <= refs + 0.07))[0]
        if len(c):
            used[c[np.argmin(abs(refs[c] - h))]] = True
            tp += 1
    return np.array([tp, len(det), len(refs)])


def base_name(n):
    return re.sub(r"_v\d$", "", n)


def validate(data, weights, tests, resume=None):
    """weights : {dossier: (poids kick, poids caisse)} des dossiers d'apprentissage ; un dossier est
    évalué s'il pèse 1 (vrai morceau) ou s'il est dans `tests`. `resume` : fichier où sont notés les
    groupes finis, pour qu'une validation interrompue reprenne au groupe suivant (les groupes sont
    indépendants : chaque apprentissage repart de la même graine)."""
    C = Corpus(data)
    evald = [d for d in data if d in tests or weights.get(d, (0,))[0] == 1]
    names = sorted({base_name(t["name"]) for t in C.tracks if t["dir"] in evald})
    fold = dict(zip(names, np.random.default_rng(0).permutation(len(names)) % 5))
    group = lambda t: fold.get(base_name(t["name"]), -1)
    sig = repr((sorted(weights.items()), sorted(tests), len(C.tracks)))
    state = {"sig": sig, "done": 0, "tot": {}}
    if resume and resume.exists():
        old = json.loads(resume.read_text(encoding="utf-8"))
        if old["sig"] == sig:
            state = old
            print(f"reprise après le groupe {state['done']}/5", flush=True)
    tot = {tuple(k.split("|")): np.array(v) for k, v in state["tot"].items()}
    for f in range(state["done"], 5):
        model = train(C, {i: weights[t["dir"]] for i, t in enumerate(C.tracks) if t["dir"] in weights and group(t) != f})
        for i, t in enumerate(C.tracks):
            if t["dir"] in evald and group(t) == f:
                o = predict(model, C, i)
                for ki, kind in enumerate(KINDS):
                    if kind in t["ref"]:
                        key = (kind, pathlib.Path(t["dir"]).name)
                        tot.setdefault(key, np.zeros(4))
                        tot[key] += [*simulate(t["p"], o[:, :, ki], t["ref"][kind], kind, t["doubt"][kind]), t["n"] * HOP]
        print(f"groupe {f + 1}/5", flush=True)
        if resume:
            state = {"sig": sig, "done": f + 1, "tot": {"|".join(k): v.tolist() for k, v in tot.items()}}
            resume.write_text(json.dumps(state), encoding="utf-8")
    for (kind, d), r in sorted(tot.items()):
        if r[2] == 0:
            print(f"{kind:7} {d:12} {r[1] / r[3] * 60:.1f} faux coups par minute")
        else:
            pr, rc = r[0] / max(r[1], 1), r[0] / max(r[2], 1)
            print(f"{kind:7} {d:12} précision {pr:.3f} rappel {rc:.3f} F1 {2 * pr * rc / max(pr + rc, 1e-9):.3f}")
    if resume:
        resume.unlink(missing_ok=True)


def write_rust(model, nin, nx, path):
    fmt = lambda v: np.format_float_scientific(np.float32(v), unique=True, trim="0")  # plus court f32 exact
    row = lambda v: "[" + ", ".join(fmt(x) for x in v) + "]"
    sd = {k: v.detach().cpu().numpy().astype(np.float64) for k, v in model.state_dict().items()}
    out = [
        "// Généré par scripts/kick_modele.py (apprendre) : ne pas modifier à la main.",
        f"// Un GRU ({nin} entrées, état de {G}) avancé à chaque pas de 5 ms garde le contexte ; par détecteur",
        f"// (kick, caisse claire), trois petits réseaux (mesures de l'attaque + état → {H} tanh → 1), un par",
        "// instant de décision après l'attaque ; la normalisation des mesures est intégrée à la première couche.",
        "",
        "use crate::kick::{Gru, Net};",
        "",
        f"pub(crate) const H: usize = {H};",
        f"pub(crate) const G: usize = {G};",
        f"pub(crate) const NI: usize = {nin};",
    ]
    out += [f"pub(crate) const N{s}: usize = {n};" for s, n in zip(STAGES, nx)]
    out += ["", "/// Contexte : portes dans l'ordre de PyTorch (r, z, n).", "pub(crate) static GRU: Gru = Gru {"]
    for name, key in (("wi", "gru.weight_ih_l0"), ("wh", "gru.weight_hh_l0")):
        out += [f"    {name}: ["] + [f"        {row(r)}," for r in sd[key]] + ["    ],"]
    out += [f"    bi: {row(sd['gru.bias_ih_l0'])},", f"    bh: {row(sd['gru.bias_hh_l0'])},", "};", ""]
    for ki, kind in enumerate(KINDS):
        for si, s in enumerate(STAGES):
            W, b1 = sd[f"heads.{si}.{ki}.0.weight"], sd[f"heads.{si}.{ki}.0.bias"]
            Wx = W[:, G:] / sd[f"sd{si}"][None, :]  # normalisation intégrée à la première couche
            out.append(f"/// {'Kick' if kind == 'kick' else 'Caisse claire'} : décision {s * 5} ms après l'attaque.")
            out.append(f"pub(crate) static {'KICK' if kind == 'kick' else 'CAISSE'}{s}: Net<N{s}> = Net {{")
            out += ["    w: ["] + [f"        {row(r)}," for r in Wx] + ["    ],"]
            out += ["    u: ["] + [f"        {row(r)}," for r in W[:, :G]] + ["    ],"]
            out.append(f"    b: {row(b1 - Wx @ sd[f'mu{si}'])},")
            out.append(f"    v: {row(sd[f'heads.{si}.{ki}.2.weight'][0])},")
            out.append(f"    c: {fmt(sd[f'heads.{si}.{ki}.2.bias'][0])},")
            out.append(f"    lo: {row(sd[f'lo{si}'])},")
            out.append(f"    hi: {row(sd[f'hi{si}'])},")
            out += ["};", ""]
    pathlib.Path(path).write_text("\n".join(out), encoding="utf-8", newline="\n")
    print("écrit", path)


def read_rust(path):
    """Poids de `kick_modele.rs` : {nom: {champ: tableau}}."""
    text = pathlib.Path(path).read_text(encoding="utf-8")
    out = {}
    for name, body in re.findall(r"static (\w+): [^=]+= \w+ (\{.*?\n\});", text, re.S):
        out[name] = {k: np.array(v) for k, v in ast.literal_eval(re.sub(r"(\w+): ", r'"\1": ', body)).items()}
    return out


def detect(model_path, f32_path):
    """Coups trouvés dans un morceau par le modèle écrit en Rust, calculés ici : [(instant, détecteur)]."""
    m = read_rust(model_path)
    Lf = levels(np.fromfile(f32_path, dtype="<f4"))
    L = macro(Lf)
    F, Fpk, P, tot, O, Gm = prep(L)
    p = candidates(F, Fpk, tot)
    fin = frame_inputs(Lf, P, O, Gm)
    g, sig = m["GRU"], lambda v: 1 / (1 + np.exp(-v))
    h, states, silent = np.zeros(G), np.empty((len(fin), G)), 0
    for i, x in enumerate(fin):
        silent = silent + 1 if tot[i] < SILENCE_DB else 0
        if silent * HOP >= 2.0:  # RESET_S en Rust : contexte oublié après 2 s de silence
            h = np.zeros(G)
        else:
            gi, gh = g["wi"] @ x + g["bi"], g["wh"] @ h + g["bh"]
            r, z = sig(gi[:G] + gh[:G]), sig(gi[G : 2 * G] + gh[G : 2 * G])
            h = (1 - z) * np.tanh(gi[2 * G :] + r * gh[2 * G :]) + z * h
        states[i] = h
    out = []
    for kind, name in (("kick", "KICK"), ("caisse", "CAISSE")):
        sc = np.empty((len(p), len(STAGES)))
        for si, s in enumerate(STAGES):
            n = m[f"{name}{s}"]
            X = np.clip(features(Lf, L, F, Fpk, P, tot, O, Gm, p, s), n["lo"], n["hi"])
            sc[:, si] = np.tanh(X @ n["w"].T + states[p + s] @ n["u"].T + n["b"]) @ n["v"] + n["c"]
        out += [(t, kind) for t in decisions(p, sc, kind)]
    return sorted(out)


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
    # kick : sinus qui descend, avec ou sans clic, saturé ou non ; un sur quatre est long (808),
    # parfois avec une ondulation d'amplitude
    long_kick = rng.random() < 0.25
    f0, f1 = rng.uniform(70, 110 if long_kick else 220), rng.uniform(38, 65)
    sweep, decay = rng.uniform(12, 45), rng.uniform(1.5, 4) if long_kick else rng.uniform(3, 16)
    click = rng.choice([0.0, rng.uniform(0.05, 0.6)])
    drive = rng.choice([1.0, rng.uniform(2, 8)], p=[0.7, 0.3])
    klen = int(SR * min(1.2 if long_kick else 0.6, 5 / decay))
    tk = np.arange(klen) / SR
    kick = np.sin(2 * np.pi * (f1 * tk + (f0 - f1) * (1 - np.exp(-tk * sweep)) / sweep)) * np.exp(-tk * decay)
    if long_kick and rng.random() < 0.6:
        depth = rng.uniform(0.15, 0.45)
        kick *= 1 - depth + depth * np.cos(2 * np.pi * rng.uniform(4, 16) * tk)
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
    # caisse claire sur 2 et 4, notes fantômes, et parfois un roulement (montée d'EDM) ; un extrait
    # sur trois n'en a aucune (kicks et charleston seuls : un charleston frappé avec le kick n'en
    # est pas une)
    backbeat = rng.random() < 0.67
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

    roll_at, roll_every = (int(rng.uniform(0, steps * 0.6)), rng.choice([1, 2])) if backbeat and rng.random() < 0.5 else (-1, 1)
    roll_len = int(rng.uniform(16, 64))
    for s in range(steps):
        if roll_at <= s < roll_at + roll_len:
            if s % roll_every == 0:
                for sub in range(2 if roll_every == 1 and rng.random() < 0.3 else 1):
                    snare((s + sub * 0.5) * step, sgain * rng.uniform(0.5, 1.0))
        elif backbeat and s % 16 in (4, 12):
            snare(s * step, sgain)
        elif backbeat and rng.random() < 0.04:
            snare(s * step, sgain * 0.3)
    # charleston
    hgain, hlen = rng.uniform(0.03, 0.3), int(SR * 0.05)
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
    if not args:
        sys.exit(__doc__)
    mode, args = args[0], args[1:]
    if mode == "synth":
        dest = pathlib.Path(args[0])
        dest.mkdir(parents=True, exist_ok=True)
        rng = np.random.default_rng(12345)
        for k in range(int(args[1]) if len(args) > 1 else 300):
            save(dest, f"synth_{k:03}", *synth_clip(rng))
    elif mode == "variantes":
        dest = pathlib.Path(args[1])
        dest.mkdir(parents=True, exist_ok=True)
        variants(pathlib.Path(args[0]), dest)
    elif mode == "detecter":
        for t, kind in detect(args[0], args[1]):
            print(f"{kind} {t:.3f}")
    elif mode in ("valider", "apprendre"):
        out = args.pop(0) if mode == "apprendre" else None
        weights, tests, default, resume = {}, [], 1.0, None
        if "--reprise" in args:
            k = args.index("--reprise")
            resume = pathlib.Path(args[k + 1])
            del args[k : k + 2]
        for a in args:
            if a in ("--leger", "--essai"):
                default = LIGHT if a == "--leger" else None
                continue
            d, _, w = a.partition("@")
            if default is None:
                tests.append(d)
                continue
            wk, _, ws = w.partition("/")
            wk = float(wk) if wk else default
            weights[d] = (wk, float(ws) if ws else wk)
        data = load(list(dict.fromkeys(list(weights) + (tests if mode == "valider" else []))))
        if mode == "valider":
            validate(data, weights, tests, resume)
        else:
            C = Corpus(data)
            model = train(C, {i: weights[t["dir"]] for i, t in enumerate(C.tracks)})
            write_rust(model, C.fin.shape[1], [x.shape[1] for x in C.cx], out)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
