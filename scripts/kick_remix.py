"""Remixages à étiquettes exactes pour l'apprentissage du kick et de la caisse claire (`kick_modele.py`).

L'accompagnement réel d'un morceau (basse + voix + reste, séparés par Demucs : `kick_bench.py ref
--demucs --pistes`) reçoit une batterie refaite avec des sons isolés (boîtes à rythmes, banque Dirt,
kicks de synthèse), posés aux instants de la batterie d'origine (sa transcription) : les instants
sont connus à l'échantillon près, et la basse et les voix restent celles d'un vrai morceau.

    python kick_remix.py <données> <pistes> <banques> <sortie> [graine=1] [--essai]

- `<données>` : `<nom>.kicks` et `<nom>.caisses` des morceaux d'origine ; `<pistes>` : `<nom>.npy`
  (int16, 4 pistes drums/bass/other/vocals à 44,1 kHz) ;
- `<banques>` : dossier contenant `tdm` (<https://github.com/geikha/tidal-drum-machines>) et `dirt`
  (<https://github.com/tidalcycles/Dirt-Samples>) ;
- `<sortie>` : par morceau, deux extraits de ~100 s rebatterisés (`<nom>_v0`, `_v1`, kits différents,
  quelques kicks retirés : leurs instants vont dans `.kicks_doute`, un reste de l'ancien coup pouvant
  subsister) ; et dans `<sortie>sb`, un extrait sans batterie (`<nom>_v9` : aucun coup ; les instants
  des coups d'origine sont douteux pour la même raison) ;
- `--essai` : un seul extrait par morceau, avec les boîtes à rythmes tenues à l'écart
  (`TEST_MACHINES`), sans kick retiré : jeu d'évaluation à étiquettes exactes.

Dépendances : numpy, scipy, soundfile.
"""
import pathlib, sys, warnings, zlib
from multiprocessing import Pool

import numpy as np
import soundfile as sf
from scipy import signal

SR = 44100
KICK_DIRT = ["808bd", "clubkick", "hardkick", "popkick", "kicklinn", "reverbkick", "bd", "gabba", "house", "drumtraks", "electro1"]
TEST_MACHINES = {"RolandTR909", "LinnDrum", "AkaiMPC60", "KorgM1", "BossDR660", "EmuSP12", "YamahaRY30", "OberheimDMX", "RolandR8", "SimmonsSDS5"}


def load_one(path):
    try:
        x, sr = sf.read(path, dtype="float32", always_2d=True)
    except Exception:
        return None
    x = x.mean(1)
    if sr != SR:
        x = signal.resample_poly(x, SR, sr).astype(np.float32)
    pk = np.abs(x).max() if len(x) else 0
    if pk < 1e-3 or len(x) < 200:
        return None
    start = int(np.argmax(np.abs(x) > 0.03 * pk))  # début : premier échantillon au-dessus de -30 dB du pic
    x = x[max(0, start - 8) : start + int(1.6 * SR)].copy()
    x[-min(len(x), 400) :] *= np.linspace(1, 0, min(len(x), 400))
    return x / np.abs(x).max()


def kits(smp, test):
    """{machine: {type: [sons]}} ; types : bd, sd, cp, hh, oh, tom, cr, perc."""
    out = {}
    for m in sorted((smp / "tdm" / "machines").iterdir()):
        if (m.name in TEST_MACHINES) != test:
            continue
        k = {}
        for d in m.iterdir():
            typ = d.name.split("-")[-1]
            typ = {"lt": "tom", "mt": "tom", "ht": "tom", "rd": "cr", "rim": "perc", "cb": "perc", "sh": "perc", "tb": "perc", "sd2": "sd"}.get(typ, typ)
            if typ in ("bd", "sd", "cp", "hh", "oh", "tom", "cr", "perc"):
                k.setdefault(typ, []).extend(sorted(d.glob("*.[wW][aA][vV]")))
        if "bd" in k and "sd" in k:
            out[m.name] = k
    return out


def dirt(smp, names):
    return [f for n in names for f in sorted((smp / "dirt" / n).glob("*.[wW][aA][vV]"))]


def synth_kick(rng):
    f0, f1 = rng.uniform(80, 400), rng.uniform(36, 70)
    sweep, decay = rng.uniform(15, 60), rng.uniform(4, 18)
    n = int(SR * min(0.7, 5 / decay))
    t = np.arange(n) / SR
    k = np.sin(2 * np.pi * (f1 * t + (f0 - f1) * (1 - np.exp(-t * sweep)) / sweep)) * np.exp(-t * decay)
    if rng.random() < 0.6:
        c = rng.normal(0, 1, int(SR * 0.006)) * np.exp(-np.arange(int(SR * 0.006)) / (SR * rng.uniform(0.0005, 0.002)))
        k[: len(c)] += rng.uniform(0.1, 0.8) * c
    drive = rng.choice([1.0, rng.uniform(2, 30)])  # 30 : kick saturé (hardstyle)
    k = np.tanh(k * drive)
    return (k / np.abs(k).max()).astype(np.float32)


def shape(x, rng):
    """Variation d'un son : hauteur (±3 demi-tons), saturation, durée."""
    r = 2 ** (rng.uniform(-3, 3) / 12)
    if abs(r - 1) > 0.02:
        x = signal.resample(x, max(64, int(len(x) / r))).astype(np.float32)
    if rng.random() < 0.3:
        d = rng.uniform(1.5, 6)
        x = np.tanh(x * d) / np.tanh(d)
    if rng.random() < 0.3:
        n = max(400, int(len(x) * rng.uniform(0.3, 1)))
        x = x[:n].copy()
        x[-min(len(x), 600) :] *= np.linspace(1, 0, min(len(x), 600))
    return x / (np.abs(x).max() + 1e-9)


def put(out, at, x, g):
    i = int(round(at * SR))
    j = min(len(out), i + len(x))
    if 0 <= i < len(out):
        out[i:j] += g * x[: j - i]


def finish(mix, rng):
    mix = signal.resample_poly(mix, 160, 147)
    return (mix * 10 ** (rng.uniform(-12, 0) / 20) * 0.95 / (np.abs(mix).max() + 1e-9)).astype("<f4")


def times(path):
    if not path.exists():
        return np.array([])
    with warnings.catch_warnings():  # fichier vide : aucun coup
        warnings.simplefilter("ignore")
        return np.loadtxt(path, ndmin=1)


def render(args):
    data, stems_dir, smp, name, dst, seed, test = args
    rng = np.random.default_rng(zlib.crc32(name.encode()) + seed)
    stems = np.load(stems_dir / (name + ".npy")).astype(np.float32) / 32768
    back_full = stems[1] + stems[2] + stems[3]
    K, S = times(data / (name + ".kicks")), times(data / (name + ".caisses"))
    dur = len(back_full) / SR
    allk = kits(smp, test)
    names = sorted(allk)
    pick = lambda xs: xs[rng.integers(len(xs))]
    ld = lambda fs: [y for y in (load_one(f) for f in fs) if y is not None]
    for v in range(1 if test else 2):
        L = min(dur, 100.0)
        a = rng.uniform(0, max(0.0, dur - L))
        back = back_full[int(a * SR) : int((a + L) * SR)].copy()
        kit = allk[pick(names)]
        u = rng.random()
        if test or u < 0.45:  # kick : boîte à rythmes, banque Dirt, ou synthèse
            kick = ld([pick(kit["bd"])])
        elif u < 0.7:
            kick = ld([pick(dirt(smp, KICK_DIRT))])
        else:
            kick = [synth_kick(rng)]
        snare = ld([pick(kit["sd"])])
        clap = ld([pick(kit["cp"])]) if "cp" in kit and rng.random() < 0.4 else []
        if not kick or not snare:
            continue
        kick, snare = shape(kick[0], rng), shape(snare[0], rng)
        clap = shape(clap[0], rng) if clap else None
        others = {t: ld(list(rng.choice(kit[t], min(3, len(kit[t])), replace=False))) for t in ("hh", "oh", "tom", "cr", "perc") if t in kit}
        rms = np.sqrt(np.mean(back**2)) + 1e-4
        gk = rms * 10 ** (rng.uniform(4, 16) / 20)
        gs = gk * 10 ** (rng.uniform(-8, 2) / 20)
        gh = gk * 10 ** (rng.uniform(-22, -8) / 20)
        drums = np.zeros_like(back)
        k = K[(K >= a) & (K < a + L - 0.05)] - a
        s = S[(S >= a) & (S < a + L - 0.05)] - a
        # passages sans kick (la basse y joue seule) et kicks retirés au hasard
        # (jamais dans le jeu d'évaluation : un reste de l'ancien kick y compterait comme faux kick)
        keep = rng.random(len(k)) > (0.0 if test else 0.08)
        for _ in range(0 if test else rng.integers(0, 3)):
            b0 = rng.uniform(0, L)
            keep &= ~((k >= b0) & (k < b0 + rng.uniform(3, 10)))
        keeps = rng.random(len(s)) > (0.0 if test else 0.05)
        doubt_k, doubt_s = k[~keep], s[~keeps]  # un reste de l'ancien coup peut subsister : ni coup ni non-coup
        k, s = k[keep], s[keeps]
        duck = np.ones_like(back)
        depth, rel = (10 ** (-rng.uniform(3, 12) / 20), rng.uniform(0.08, 0.3)) if rng.random() < 0.4 else (1.0, 0.1)
        for t in k:
            put(drums, t, kick, gk * 10 ** (rng.uniform(-2, 1) / 20))
            if depth < 1:
                i, n = int(t * SR), int(rel * SR)
                seg = duck[i : i + n]
                seg[:] = np.minimum(seg, depth + (1 - depth) * np.linspace(0, 1, n)[: len(seg)] ** 2)
        for t in s:
            g = gs * 10 ** (rng.uniform(-3, 1) / 20)
            put(drums, t, snare, g)
            if clap is not None:
                put(drums, t + rng.uniform(0, 0.004), clap, g * 0.8)
        # charleston, toms, percussions : entre les coups (moitié ou quarts de l'intervalle)
        ev = np.unique(np.concatenate([k, s]))
        hat_p, sub4 = rng.uniform(0.3, 1.0), rng.random() < 0.4
        for t0, t1 in zip(ev[:-1], ev[1:]):
            d = t1 - t0
            if not 0.15 < d < 1.2:
                continue
            for f in [0.25, 0.5, 0.75] if sub4 and d > 0.3 else [0.5]:
                if rng.random() < hat_p and others.get("hh"):
                    put(drums, t0 + f * d, pick(others["hh"]), gh * rng.uniform(0.5, 1))
                u = rng.random()
                if u < 0.03 and others.get("tom"):
                    put(drums, t0 + f * d, pick(others["tom"]), gs * rng.uniform(0.5, 1.2))
                elif u < 0.05 and others.get("perc"):
                    put(drums, t0 + f * d, pick(others["perc"]), gs * rng.uniform(0.3, 0.9))
                elif u < 0.07 and others.get("oh"):
                    put(drums, t0 + f * d, pick(others["oh"]), gh * 1.5)
        for t in ev[:: max(1, int(rng.integers(8, 40)))]:
            if others.get("cr") and rng.random() < 0.5:
                put(drums, t, pick(others["cr"]), gs * 0.6)
        mix = back * duck + drums
        if rng.random() < 0.5:  # écrêtage doux (mastering)
            mix = np.tanh(mix * rng.uniform(1, 3) / (np.percentile(np.abs(mix), 99.9) + 1e-6))
        out = str(dst / f"{name}_v{v}")
        finish(mix, rng).tofile(out + ".f32")
        pathlib.Path(out + ".kicks").write_text("".join(f"{t:.4f}\n" for t in k))
        pathlib.Path(out + ".caisses").write_text("".join(f"{t:.4f}\n" for t in s))
        pathlib.Path(out + ".kicks_doute").write_text("".join(f"{t:.4f}\n" for t in doubt_k))
        pathlib.Path(out + ".caisses_doute").write_text("".join(f"{t:.4f}\n" for t in doubt_s))
    if not test:  # extrait sans batterie
        L = min(dur, 60.0)
        a = rng.uniform(0, max(0.0, dur - L))
        sb = dst.parent / (dst.name + "sb")
        sb.mkdir(exist_ok=True)
        out = str(sb / f"{name}_v9")
        finish(back_full[int(a * SR) : int((a + L) * SR)], rng).tofile(out + ".f32")
        for suf, r in ((".kicks", K), (".caisses", S)):
            pathlib.Path(out + suf).write_text("")
            pathlib.Path(out + suf + "_doute").write_text("".join(f"{t - a:.4f}\n" for t in r[(r >= a) & (r < a + L)]))
    return name


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if a != "--essai"]
    if len(args) < 4:
        sys.exit(__doc__)
    data, stems_dir, smp, dst = map(pathlib.Path, args[:4])
    seed, test = int(args[4]) if len(args) > 4 else 1, "--essai" in sys.argv
    dst.mkdir(parents=True, exist_ok=True)
    names = [f.stem for f in sorted(stems_dir.glob("*.npy")) if (data / (f.stem + ".kicks")).exists()]
    with Pool(8) as pool:
        for name in pool.imap_unordered(render, [(data, stems_dir, smp, n, dst, seed, test) for n in names]):
            pass
    print(len(names), "morceaux ->", dst)
