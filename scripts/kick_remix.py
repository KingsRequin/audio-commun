"""Remixages à étiquettes exactes pour l'apprentissage du kick et de la caisse claire (`kick_modele.py`).

L'accompagnement réel d'un morceau (basse + voix + reste, séparés par Demucs : `kick_bench.py ref
--demucs --pistes`) reçoit une batterie refaite avec des sons isolés (boîtes à rythmes, banque Dirt,
kicks de synthèse), posés aux instants de la batterie d'origine (sa transcription) : les instants
sont connus à l'échantillon près, et la basse et les voix restent celles d'un vrai morceau.

    python kick_remix.py <données> <pistes> <banques> <sortie> [graine=1] [--essai] [--acoustique | --packs]

- `<données>` : `<nom>.kicks` et `<nom>.caisses` des morceaux d'origine ; `<pistes>` : `<nom>.npy`
  (int16, 4 pistes drums/bass/other/vocals à 44,1 kHz) ;
- `<banques>` : dossier contenant `tdm` (<https://github.com/geikha/tidal-drum-machines>), `dirt`
  (<https://github.com/tidalcycles/Dirt-Samples>), pour `--acoustique`, `acoustique` (un dossier par
  kit de batterie acoustique échantillonné : voir `acoustic_kits`) et, pour `--packs`, `packs` (coups
  isolés de packs de production actuels : voir `pack_kits`) ;
- `<sortie>` : par morceau, deux extraits de ~100 s rebatterisés (`<nom>_v0`, `_v1`, kits différents,
  quelques kicks retirés : leurs instants vont dans `.kicks_doute`, un reste de l'ancien coup pouvant
  subsister) ; et dans `<sortie>sb`, un extrait sans batterie (`<nom>_v9` : aucun coup ; les instants
  des coups d'origine sont douteux pour la même raison) ;
- `--acoustique` : un seul extrait par morceau, rebatterisé avec un kit acoustique : chaque coup est une
  autre frappe du même fût (nuances et variantes du kit, micros mélangés), avec parfois des notes
  fantômes à la caisse claire (`.caisses_doute`) et une cymbale ride au lieu du charleston ;
- `--packs` : un seul extrait par morceau, rebatterisé avec des coups isolés de packs de production
  actuels (kicks de hardstyle, de techno, de trap ; caisses claires et claps d'électro) : le kick et la
  caisse claire peuvent venir de deux kits, un clap double souvent la caisse claire ou la remplace ;
  `<nom>_v0.sons` nomme les sons joués ;
- `--essai` : un seul extrait par morceau, avec les boîtes à rythmes (ou, avec `--acoustique` ou
  `--packs`, les kits) tenues à l'écart (`TEST_MACHINES`, `TEST_ACOUSTIC`, `pack_test`), sans kick
  retiré : jeu d'évaluation à étiquettes exactes.

Dépendances : numpy, scipy, soundfile.
"""
import functools, pathlib, re, sys, warnings, zlib
from multiprocessing import Pool

import numpy as np
import soundfile as sf
from scipy import signal

SR = 44100
KICK_DIRT = ["808bd", "clubkick", "hardkick", "popkick", "kicklinn", "reverbkick", "bd", "gabba", "house", "drumtraks", "electro1"]
TEST_MACHINES = {"RolandTR909", "LinnDrum", "AkaiMPC60", "KorgM1", "BossDR660", "EmuSP12", "YamahaRY30", "OberheimDMX", "RolandR8", "SimmonsSDS5"}
TEST_ACOUSTIC = {"karoryfer.unruly-drums", "muldjordkit", "h2-YamahaVintageKit", "h2-ColomboAcousticDrumkit", "h2-Dave_Grohl"}
# Type d'un instrument acoustique d'après son chemin en minuscules (le premier motif trouvé l'emporte) ;
# `rim` : baguette posée sur le cercle, comptée comme caisse claire (ADTOF fait de même). `B…E` : le mot
# entier, entre séparateurs (`bd_01`, `01-sd12-normal`).
B, E = r"(^|[/_ +-])", r"([/_ +-]|$)"
AC_TYPES = [
    ("rim", rf"side ?stick|rim ?click|xstick|cross|stick-rim|{B}rim{E}"),
    ("bd", rf"kick|kdrum|bass ?drum|kik|thump|{B}(bd|bass){E}"),
    ("cp", r"clap"),
    ("sd", rf"snare|snar|slap|{B}(sd|sn){E}"),
    ("oh", rf"(hat|hh).*(open|opn|offen|on|semi|half){E}|open.*(hat|hh)|{B}(oh|ho|op_hh|open){E}"),
    ("hh", rf"hat|hh|{B}ch{E}"),
    ("tom", rf"tom|{B}tm{E}"),
    ("rd", rf"ride|{B}(rd|rc){E}"),
    ("cr", rf"crash|china|splash|crsh|{B}cc{E}"),
    ("perc", rf"cowbell|tamb|maraca|shaker|djembe|darbouka|bongo|conga|agogo|timbal|tumba|woodblock|clave|{B}cb{E}"),
]
# Kits dont les dossiers ne nomment pas l'instrument en toutes lettres.
AC_KITS = {
    "karoryfer.unruly-drums": [("bd", r"/k\d"), ("sd", r"/s\d"), ("hh", r"/h\d"), ("rd", r"/r\d"), ("cr", r"/c\d")],
    "karoryfer.frankensnare": [("sd", r"_(basic|rimshot|brush|prep|muted|tight|spring)")],  # caisses claires seules
    "virtuosity_drums": [("bd", r"/kick$"), ("sd", r"/snare$"), ("hh", r"/hh$"), ("rd", r"ride$"), ("cr", r"/crash$"), ("tom", r"tom$"), ("perc", r"/perc/")],
}
# Dossiers de micros dont le nom est repris dans celui des fichiers (Virtuosity Drums).
AC_MICS = {"oh", "room", "mid", "close", "lofi", "kickmic", "snaremic"}
# Jamais joués : étouffés, charleston au pied, roulements, balais frottés, bruits, cloches, caisse sans timbre.
AC_SKIP = r"choke|(hat|hh)\w*[ _-]?(foot|ft|pedal)|rest|roll|flam|drag|stir|flutter|swirl|swish|noise|release|stack|stick ?click|(?<!cow)bell|snare ?off|empty|chime"
# Nuance écrite dans le nom d'un fichier (`Kick-Hard`, `kick_low_vol`) : retirée, comme les chiffres, pour
# réunir les frappes d'un même instrument.
AC_LEVEL = r"\d+|hardest|hard|softest|soft|medium|med|(high|low)_vol|normal"
AC_SOUND = {".wav", ".flac", ".ogg", ".aiff", ".aif"}
PACK_TYPES = ("bd", "sd", "cp", "hh", "oh", "tom", "cr", "perc")
# Sons de `packs` jamais joués : ceux des boîtes à rythmes de `TEST_MACHINES` (909 retraitée, LinnDrum
# d'un autre auteur), qui doivent rester inconnues de l'apprentissage.
PACK_HELD = rf"909|linn|mpc[ _-]?60|korg[ _-]?m1|dr[ _-]?660|sp[ _-]?12|ry[ _-]?30|dmx|simmons|{B}(sds|r[ _-]?8){E}"


def read(path):
    """Le fichier en mono à 44,1 kHz, tel quel (None : illisible)."""
    try:
        x, sr = sf.read(path, dtype="float32", always_2d=True)
    except Exception:
        return None
    x = x.mean(1)
    return signal.resample_poly(x, SR, sr).astype(np.float32) if sr != SR else x


def trim(x):
    """(son coupé à son début, 1,6 s au plus, pic à 1 ; pic d'origine), ou None s'il est vide."""
    pk = np.abs(x).max() if x is not None and len(x) else 0
    if pk < 1e-4 or len(x) < 200:
        return None
    start = int(np.argmax(np.abs(x) > 0.03 * pk))  # début : premier échantillon au-dessus de -30 dB du pic
    x = x[max(0, start - 8) : start + int(1.6 * SR)].copy()
    x[-min(len(x), 400) :] *= np.linspace(1, 0, min(len(x), 400))
    return x / np.abs(x).max(), pk


def load_one(path):
    x = trim(read(path))
    return None if x is None or x[1] < 1e-3 else x[0]


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


def pack_test(name):
    """Un kit de `packs` sur cinq, tiré de son nom, est tenu à l'écart pour l'évaluation."""
    return zlib.crc32(name.encode()) % 5 == 0


@functools.lru_cache(None)
def pack_kits(smp, test):
    """{kit: {type: [sons]}} des kits de `<banques>/packs/<kit>/<type>/` ; mêmes types que `kits`.

    Coups isolés de packs de production actuels, rangés par `kick_packs.py ranger` (packs gratuits
    d'éditeurs et de dépôts libres) et `kick_freesound.py` (Freesound : un kit par auteur) ; les sons
    notés dans `packs/ecartes.txt` par `kick_packs.py trier` (attaque lente, début faible) sont laissés."""
    out, root = {}, smp / "packs"
    dropped = {l.split("\t")[0] for l in (root / "ecartes.txt").read_text(encoding="utf-8").splitlines()} if (root / "ecartes.txt").exists() else set()
    for kit in sorted(p for p in root.iterdir() if p.is_dir() and not p.name.startswith("_") and pack_test(p.name) == test):
        heard = lambda f: (
            f.suffix.lower() in AC_SOUND and not re.search(PACK_HELD, f"{kit.name}/{f.name}".lower()) and f.relative_to(root).as_posix() not in dropped
        )
        k = {d.name: sorted(f for f in d.iterdir() if heard(f)) for d in sorted(kit.iterdir()) if d.name in PACK_TYPES and d.is_dir()}
        k = {t: fs for t, fs in k.items() if fs}
        if k:
            out[kit.name] = k
    return out


@functools.lru_cache(None)
def acoustic_kits(smp, test):
    """{kit: {type: [instrument]}} des kits de batterie acoustique de `<banques>/acoustique/<kit>/`.

    Un instrument est une liste de frappes (toutes les nuances et variantes d'un même fût joué d'une même
    façon) ; une frappe est la liste de ses prises `(micro, fichier)`. Deux dispositions sont reconnues :
    un fichier par frappe (`36-Pearl22Kick-3.flac`, `Kick-Hard.wav` : l'instrument est le nom sans ses
    chiffres ni sa nuance), ou un dossier par micro, les mêmes fichiers se retrouvant sous chacun
    (`kick_1/kick/{kick,oh,sn}/k_vl3_rr2.flac`, `s14/{top,btm,ohl}/rr1/...`, `{oh,room}/snare/...` :
    l'instrument est le chemin sans le dossier du micro). Le type vient du chemin de l'instrument
    (`AC_TYPES`, ou `AC_KITS` pour un kit aux noms abrégés).

    Kits utilisés (tous libres) : AVL Drumkits, Karoryfer (Big Rusty Drums, Swirly Drums, Frankensnare,
    Unruly Drums), Virtuosity Drums, MuldjordKit (<https://github.com/sfzinstruments>,
    <https://github.com/freepats/muldjordkit>, <https://github.com/studiorack/avl-drumkits>) et les kits
    acoustiques de Hydrogen (<https://sourceforge.net/projects/hydrogen/files/Sound%20Libraries/>,
    extraits dans `h2-<nom>`).
    """
    out = {}
    bare = lambda name: re.sub(r"\d+", "", name)  # `Snare1` et `Snare2` : deux fûts, pas deux micros
    for kit in sorted(p for p in (smp / "acoustique").iterdir() if p.is_dir()):
        if (kit.name in TEST_ACOUSTIC) != test:
            continue
        files = sorted(f.relative_to(kit).parts for f in kit.rglob("*") if f.suffix.lower() in AC_SOUND and not f.name.startswith("._"))
        have, children = set(files), {}
        for parts in files:
            for i in range(len(parts) - 1):
                children.setdefault(parts[:i], set()).add(parts[i])
        inst = {}  # {chemin de l'instrument: {frappe: [(micro, fichier)]}}
        for parts in files:
            # dossier de micro : le plus profond dont un voisin d'un autre nom contient le même fichier
            # (au nom du micro près s'il est connu : `oh/kick/oh_kick_vl1.flac`, `room/kick/room_kick_vl1.flac`)
            named = lambda i, o: parts[i] in AC_MICS and o in AC_MICS
            twin = lambda i, o: parts[:i] + (o,) + tuple(p.replace(parts[i], o) if named(i, o) else p for p in parts[i + 1 :])
            mic = next((i for i in range(len(parts) - 2, -1, -1) if any(bare(o) != bare(parts[i]) and twin(i, o) in have for o in children[parts[:i]])), None)
            if mic is None:
                key, hit, name = parts[:-1] + (re.sub(AC_LEVEL, "", pathlib.Path(parts[-1]).stem.lower()),), parts[-1], ""
            else:
                name = parts[mic]
                below = tuple(p.replace(name, "") if name in AC_MICS else p for p in parts[mic + 1 :])
                key, hit = parts[:mic] + tuple(bare(p) for p in below[:-1]), below
            inst.setdefault("/".join(key), {}).setdefault(hit, []).append((name, kit.joinpath(*parts)))
        types, k = AC_KITS.get(kit.name, AC_TYPES), {}
        for key, hits in inst.items():
            name = "/" + key.lower()
            typ = None if re.search(AC_SKIP, name) else next((t for t, pat in types if re.search(pat, name)), None)
            if typ:
                k.setdefault(typ, []).append(list(hits.values()))
        if k:
            out[kit.name] = k
    return out


def voice(inst, rng, n=10, tune=1.0):
    """`n` frappes d'un instrument, la plus forte d'abord : [(niveau en dB sous la plus forte, son)].
    Les micros d'une frappe sont mélangés avec des poids tirés une fois pour l'instrument."""
    weights, out = {}, []
    for files in [inst[i] for i in rng.choice(len(inst), min(n, len(inst)), replace=False)]:
        mics = [(mic, x) for mic, x in ((mic, read(f)) for mic, f in files) if x is not None]
        if not mics:
            continue
        mix = np.zeros(max(len(x) for _, x in mics), np.float32)
        for mic, x in mics:
            mix[: len(x)] += weights.setdefault(mic, rng.uniform(0.25, 1.0)) * x
        t = trim(mix)
        if t is not None:
            x = signal.resample(t[0], max(64, int(len(t[0]) / tune))).astype(np.float32) if abs(tune - 1) > 0.02 else t[0]
            out.append((20 * np.log10(t[1]), x / (np.abs(x).max() + 1e-9)))
    top = max((lv for lv, _ in out), default=0.0)
    return sorted(((lv - top, x) for lv, x in out), key=lambda h: -h[0])


def strike(v, rng, soft=False):
    """(son, gain) d'un coup : une frappe franche (à moins de 6 dB de la plus forte) à son niveau
    naturel, ou, `soft`, une frappe légère (note fantôme)."""
    main = [h for h in v if h[0] >= -6]
    pool = ([h for h in v if h[0] < -6] or main) if soft else main
    lv, x = pool[rng.integers(len(pool))]
    return x, 10 ** (lv / 20)


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
    data, stems_dir, smp, name, dst, seed, test, mode = args
    acoustic = mode == "acoustique"
    rng = np.random.default_rng(zlib.crc32(name.encode()) + seed)
    stems = np.load(stems_dir / (name + ".npy")).astype(np.float32) / 32768
    back_full = stems[1] + stems[2] + stems[3]
    K, S = times(data / (name + ".kicks")), times(data / (name + ".caisses"))
    dur = len(back_full) / SR
    allk = {"machines": kits, "acoustique": acoustic_kits, "packs": pack_kits}[mode](smp, test)
    names = sorted(n for n, k in allk.items() if "bd" in k)
    pick = lambda xs: xs[rng.integers(len(xs))]
    ld = lambda fs: [y for y in (load_one(f) for f in fs) if y is not None]

    def among(t):
        """Un kit de `packs` qui a le type `t` (None : aucun), les kits fournis étant tirés plus
        souvent (racine du nombre de sons : un auteur d'un seul kick garde sa chance)."""
        ns = sorted(n for n, k in allk.items() if t in k)
        w = np.sqrt([len(allk[n][t]) for n in ns])
        return allk[ns[rng.choice(len(ns), p=w / w.sum())]] if ns else None

    for v in range(1 if test or mode != "machines" else 2):
        L = min(dur, 100.0)
        a = rng.uniform(0, max(0.0, dur - L))
        back = back_full[int(a * SR) : int((a + L) * SR)].copy()
        kit = among("bd") if mode == "packs" else allk[pick(names)]
        rms = np.sqrt(np.mean(back**2)) + 1e-4
        clap = None
        if acoustic:
            # Batterie acoustique : chaque coup est une autre frappe du même fût, à son niveau naturel.
            tune = lambda: 2 ** (rng.uniform(-2, 2) / 12)
            kv = voice(pick(kit["bd"]), rng, tune=tune())
            # caisse claire du kit, ou une fois sur trois celle d'un autre (dont les kits de caisses claires seules)
            skit = kit if "sd" in kit and rng.random() < 0.67 else allk[pick(sorted(n for n, k in allk.items() if "sd" in k))]
            sv = voice(pick(skit["rim" if "rim" in skit and rng.random() < 0.1 else "sd"]), rng, tune=tune())
            if not kv or not sv:
                continue
            others = {t: [x for _, x in voice(pick(kit[t]), rng, n=4)] for t in ("hh", "oh", "tom", "cr", "perc") if t in kit}
            if "rd" in kit and rng.random() < 0.3:  # cymbale ride à la place du charleston (jazz)
                others["hh"] = [x for _, x in voice(pick(kit["rd"]), rng, n=4)]
            # niveaux mesurés sur MDB Drums (pic du coup face à l'accompagnement : +6 à +15 dB hors jazz)
            gk = rms * 10 ** (rng.uniform(5, 17) / 20)
            gs = gk * 10 ** (rng.uniform(-6, 3) / 20)
            gh = gk * 10 ** (rng.uniform(-20, -6) / 20)

            def hit(voc, g):
                x, nat = strike(voc, rng)
                return x, g * nat * 10 ** (rng.uniform(-3, 1.5) / 20)

            hit_k, hit_s = (lambda: hit(kv, gk)), (lambda: hit(sv, gs))
        else:
            if mode == "packs":
                # Packs de production : la caisse claire vient une fois sur deux d'un autre kit ; un clap
                # la double souvent, ou la remplace (pop, trap, house : le clap compte comme caisse claire).
                used = {"kick": pick(kit["bd"])}
                skit = kit if "sd" in kit and rng.random() < 0.5 else among("sd")
                ckit = skit if "cp" in skit else among("cp")
                if ckit and rng.random() < 0.2:
                    used["caisse"] = pick(ckit["cp"])
                else:
                    used["caisse"] = pick(skit["sd"])
                    if ckit and rng.random() < 0.4:
                        used["clap"] = pick(ckit["cp"])
                kick, snare, clap = ld([used["kick"]]), ld([used["caisse"]]), ld([used["clap"]]) if "clap" in used else []
                # charleston, toms, percussions : du kit du kick s'il en a, sinon d'un autre
                okit = {t: k[t] for t in ("hh", "oh", "tom", "cr", "perc") for k in [kit if t in kit else among(t)] if k}
            else:
                u = rng.random()
                if test or u < 0.45:  # kick : boîte à rythmes, banque Dirt, ou synthèse
                    kick = ld([pick(kit["bd"])])
                elif u < 0.7:
                    kick = ld([pick(dirt(smp, KICK_DIRT))])
                else:
                    kick = [synth_kick(rng)]
                snare = ld([pick(kit["sd"])])
                clap = ld([pick(kit["cp"])]) if "cp" in kit and rng.random() < 0.4 else []
                okit = kit
            if not kick or not snare:
                continue
            kick, snare = shape(kick[0], rng), shape(snare[0], rng)
            clap = shape(clap[0], rng) if clap else None
            others = {t: ld(list(rng.choice(okit[t], min(3, len(okit[t])), replace=False))) for t in ("hh", "oh", "tom", "cr", "perc") if t in okit}
            gk = rms * 10 ** (rng.uniform(4, 16) / 20)
            gs = gk * 10 ** (rng.uniform(-8, 2) / 20)
            gh = gk * 10 ** (rng.uniform(-22, -8) / 20)
            hit_k = lambda: (kick, gk * 10 ** (rng.uniform(-2, 1) / 20))
            hit_s = lambda: (snare, gs * 10 ** (rng.uniform(-3, 1) / 20))
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
        # compression latérale sur le kick (musique électronique) : jamais avec une batterie acoustique
        depth, rel = (10 ** (-rng.uniform(3, 12) / 20), rng.uniform(0.08, 0.3)) if not acoustic and rng.random() < 0.4 else (1.0, 0.1)
        for j, t in enumerate(k):
            x, g = hit_k()
            # Kick d'un pack : monophonique, comme dans une production (le suivant coupe la queue d'un
            # kick long de hardstyle ou d'une 808, au lieu de s'y ajouter).
            cut = int((k[j + 1] - t) * SR) if mode == "packs" and j + 1 < len(k) else len(x)
            if 0 < cut < len(x):
                x = x[:cut].copy()
                x[-min(cut, 220) :] *= np.linspace(1, 0, min(cut, 220))
            put(drums, t, x, g)
            if depth < 1:
                i, n = int(t * SR), int(rel * SR)
                seg = duck[i : i + n]
                seg[:] = np.minimum(seg, depth + (1 - depth) * np.linspace(0, 1, n)[: len(seg)] ** 2)
        for t in s:
            x, g = hit_s()
            put(drums, t, x, g)
            if clap is not None:
                put(drums, t + rng.uniform(0, 0.004), clap, g * 0.8)
        # charleston, toms, percussions : entre les coups (moitié ou quarts de l'intervalle)
        ev = np.unique(np.concatenate([k, s]))
        hat_p, sub4 = rng.uniform(0.3, 1.0), rng.random() < 0.4
        ghost_p = rng.uniform(0.05, 0.3) if acoustic and rng.random() < 0.5 else 0.0
        for t0, t1 in zip(ev[:-1], ev[1:]):
            d = t1 - t0
            if not 0.15 < d < 1.2:
                continue
            for f in [0.25, 0.5, 0.75] if sub4 and d > 0.3 else [0.5]:
                if rng.random() < hat_p and others.get("hh"):
                    put(drums, t0 + f * d, pick(others["hh"]), gh * rng.uniform(0.5, 1))
                if ghost_p and rng.random() < ghost_p:
                    # note fantôme : frappe légère de la caisse claire, ni coup ni non-coup pour l'apprentissage
                    put(drums, t0 + f * d, strike(sv, rng, soft=True)[0], gs * 10 ** (-rng.uniform(14, 26) / 20))
                    doubt_s = np.append(doubt_s, t0 + f * d)
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
        pathlib.Path(out + ".caisses_doute").write_text("".join(f"{t:.4f}\n" for t in np.sort(doubt_s)))
        if mode == "packs":  # sons joués, pour remonter d'un extrait mal reconnu à ses sons
            pathlib.Path(out + ".sons").write_text("".join(f"{role}\t{f.relative_to(smp / 'packs').as_posix()}\n" for role, f in used.items()), encoding="utf-8")
    if not test and mode == "machines":  # extrait sans batterie
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
    args = [a for a in sys.argv[1:] if a not in ("--essai", "--acoustique", "--packs")]
    if len(args) < 4 or ("--acoustique" in sys.argv and "--packs" in sys.argv):
        sys.exit(__doc__)
    data, stems_dir, smp, dst = map(pathlib.Path, args[:4])
    seed, test = int(args[4]) if len(args) > 4 else 1, "--essai" in sys.argv
    mode = "acoustique" if "--acoustique" in sys.argv else "packs" if "--packs" in sys.argv else "machines"
    dst.mkdir(parents=True, exist_ok=True)
    names = [f.stem for f in sorted(stems_dir.glob("*.npy")) if (data / (f.stem + ".kicks")).exists()]
    with Pool(8) as pool:
        for name in pool.imap_unordered(render, [(data, stems_dir, smp, n, dst, seed, test, mode) for n in names]):
            pass
    print(len(names), "morceaux ->", dst)
