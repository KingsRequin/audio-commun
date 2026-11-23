"""Coups de batterie isolés de Freesound, rangés en kits pour `kick_remix.py --packs`.

    python kick_freesound.py <fichier de la clé d'API> <banques>/packs [pages=4]

Interroge l'API de Freesound (<https://freesound.org/docs/api/>) : sons de moins de 2 s, sous licence
CC0 ou CC BY, cherchés par instrument et par style (kicks hardstyle, techno, trap ; caisses claires et
claps d'électro ; charlestons, toms, cymbales, percussions). Chaque son est classé d'après son nom
seul (les étiquettes de Freesound nomment souvent tout le kit, ou un son qui n'a rien d'une batterie) ;
ceux qui nomment plusieurs instruments, les boucles, les bruitages et les sons hors musique (un « kick »
dans une porte) sont écartés. Un auteur = un kit :

    <banques>/packs/fs-<auteur>/<type>/<identifiant>_<nom>.ogg      types : bd, sd, cp, hh, oh, tom, cr, perc

Avec une clé d'API seule, Freesound ne donne que l'écoute en Ogg Vorbis (qualité haute, ~192 kb/s) : c'est
ce qui est téléchargé. `<banques>/packs/freesound.tsv` garde l'auteur, la licence et l'adresse de chaque
son (les sons CC BY demandent de citer leur auteur si on les redistribue : ils restent hors du dépôt).
Les réponses de l'API sont gardées dans `<banques>/packs/freesound-recherche.json` : relancé, le script
ne réinterroge pas Freesound (effacer ce fichier pour chercher à nouveau), ne retélécharge pas ce qui est
là, et retire des dossiers `fs-*` les sons qu'il ne garde plus. Passer ensuite `kick_packs.py trier`.
Dépendances : aucune (bibliothèque standard).
"""
import json, pathlib, re, sys, time, urllib.parse, urllib.request
from concurrent.futures import ThreadPoolExecutor

API = "https://freesound.org/apiv2/search/text/"
FILTER = 'duration:[0.03 TO 2.0] license:("Creative Commons 0" OR "Attribution")'
FIELDS = "id,name,tags,license,username,duration,previews"
# Recherches par type, de la plus générale à la plus précise.
QUERIES = {
    "bd": ["kick", "kick drum", "bass drum", "hardstyle kick", "hardcore kick", "gabber kick", "techno kick", "house kick", "trap kick",
           "808 kick", "909 kick", "dubstep kick", "edm kick", "drum and bass kick", "distorted kick", "punchy kick", "sub kick", "electro kick"],
    "sd": ["snare", "snare drum", "trap snare", "dubstep snare", "edm snare", "drum and bass snare", "electronic snare", "808 snare", "909 snare",
           "house snare", "techno snare", "hip hop snare", "pop snare", "layered snare", "rimshot"],
    "cp": ["clap", "hand clap", "808 clap", "909 clap", "edm clap", "trap clap", "house clap", "techno clap", "layered clap"],
    "hh": ["closed hi hat", "hihat", "trap hat", "808 hat", "909 hat"],
    "oh": ["open hi hat", "open hat"],
    "tom": ["tom drum", "electronic tom", "808 tom", "floor tom"],
    "cr": ["crash cymbal", "ride cymbal"],
    "perc": ["percussion one shot", "cowbell", "shaker", "conga", "bongo", "tambourine", "clave", "woodblock"],
}
B, E = r"(^|[^a-z])", r"([^a-z]|$)"  # mot entier : entre deux caractères qui ne sont pas des lettres (`sd_01`, `hh2`)
# Un son qui correspond à `oh` et à `hh` est un charleston ouvert.
TYPES = [
    ("bd", rf"kick|bass ?drum|bassdrum|{B}(bd|kik|kck){E}"),
    ("sd", rf"snare|rim ?shot|{B}(sd|snr){E}"),
    ("cp", r"clap"),
    ("oh", rf"open[ _-]?(hi[ _-]?)?hats?|(hat|hh)s?[ _-]?(open|op){E}|{B}(oh|ohh|ohat|hho){E}"),
    ("hh", rf"hi[ _-]?hat|{B}(hh|hat|chh|hats){E}"),
    ("tom", rf"{B}(tom|toms|floortom){E}"),
    ("cr", r"crash|cymbal|ride|splash|china"),
    ("perc", rf"perc|cowbell|shaker|conga|bongo|tambourine|clave|wood ?block|{B}rim{E}"),
]
# Jamais des coups isolés (aussi pour les chemins des packs : `kick_packs.py`).
SKIP_NAME = r"loop|bpm|groove|pattern|fill|roll|sequence|break|rhythm|riff|melody|song|demo|reverse"
# Bruitages et sons de bouche que leurs auteurs nomment « kick », « snare » ou « clap ».
SKIP_FOLEY = r"applause|laser|bullet|snap|stick|bottle|door|ball|body|mouth|beatbox|vocal|voice|foley"
SKIP_TAGS = {"loop", "loops", "drum-loop", "drumloop", "vocal", "vocals", "voice", "speech", "acapella", "field-recording", "foley", "reverse", "reversed",
             "applause", "beatbox"}
# Un mot de musique est exigé (nom ou étiquettes) : « kick » seul trouve aussi des portes et des ballons.
MUSIC = r"drum|percussion|808|909|707|606|techno|house|trap|edm|electro|hardstyle|hardcore|gabber|dubstep|dnb|hip-?hop|synth|sample|one-?shot|beat|kit|trance|bass|snare|clap|hat|cymbal"
# Plafonds : sons gardés par type, et par auteur pour un même type (la variété des auteurs compte plus que le nombre).
CAP = {"bd": 4000, "sd": 3000, "cp": 1500, "hh": 1200, "oh": 600, "tom": 600, "cr": 300, "perc": 1000}
PER_AUTHOR = 40


def classify(name, tags):
    """Type du son d'après son nom ; None s'il est ambigu, sans instrument nommé ou hors sujet."""
    name = re.sub(r"\.(wav|aiff?|flac|ogg|mp3)$", "", name.lower())
    if re.search(SKIP_NAME + "|" + SKIP_FOLEY, name) or SKIP_TAGS & {t.lower() for t in tags} or not re.search(MUSIC, name + " " + " ".join(tags).lower()):
        return None
    found = [t for t, pat in TYPES if re.search(pat, name)]
    if "oh" in found and "hh" in found:
        found.remove("hh")
    return found[0] if len(found) == 1 else None  # plusieurs instruments nommés : un kit entier ou une boucle


def get(url, key, tries=4):
    for k in range(tries):
        try:
            req = urllib.request.Request(url, headers={"Authorization": "Token " + key, "User-Agent": "kick-freesound/1.0"})
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.load(r)
        except Exception as e:
            if k == tries - 1:
                print("  échec :", e, flush=True)
            time.sleep(5 * (k + 1))
    return None


def search(key, pages, cache):
    """{recherche: [sons]} : toutes les réponses de l'API, relues de `cache` s'il existe."""
    if cache.exists():
        return json.loads(cache.read_text(encoding="utf-8"))
    found = {}
    for typ, queries in QUERIES.items():
        for q in queries:
            found[q] = []
            for page in range(1, pages + 1):
                params = {"query": q, "filter": FILTER, "fields": FIELDS, "page_size": 150, "page": page, "sort": "downloads_desc"}
                r = get(API + "?" + urllib.parse.urlencode(params), key)
                time.sleep(1.1)  # 60 requêtes par minute au plus
                if not r:
                    break
                found[q] += r["results"]
                if not r.get("next"):
                    break
            print(f"{typ} « {q} » : {len(found[q])} sons", flush=True)
    cache.write_text(json.dumps(found), encoding="utf-8")
    return found


def fetch(job):
    url, path = job
    if path.exists() and path.stat().st_size > 0:
        return True
    for k in range(3):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "kick-freesound/1.0"})
            with urllib.request.urlopen(req, timeout=30) as r:
                data = r.read()
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            return True
        except Exception:
            time.sleep(2 * (k + 1))
    return False


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    key = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").strip()
    dest = pathlib.Path(sys.argv[2])
    pages = int(sys.argv[3]) if len(sys.argv) > 3 else 4
    dest.mkdir(parents=True, exist_ok=True)
    slug = lambda s, n: re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")[:n] or "x"
    kept, seen, per_author = {t: [] for t in QUERIES}, set(), {}
    for sounds in search(key, pages, dest / "freesound-recherche.json").values():
        for s in sounds:
            got = classify(s["name"], s["tags"])
            count = per_author.get((s["username"], got), 0)
            if s["id"] in seen or got is None or count >= PER_AUTHOR or len(kept[got]) >= CAP[got]:
                continue
            seen.add(s["id"])
            per_author[(s["username"], got)] = count + 1
            kept[got].append(s)
    print("gardés : " + ", ".join(f"{t} {len(v)}" for t, v in kept.items()), flush=True)
    jobs, rows = [], []
    for typ, sounds in kept.items():
        for s in sounds:
            path = dest / ("fs-" + slug(s["username"], 40)) / typ / f"{s['id']}_{slug(s['name'], 50)}.ogg"
            jobs.append((s["previews"]["preview-hq-ogg"], path))
            rows.append([str(s["id"]), typ, s["username"], s["license"], s["name"].replace("\t", " "), f"https://freesound.org/s/{s['id']}/"])
    with ThreadPoolExecutor(6) as pool:
        ok = list(pool.map(fetch, jobs))
    # Les sons d'un passage précédent que le classement ne garde plus sont retirés.
    wanted, removed = {path for _, path in jobs}, 0
    for f in [f for kit in dest.glob("fs-*") for f in kit.rglob("*.ogg")]:
        if f not in wanted:
            f.unlink()
            removed += 1
    for d in sorted((d for kit in dest.glob("fs-*") for d in [kit, *kit.iterdir()] if d.is_dir()), key=lambda d: -len(d.parts)):
        if not any(d.iterdir()):
            d.rmdir()
    rows = [r for r, good in zip(rows, ok) if good]
    head = ["identifiant", "type", "auteur", "licence", "nom", "adresse"]
    (dest / "freesound.tsv").write_text("".join("\t".join(r) + "\n" for r in [head, *sorted(rows)]), encoding="utf-8", newline="\n")
    print(f"{sum(ok)} sons de {len({r[2] for r in rows})} auteurs -> {dest} ({len(ok) - sum(ok)} échecs, {removed} retirés)")


if __name__ == "__main__":
    main()
