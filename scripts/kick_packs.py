"""Prépare les kits de `kick_remix.py --packs` : range des packs téléchargés, puis trie tous les sons.

    python kick_packs.py ranger <packs bruts> <banques>/packs
    python kick_packs.py trier <banques>/packs

`ranger` : `<packs bruts>/<pack>/...` est un dossier par pack, dans la disposition de son auteur (un
dossier dont le nom commence par `_` est ignoré). Chaque son est classé d'après son nom de fichier, à
défaut d'après ses dossiers en remontant (`Kicks/01.wav`), jusqu'au nom du pack
(`hardstyle-kicks/000 - D1.wav`) ; sont écartés les boucles (nom, ou durée de plus de 2,5 s), les sons qui
nomment plusieurs instruments et ceux qui n'en nomment aucun (basses, effets, voix). Un pack = un kit :

    <banques>/packs/<pack>/<type>/<chemin d'origine aplati>      types : bd, sd, cp, hh, oh, tom, cr, perc

Les fichiers sont copiés ; un pack déjà rangé est refait.

`trier` : écoute les kicks, les caisses claires et les claps de tous les kits (ceux de
`kick_freesound.py` compris) et note dans `<banques>/packs/ecartes.txt` ceux qui ne sont pas des coups
francs : attaque lente (plus de 30 ms pour atteindre la moitié du pic : l'instant du coup serait mal
défini) ou début trop faible face au pic (un clic, un souffle). `kick_remix.py` ne les joue pas. Les
seuils laissent passer 98 % ou plus des sons des boîtes à rythmes de `tidal-drum-machines`.

Dépendances : numpy, scipy, soundfile.
"""
import pathlib, re, shutil, sys
from multiprocessing import Pool

import numpy as np
import soundfile as sf

import kick_remix
from kick_freesound import SKIP_NAME, TYPES

SOUND = {".wav", ".flac", ".ogg", ".aiff", ".aif"}
MAX_S, MIN_S = 2.5, 0.02
# Abréviations des noms de fichiers de packs (`ClHat03`, `OpHat01`), en plus des motifs de Freesound.
ABBREV = {"oh": r"op[ _-]?hat", "hh": r"cl[ _-]?hat"}
PATTERNS = [(t, pat + "|" + ABBREV[t] if t in ABBREV else pat) for t, pat in TYPES]
ATTACK_MS = 30.0  # temps laissé à l'enveloppe (lissée sur 2 ms) pour atteindre la moitié de son pic
LEVEL_DB = {"bd": -13.0, "sd": -17.0, "cp": -18.0}  # niveau efficace minimal des 50 premières ms, pic à 0 dB


def classify(parts):
    """Type d'un son d'après son chemin (en minuscules, nom du pack compris ; lu du fichier vers le
    pack) ; None s'il est ambigu ou sans instrument reconnu."""
    if re.search(SKIP_NAME, "/".join(parts[1:])):
        return None
    for text in reversed(parts):
        found = [t for t, pat in PATTERNS if re.search(pat, text)]
        if "oh" in found and "hh" in found:
            found.remove("hh")
        if len(found) == 1:
            return found[0]
        if found:
            return None
    return None


def arrange(src, dest):
    for pack in sorted(p for p in src.iterdir() if p.is_dir() and not p.name.startswith("_")):
        out, count, skipped = dest / pack.name, {}, 0
        if out.exists():
            shutil.rmtree(out)
        for f in sorted(pack.rglob("*")):
            if f.suffix.lower() not in SOUND or f.name.startswith("._"):
                continue
            rel = f.relative_to(pack)
            typ = classify([p.lower() for p in (pack.name, *rel.with_suffix("").parts)])
            try:
                info = sf.info(f)
                ok = typ is not None and MIN_S <= info.frames / info.samplerate <= MAX_S
            except Exception:
                ok = False
            if not ok:
                skipped += 1
                continue
            (out / typ).mkdir(parents=True, exist_ok=True)
            shutil.copyfile(f, out / typ / re.sub(r"[^A-Za-z0-9._-]+", "_", "__".join(rel.parts))[-150:])
            count[typ] = count.get(typ, 0) + 1
        print(f"{pack.name} : " + (", ".join(f"{t} {n}" for t, n in sorted(count.items())) or "rien") + f" ({skipped} écartés)", flush=True)


def judge(path):
    """Raison d'écarter un kick, une caisse claire ou un clap, ou None s'il est franc."""
    x = kick_remix.load_one(path)  # le son tel que `kick_remix.py` le jouerait : coupé à son début, pic à 1
    if x is None:
        return "illisible ou vide"
    n = int(kick_remix.SR * 0.002)
    env = np.convolve(np.abs(x), np.ones(n) / n, mode="same")
    attack = np.argmax(env >= 0.5 * env.max()) / kick_remix.SR * 1000
    level = 20 * np.log10(np.sqrt(np.mean(x[: int(kick_remix.SR * 0.05)] ** 2)) + 1e-9)
    if attack > ATTACK_MS:
        return f"attaque lente ({attack:.0f} ms)"
    if level < LEVEL_DB[path.parent.name]:
        return f"début faible ({level:.0f} dB)"
    return None


def sort_out(dest):
    files = sorted(f for kit in dest.iterdir() if kit.is_dir() and not kit.name.startswith("_") for t in LEVEL_DB for f in (kit / t).glob("*") if f.suffix.lower() in SOUND)
    with Pool(8) as pool:
        why = pool.map(judge, files, chunksize=50)
    out = [f"{f.relative_to(dest).as_posix()}\t{w}\n" for f, w in zip(files, why) if w]
    (dest / "ecartes.txt").write_text("".join(out), encoding="utf-8", newline="\n")
    for t in LEVEL_DB:
        n = sum(1 for f in files if f.parent.name == t)
        print(f"{t} : {sum(1 for f, w in zip(files, why) if w and f.parent.name == t)} écartés sur {n}")


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    args = sys.argv[1:]
    if args[:1] == ["ranger"] and len(args) == 3:
        arrange(pathlib.Path(args[1]), pathlib.Path(args[2]))
    elif args[:1] == ["trier"] and len(args) == 2:
        sort_out(pathlib.Path(args[1]))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
