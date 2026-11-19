# audio-commun

*Windows sound capture (WASAPI loopback) and music analysis (spectrum, onsets, tempo, low-latency
kick detection), shared by [Horizon](https://github.com/KingsRequin/horizon),
[Essaim](https://github.com/KingsRequin/essaim) and Prisme. Documentation in French below.
License: [MIT](LICENSE).*

Bibliothèque Rust partagée par **Prisme**, **Horizon** et **Essaim** :
capture du son de Windows et analyses musicales. Avant elle, chaque application en avait sa copie (trois
captures quasi identiques, deux analyses spectrales qui divergeaient).

| Module | Feature | Contenu | Utilisé par |
|---|---|---|---|
| `capture` | `capture` | Loopback WASAPI de la sortie par défaut ou d'une sortie choisie, mode événement, mono, MMCSS « Pro Audio », relance si la sortie change, silence injecté | les trois |
| `spectre` | `spectre` | FFT : bandes mel, graves/médiums/aigus, attaques kick/snare/hihat, tempo + PLL, intensité, drops, tonalité ; `spectre::start` = capture + analyse à cadence fixe | Prisme, Horizon |
| `kick` | (toujours) | Grosse caisse à 15–30 ms de délai (biquads sur 5 bandes + petit réseau appris, sans FFT), niveau et hauteur de la basse | Essaim |
| `pulsation` | (toujours) | Tempo (BPM) et phase du temps d'après les seuls instants des kicks, sans FFT ; relais pendant un passage sans kick | Essaim |

Chaque application garde **sa propre capture** : faire passer l'analyse d'un processus à l'autre ajouterait
du délai (Essaim vise 10–20 ms) et ferait dépendre une application du lancement d'une autre, pour un gain
négligeable (une capture loopback lit le mélange déjà calculé par Windows).

## Utilisation

```toml
# Cargo.toml de l'application
audio-commun = { git = "https://github.com/KingsRequin/audio-commun", tag = "v0.2.0" }  # tout
audio-commun = { git = "https://github.com/KingsRequin/audio-commun", tag = "v0.2.0", default-features = false, features = ["capture"] }  # Essaim
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

## Kick

Détails de la méthode en tête de `src/kick.rs`. En résumé : 5 bandes (sub, 90–200 Hz, 200 Hz–1 kHz,
1–4 kHz, aigus), attaques candidates sur les pics de montée d'énergie, jugées par trois petits réseaux
(16 neurones) 10, 20 et 30 ms après l'attaque ; la sensibilité règle le dernier seuil ; sans kick depuis
4 s, le premier doit être franc.

Pourquoi un modèle appris plutôt que des seuils : l'ancien détecteur (énergie sous 90 Hz au-dessus de sa
moyenne lente) était excellent sur la batterie acoustique, mais en musique électronique l'attaque du kick
est dans les médiums et les aigus, et le sub n'arrive que 50 à 100 ms plus tard, en commençant même par se
creuser (sidechain). Il y était en retard de 50 à 150 ms et confondait la basse et les voix graves.

### Banc d'essai (mesuré le 2026-11-17)

Kick compté juste s'il tombe entre 20 ms avant et 70 ms après un vrai kick ; F1 = moyenne de la précision
et du rappel.

| | Ancien détecteur | Nouveau (validation croisée) |
|---|---|---|
| 107 morceaux de l'utilisateur (électro, dubstep, hardstyle, rap, pop, rock) | F1 0,18, délai médian 60 ms | **F1 0,81**, délai médian 25–30 ms |
| MDB Drums, rock et pop acoustiques (15 morceaux) | F1 0,93 | F1 0,84 |
| MDB Drums, jazz (8 morceaux) | faible | faible (balais, contrebasse) |
| Faux kicks d'intro (avant le premier vrai kick, 107 morceaux) | 206 | 68 |

Références : MDB Drums (annotations humaines, <https://github.com/CarlSouthall/MDBDrums>) ; pour tout autre
morceau, la batterie séparée par Demucs (`htdemucs`) puis transcrite par ADTOF
(<https://github.com/xavriley/ADTOF-pytorch>), hors temps réel : F1 0,88 face aux annotations de MDB.

```
python scripts/kick_bench.py mdb <MDB Drums> <données>/mdb                     # annotations humaines
python scripts/kick_bench.py ref <données>/perso --demucs <dossier de musique>  # référence ADTOF (GPU)
cargo run --release --example kick_bench -- <données>/perso [sensibilité] [délai] [-v]
```

### Réapprendre le modèle

```
python scripts/kick_modele.py variantes <MDB Drums> <données>/mdbaug   # remixages de MDB
python scripts/kick_modele.py synth <données>/synth 200                # boîte à rythmes, étiquettes exactes
python scripts/kick_modele.py valider <données>/perso <données>/mdb --leger <données>/mdbaug <données>/synth
python scripts/kick_modele.py apprendre src/kick_modele.rs <données>/perso <données>/mdb --leger <données>/mdbaug <données>/synth
```

Les données d'appoint (`--leger`, un quart du poids d'un morceau) apportent du rock acoustique (remixages
de MDB) et les cas que la vraie musique montre peu : kick de sinus pur (808), caisses claires seules,
roulements de montée sans kick. Les mesures de `kick_modele.py` (`levels`, `prep`, `features`) sont
reproduites à l'identique dans `kick.rs` : les changer des deux côtés, puis réapprendre. Environnement
utilisé : Python 3.11, torch 2.6 (CUDA), demucs, adtof-pytorch, scipy ; ffmpeg.
