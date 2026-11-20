# audio-commun

*Windows sound capture (WASAPI loopback) and music analysis (spectrum, onsets, tempo, low-latency
kick and snare detection), shared by [Horizon](https://github.com/KingsRequin/horizon),
[Essaim](https://github.com/KingsRequin/essaim) and Prisme. Documentation in French below.
License: [MIT](LICENSE).*

Bibliothèque Rust partagée par **Prisme**, **Horizon** et **Essaim** :
capture du son de Windows et analyses musicales. Avant elle, chaque application en avait sa copie (trois
captures quasi identiques, deux analyses spectrales qui divergeaient).

| Module | Feature | Contenu | Utilisé par |
|---|---|---|---|
| `capture` | `capture` | Loopback WASAPI de la sortie par défaut ou d'une sortie choisie, mode événement, mono, MMCSS « Pro Audio », relance si la sortie change, silence injecté | les trois |
| `spectre` | `spectre` | FFT : bandes mel, graves/médiums/aigus, attaques kick/snare/hihat, tempo + PLL, intensité, drops, tonalité ; `spectre::start` = capture + analyse à cadence fixe | Prisme, Horizon |
| `kick` | (toujours) | Grosse caisse et caisse claire à 15–30 ms de délai (biquads sur 10 bandes + petits réseaux appris, sans FFT), niveau et hauteur de la basse | Essaim |
| `pulsation` | (toujours) | Tempo (BPM) et phase du temps d'après les seuls instants des kicks, sans FFT ; relais pendant un passage sans kick | Essaim |

Chaque application garde **sa propre capture** : faire passer l'analyse d'un processus à l'autre ajouterait
du délai (Essaim vise 10–20 ms) et ferait dépendre une application du lancement d'une autre, pour un gain
négligeable (une capture loopback lit le mélange déjà calculé par Windows).

## Utilisation

```toml
# Cargo.toml de l'application
audio-commun = { git = "https://github.com/KingsRequin/audio-commun", tag = "v0.3.0" }  # tout
audio-commun = { git = "https://github.com/KingsRequin/audio-commun", tag = "v0.3.0", default-features = false, features = ["capture"] }  # Essaim
```

Pour la modifier en même temps qu'une application, la cloner à côté et la désigner dans le
`.cargo/config.toml` (non suivi) de l'application :

```toml
[patch."https://github.com/KingsRequin/audio-commun"]
audio-commun = { path = "../audio-commun" }
```

## Sortie écoutée (`capture`, 0.2.0)

`capture::start(nom, sink)` écoute la sortie par défaut de Windows et suit ses changements (inchangé).
`capture::start_sur(nom, Sortie::Id(id), sink)` écoute une sortie précise ; `capture::sorties()` liste les
sorties actives, `(identifiant, nom affiché)`, et `Sortie::depuis_id("")` donne `Sortie::ParDefaut` (pratique
pour un réglage vide). Si la sortie choisie disparaît, la capture se replie sur la sortie par défaut et revient
sur la choisie dès qu'elle est de nouveau active (vérification toutes les 2 s, comme la sortie par défaut).

## Niveaux stéréo (`capture`)

Le son est remis au `Sink` en mono, mais `Sink::stereo(gauche, droite)` reçoit après chaque paquet le niveau
efficace (RMS) de chaque canal : de quel côté joue le son. Méthode facultative (vide par défaut) ; Essaim
s'en sert pour pousser l'essaim vers le côté qui joue le plus fort.

## Tempo des kicks (`pulsation`, 0.2.0)

`Pulsation` déduit le tempo des seuls instants des kicks, pour une application qui n'a pas la FFT
(`spectre::tempo` demande le flux spectral). À chaque kick, les tempos de 70 à 180 BPM (pas de 0,5) sont
essayés : alignement des phases des kicks des 8 dernières secondes × part des temps de la grille qui ont leur
kick (ce produit écarte le double et la moitié du tempo), puis ajustement par moindres carrés (période et
origine). Grille retenue seulement si les kicks sont réguliers (8 kicks au moins, 80 % sur la grille, écart
type sous 4 % de la période, 60 % des temps marqués) ; tempo annoncé après trois estimations concordantes.
Gardé pendant un passage sans kick tant que la musique joue (30 s au plus), oublié après 3 s de silence ;
`relais(t)` dit quand prendre le relais des kicks (tempo connu, musique, aucun kick depuis 2 s).

```rust
let mut p = audio_commun::pulsation::Pulsation::new();
p.kick(t);                 // à chaque kick (t en secondes, horloge croissante)
p.avancer(t, audible);     // régulièrement
p.bpm(); p.phase(t); p.relais(t);
```

Tests sur signaux synthétiques : kicks réguliers à 74, 90, 120, 128 et 174 BPM avec ±12 ms de gigue → tempo à
±1 BPM ; 200 suites de kicks irréguliers → aucun tempo ; silence → aucun tempo.

## Vérification

```
cargo test --release
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
```

Après une modification, recompiler et réinstaller les applications qui l'utilisent (`scripts\install.ps1`
de chacune) : les tests de la bibliothèque ne remplacent pas un essai en musique réelle.

## Kick et caisse claire

Détails de la méthode en tête de `src/kick.rs`. En résumé : 10 bandes fines (deux par bande large : sub,
90–200 Hz, 200 Hz–1 kHz, 1–4 kHz, aigus), attaques candidates sur les pics de montée d'énergie des bandes
larges, jugées 10, 20 et 30 ms après l'attaque par trois petits réseaux (32 neurones) pour le kick et trois
pour la caisse claire (et les claps). Une attaque peut être les deux à la fois. La sensibilité règle le
dernier seuil du kick ; sans kick depuis 4 s, le premier doit être franc. `Analyzer::kicks` et
`Analyzer::caisses` comptent les coups reconnus.

Pourquoi un modèle appris plutôt que des seuils : l'ancien détecteur de kick (énergie sous 90 Hz au-dessus
de sa moyenne lente) était excellent sur la batterie acoustique, mais en musique électronique l'attaque du
kick est dans les médiums et les aigus, et le sub n'arrive que 50 à 100 ms plus tard, en commençant même par
se creuser (sidechain). Il y était en retard de 50 à 150 ms et confondait la basse et les voix graves (F1
0,18 sur les morceaux de l'utilisateur). La caisse claire d'Essaim était, elle, un détecteur à seuils
(bande à 2 kHz, enveloppes rapide et lente, tenue à 25 ms) : F1 0,36.

### Banc d'essai (mesuré le 2026-11-20)

Coup compté juste s'il tombe entre 20 ms avant et 70 ms après un vrai coup ; F1 = moyenne (harmonique) de
la précision et du rappel. F1 en validation croisée (5 groupes de morceaux, chaque morceau jugé par un
modèle qui ne l'a pas appris) ; délai et faux kicks d'intro mesurés par `kick_bench` (Rust).

| | Avant (5 bandes, 16 neurones ; caisse claire à seuils) | Maintenant (10 bandes, 32 neurones) |
|---|---|---|
| Kick, 107 morceaux de l'utilisateur (électro, dubstep, hardstyle, rap, pop, rock) | F1 0,807, délai médian 30 ms | **F1 0,811**, délai médian 30 ms |
| Kick, MDB Drums (23 morceaux acoustiques, jazz compris) | F1 0,619, délai médian 20 ms | **F1 0,703**, délai médian 20 ms |
| Faux kicks d'intro (avant le premier vrai kick, 107 morceaux) | 68 | 75 |
| Caisse claire, 107 morceaux de l'utilisateur | F1 0,36 (précision 0,29), délai médian 32 ms | **F1 0,63** (précision 0,61), délai médian 30 ms |
| Caisse claire, MDB Drums (notes fantômes comprises) | F1 0,28 | **F1 0,48** |
| Calcul (un cœur) | 0,19 % | 0,35 % |

Ce qui a compté (validation croisée, F1 kick perso / MDB) : 32 neurones au lieu de 16 (0,807 → 0,814 /
0,619 → 0,640), bandes fines (→ 0,812 / 0,674 à 16 neurones), les deux (0,813 / 0,695). Essayé sans gain
net : plus d'époques ; le niveau des 200 ms d'avant l'attaque (+0,02 sur MDB, −0,005 en perso).

Références : MDB Drums (annotations humaines, <https://github.com/CarlSouthall/MDBDrums>) ; pour tout autre
morceau, la batterie séparée par Demucs (`htdemucs`) puis transcrite par ADTOF
(<https://github.com/xavriley/ADTOF-pytorch>), hors temps réel : F1 0,88 face aux annotations de MDB pour le
kick. La caisse claire de référence vient de la même transcription (note 38).

```
python scripts/kick_bench.py mdb <MDB Drums> <données>/mdb                     # annotations humaines
python scripts/kick_bench.py ref <données>/perso --demucs <dossier de musique>  # référence ADTOF (GPU)
python scripts/kick_bench.py caisses <données>/perso --demucs                   # `.caisses` des morceaux déjà préparés
cargo run --release --example kick_bench -- <données>/perso [sensibilité] [délai] [-v]
```

### Réapprendre le modèle

```
python scripts/kick_modele.py variantes <MDB Drums> <données>/mdbaug   # remixages de MDB
python scripts/kick_modele.py synth <données>/synth 200                # boîte à rythmes, étiquettes exactes
python scripts/kick_modele.py valider <données>/perso <données>/mdb --leger <données>/mdbaug <données>/synth@0.25/0.1
python scripts/kick_modele.py apprendre src/kick_modele.rs <données>/perso <données>/mdb --leger <données>/mdbaug <données>/synth@0.25/0.1
```

Les données d'appoint (`--leger`, un quart du poids d'un morceau) apportent du rock acoustique (remixages
de MDB) et les cas que la vraie musique montre peu : kick de sinus pur (808), caisses claires seules,
roulements de montée sans kick. Pour la caisse claire, la boîte à rythmes ne pèse que 0,1 (`@0.25/0.1`) :
à 0,25, sa précision sur la vraie musique baissait (F1 0,645 → 0,621) ; sans elle, une caisse claire de
boîte à rythmes toute simple n'était plus reconnue. Les mesures de `kick_modele.py` (`levels`, `macro`,
`prep`, `features`) sont reproduites à l'identique dans `kick.rs` : les changer des deux côtés, puis
réapprendre. Environnement utilisé : Python 3.11, torch 2.6 (CUDA), demucs, adtof-pytorch, scipy ; ffmpeg.
