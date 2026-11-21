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
| `kick` | (toujours) | Grosse caisse et caisse claire à 15–30 ms de délai (biquads sur 10 bandes, un petit réseau récurrent pour le contexte et de petits réseaux de décision, appris, sans FFT), niveau et hauteur de la basse | Essaim |
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
90–200 Hz, 200 Hz–1 kHz, 1–4 kHz, aigus) suivies tous les 5 ms, attaques candidates sur les pics de montée
d'énergie des bandes larges, jugées 10, 20 et 30 ms après l'attaque par trois petits réseaux (32 neurones)
pour le kick et trois pour la caisse claire (et les claps). Chaque réseau lit la forme de l'attaque **et le
contexte** : l'état d'un petit réseau récurrent (GRU de 48) qui écoute les niveaux des bandes à chaque pas
et retient ce qui précède (rythme, coups récents, nature du son : musique, voix seule, basse tenue). Une
attaque peut être kick et caisse claire à la fois. La sensibilité règle le dernier seuil du kick ; sans kick
depuis 4 s, le premier doit être franc ; après 2 s de silence, le contexte est oublié. `Analyzer::kicks` et
`Analyzer::caisses` comptent les coups reconnus.

Les bandes sous 4 kHz sont calculées sur le son sous-échantillonné (~12 kHz de 1 à 4 kHz, ~6 kHz en
dessous, passe-bas d'ordre 6 avant chaque division) : mêmes niveaux, deux fois moins de filtrage.

Pourquoi un modèle appris plutôt que des seuils : l'ancien détecteur de kick (énergie sous 90 Hz au-dessus
de sa moyenne lente) était excellent sur la batterie acoustique, mais en musique électronique l'attaque du
kick est dans les médiums et les aigus, et le sub n'arrive que 50 à 100 ms plus tard, en commençant même par
se creuser (sidechain). Il y était en retard de 50 à 150 ms et confondait la basse et les voix graves.
Pourquoi un contexte : en 30 ms, l'attaque d'une contrebasse, d'une voix grave ou d'un tom ressemble à un
kick ; ce qui les distingue est ce qu'on a entendu avant.

### Banc d'essai (mesuré le 2026-11-21)

Coup compté juste s'il tombe entre 20 ms avant et 70 ms après un vrai coup ; F1 = moyenne (harmonique) de
la précision et du rappel. Références : annotations humaines pour MDB Drums ; instants exacts pour les
remixages ; pour un morceau quelconque, le **consensus** de deux transcriptions ADTOF (voir « Données ») :
un coup vu par les deux est sûr, un coup vu par une seule est douteux et ne compte ni juste ni faux. Ces
références sont plus strictes que celles de la 0.3.0 (une seule transcription) : les chiffres d'alors
(F1 0,81) ne se comparent pas à ceux-ci, d'où les deux colonnes remesurées.

Modèles livrés, mesurés par `kick_bench` (Rust) sur les mêmes fichiers (F1 kick / F1 caisse claire) :

| | 0.3.0 (10 bandes, réseaux sans contexte) | 0.4.0 (contexte par GRU) |
|---|---|---|
| 107 morceaux de l'utilisateur (électro, dubstep, hardstyle, rap, pop, rock) | 0,87 / 0,74, délai médian 30 ms | **0,92 / 0,83**, délai médian 30 ms (caisse claire 20 ms) |
| 93 morceaux variés (metal, jazz, funk, reggae, drum and bass, rap, techno, hardstyle, pop…) | 0,80 / 0,70 (jamais vus), délai 30 ms | **0,91 / 0,86**, délai 20 ms |
| MDB Drums (23 morceaux acoustiques, annotations humaines) | 0,79 / 0,60, délai 20 ms | **0,86 / 0,75**, délai 20 ms |
| 107 remixages à instants exacts, boîtes à rythmes jamais apprises | 0,76 / 0,54, délai 23 ms | **0,92 / 0,74**, délai 21 ms |
| Faux coups par minute sur 59 sons sans batterie (voix, piano, orgue, cordes, contrebasse, ASMR ; 214 min) | 9,9 / 8,4 | **0,5 / 0,5** |
| Faux kicks d'intro (avant le premier vrai kick, 107 morceaux) | 68 | 51 |
| Calcul (un cœur, mesuré en alternance sur MDB) | 0,31 % | **0,26 %** |

Sauf les remixages d'essai, la 0.4.0 a appris sur ces morceaux. L'estimation honnête est la validation
croisée (5 groupes de morceaux, chaque morceau jugé par un modèle qui ne l'a pas appris, ni lui ni ses
remixages), F1 kick / F1 caisse claire :

| | Réseaux sans contexte (0.3.0), mêmes références | Contexte par GRU (0.4.0) |
|---|---|---|
| 107 morceaux de l'utilisateur | 0,835 / 0,675 | **0,891 / 0,777** |
| 93 morceaux variés | 0,789 / 0,703 | **0,878 / 0,800** |
| MDB Drums | 0,638 / 0,393 | **0,787 / 0,561** |
| Remixages d'essai | 0,723 / 0,522 | **0,895 / 0,712** |
| Faux coups par minute, sons sans batterie | 5,9 / 13,4 | **2,3 / 0,7** |

Cette validation date d'avant le dernier complément des données de synthèse (kicks longs, extraits sans
caisse claire, ajoutés pour le test `kicks_seuls_ne_donnent_aucune_caisse_claire`), qui ne change pas les
scores du banc Rust à 0,01 près ; elle n'a pas été refaite depuis (25 minutes de calcul).

Ce qui a compté (validation croisée) :

- le contexte (GRU) à lui seul, à données égales : kick +0,01 sur les morceaux de l'utilisateur, +0,05 sur
  MDB, +0,10 sur les remixages d'essai ; caisse claire +0,04 à +0,07, et deux fois moins de fausses caisses
  claires sur les sons sans batterie ;
- les remixages à instants exacts et les sons sans batterie dans l'apprentissage : encore +0,05 sur les
  remixages d'essai, faux coups sur les sons sans batterie divisés par 2 (kick) et 5 (caisse claire) ;
- les références de consensus : les coups douteux n'apprennent plus rien de faux, et ne faussent plus la
  mesure ;
- un apprentissage dix fois plus long (40 → 400 passes) avec un GRU de 48 au lieu de 32 : kick +0,05,
  caisse claire +0,06.

Essayé sans gain : GRU de 64, réseaux de décision de 48, 800 passes, d'autres entrées pour le GRU, attendre
50 ms au lieu de 30, une référence de kick par séparation de batterie pièce par pièce (DrumSep : pas
meilleure que le consensus). Le banc de filtres sous-échantillonné donne les mêmes scores que le calcul à
pleine cadence.

Limites connues : le kick effleuré du jazz acoustique (bebop, cool jazz : presque aucun kick trouvé sur
MDB), les notes fantômes de la batterie acoustique (rappel de la caisse claire 0,68 sur MDB), les caisses
claires des boîtes à rythmes jamais entendues (0,74 sur les remixages d'essai), et un charleston fort
frappé exactement avec un kick long (808), parfois compté comme caisse claire.

### Données

Hors du dépôt (droits des morceaux) ; tout se refait avec les scripts.

- **Référence d'un morceau quelconque** (`kick_bench.py ref --demucs`) : ADTOF
  (<https://github.com/xavriley/ADTOF-pytorch>) transcrit la batterie séparée par Demucs (`htdemucs`) et, à
  part, le mélange complet. Vu par les deux : coup sûr (`.kicks`, `.caisses` ; précision 0,92 face aux
  annotations de MDB). Vu par une seule : douteux (`.kicks_doute`, `.caisses_doute`), ignoré à
  l'apprentissage comme à l'évaluation. `--pistes` garde les 4 pistes séparées pour les remixages.
- **MDB Drums** (<https://github.com/CarlSouthall/MDBDrums>) : annotations humaines, et 4 remixages de
  chaque morceau à partir de ses pistes (`kick_modele.py variantes`).
- **Remixages à instants exacts** (`kick_remix.py`) : l'accompagnement réel d'un morceau (basse + voix +
  reste, sans la batterie) reçoit une batterie refaite avec des sons isolés (boîtes à rythmes de
  <https://github.com/geikha/tidal-drum-machines>, banque <https://github.com/tidalcycles/Dirt-Samples>,
  kicks de synthèse) posés aux instants de la batterie d'origine. Deux versions par morceau, plus une sans
  batterie du tout (aucun coup). Dix boîtes à rythmes sont tenues à l'écart pour l'évaluation (`--essai`).
- **Sons sans batterie** : voix parlée et chantée, piano, orgue, cordes, guitare, contrebasse, didgeridoo,
  ASMR, nappes (fichiers `.kicks` et `.caisses` vides) : tout coup y est faux.
- **Boîte à rythmes de synthèse** (`kick_modele.py synth`) : les cas que la vraie musique montre peu ou
  étiquette mal : kick de sinus pur, kick long qui ondule (808), caisses claires seules, roulements de
  montée sans kick, extraits sans aucune caisse claire.

```
python scripts/kick_bench.py mdb <MDB Drums> <données>/mdb                               # annotations humaines
python scripts/kick_bench.py ref <données>/perso --demucs --pistes <pistes>/perso <dossier de musique>   # consensus ADTOF (GPU)
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remix     # + <données>/remixsb (sans batterie)
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remixtest 1 --essai
cargo run --release --example kick_bench -- <données>/perso [sensibilité] [délai] [-v]
```

### Réapprendre le modèle

```
python scripts/kick_modele.py variantes <MDB Drums> <données>/mdbaug   # remixages de MDB
python scripts/kick_modele.py synth <données>/synth 300                # boîte à rythmes, étiquettes exactes
cd <données>
python <dépôt>/scripts/kick_modele.py valider perso varie mdb remix@.5 remixv@.5 remixsb@.5 remixvsb@.5 neg@.5 --leger mdbaug synth@0.25/0.1 --essai remixtest neg
python <dépôt>/scripts/kick_modele.py apprendre <dépôt>/src/kick_modele.rs perso varie mdb remix@.5 remixv@.5 remixsb@.5 remixvsb@.5 neg@.5 --leger mdbaug synth@0.25/0.1
python <dépôt>/scripts/kick_modele.py detecter <dépôt>/src/kick_modele.rs <morceau.f32>   # doit donner les coups de :
cargo run --release --example kick_mesures -- <morceau.f32>
```

`perso` et `varie` sont les vrais morceaux (poids 1), `remix*` leurs remixages (0,5), `neg` les sons sans
batterie (0,5) ; les données d'appoint (`--leger`, 0,25) apportent le rock acoustique (remixages de MDB) et
la boîte à rythmes de synthèse, qui ne pèse que 0,1 pour la caisse claire (`@0.25/0.1`). Un apprentissage
dure 5 minutes sur une carte graphique (RTX 4070), une validation croisée 25 ; il y faut plusieurs Go de
mémoire vive et 4 Go de mémoire graphique : ne pas en lancer deux à la fois. Les mesures de `kick_modele.py`
(`levels`, `macro`, `prep`, `candidates`, `features`, `frame_inputs`, `decisions`) sont reproduites à
l'identique dans `kick.rs` : les changer des deux côtés, puis réapprendre ; `detecter` et `kick_mesures`
doivent donner les mêmes coups aux mêmes instants (vérifié sur 5 morceaux : aucun écart). Les niveaux par
bande sont gardés dans `<dossier>/.mesures/` (à effacer si `levels` change). Environnement utilisé :
Python 3.11, torch 2.6 (CUDA), demucs, adtof-pytorch, scipy, soundfile ; ffmpeg.

### Pistes suivantes (non faites, notées le 2026-11-21)

Par ordre de gain attendu. Chacune se juge comme les précédentes : `kick_modele.py valider`, puis tests
synthétiques et `kick_bench`, chiffres ici.

1. **Caisse claire des boîtes à rythmes et notes fantômes** : c'est le score le plus bas (0,74 sur les
   remixages d'essai, rappel 0,68 sur MDB). Pistes : plus de kits dans les remixages, changements de hauteur
   et de tempo des sons isolés, cible élargie aux candidats voisins d'un vrai coup.
2. **Toms et charleston** : ADTOF les transcrit aussi (notes 47 et 42) ; un détecteur de plus coûterait
   trois petits réseaux, le contexte étant déjà calculé.
3. **Démarrage** : à 90 BPM, le test d'Essaim `tempo_trouve_sur_des_kicks_reguliers` montre que les 2
   premiers kicks d'un flux sont manqués, à cause de la règle « sans kick depuis 4 s, le premier doit être
   franc ». Mesurer ce que coûterait un relèvement plus court ou plus doux en faux kicks d'intro (51 sur
   107 morceaux aujourd'hui).
4. **Tempo (`pulsation`)** sur l'activité continue de la batterie (sortie des réseaux à chaque attaque)
   plutôt que sur les seuls instants des kicks, à la manière de BeatNet+ (Heydari 2024, filtre particulaire
   en ligne) : plus robuste quand le kick manque (breaks, rap sans kick régulier).
5. **Niveau et hauteur de la basse** : inchangés depuis la 0.2.0, faute de référence objective. La piste
   `bass` de Demucs (gardée par `--pistes`) en fournirait une : niveau de la basse seule, hauteur par pYIN
   hors ligne.

Lectures : Böck et Widmer 2013 (détection d'attaques en ligne), Vogl et al. 2017 (transcription de
batterie), Zehren et al. (ADTOF), Weber et al. (STAR Drums : batterie refaite sur accompagnement réel ;
astuces d'entraînement de 2024), Heydari et al. 2024 (BeatNet+).
