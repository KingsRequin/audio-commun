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
| `capture` | `capture` | Loopback WASAPI de la sortie par défaut, d'une sortie choisie ou de certaines applications (0.7.0), mode événement, mono, MMCSS « Pro Audio », relance si la sortie change, silence injecté | les trois |
| `spectre` | `spectre` | FFT : bandes mel, graves/médiums/aigus, charleston, tempo + PLL, intensité, drops, tonalité ; kick et caisse claire repris de `kick` (0.6.0) ; `spectre::start` = capture + analyse à cadence fixe | Prisme, Horizon |
| `kick` | (toujours) | Grosse caisse et caisse claire à 15–30 ms de délai (biquads sur 10 bandes, un petit réseau récurrent pour le contexte et de petits réseaux de décision, appris, sans FFT), niveau et hauteur de la basse | Essaim, et `spectre` |
| `pulsation` | (toujours) | Tempo (BPM) et phase du temps d'après les seuls instants des kicks, sans FFT ; relais pendant un passage sans kick | Essaim |

Chaque application garde **sa propre capture** : faire passer l'analyse d'un processus à l'autre ajouterait
du délai (Essaim vise 10–20 ms) et ferait dépendre une application du lancement d'une autre, pour un gain
négligeable (une capture loopback lit le mélange déjà calculé par Windows).

## Utilisation

```toml
# Cargo.toml de l'application
audio-commun = { git = "https://github.com/KingsRequin/audio-commun", tag = "v0.7.0" }  # tout
audio-commun = { git = "https://github.com/KingsRequin/audio-commun", tag = "v0.7.0", default-features = false, features = ["capture"] }  # Essaim
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

Changer de sortie n'est pas une panne : un changement de sortie par défaut (signalé aussitôt par Windows,
`register_notification_callback`, la vérification des 2 s restant en secours) ou le départ et le retour de la
sortie choisie rouvrent le flux tout de suite, sans pause ni `lost()`. `lost()` ne sert qu'aux erreurs. Sans
paquet, le client est interrogé toutes les 250 ms : une sortie reconfigurée (format, son spatial, mode exclusif,
pilote relancé) invalide le client et la capture est rouverte. `Capture::drop` attend le fil au plus 300 ms,
puis le détache (une ligne de journal) : l'appelant ne reste jamais bloqué. Un fil détaché peut encore appeler
son `Sink` un moment : si une nouvelle capture écrit au même endroit, au `Sink` de reconnaître qu'il n'est plus
le bon (Essaim lui donne un numéro de génération).

Une panique dans le fil de capture (dans le `Sink` ou la capture) est rattrapée : une ligne de journal,
`lost()`, puis le fil se termine. `Capture::actif()` passe alors à `false` : à l'application de relancer la
capture.

## Son de certaines applications (`capture`, 0.7.0)

`capture::start_ecoute(nom, ecoute, sink)` lance la capture d'une `Ecoute` : `Ecoute::Sortie(sortie)` (tout le
mélange, comme `start_sur`, qui passe désormais par là sans changer de comportement) ou
`Ecoute::Applis(vec!["spotify.exe".into(), "chrome.exe".into()])`, le son de ces applications seulement, quelle
que soit la sortie où elles jouent (capture « process loopback » de Windows 10 2004 et suivants). Mêmes appels
au `Sink` (`open`, `samples` en mono, `stereo`, `silence`, `tick`, `lost`, et `repli`, qui ne fait rien par
défaut), même MMCSS.
`Ecoute::depuis_reglages(sortie_id, &applis)` donne `Applis` si la liste contient un nom, sinon toute la sortie
(`Sortie::depuis_id`) : aucune appli cochée = tout le son.

- **Noms** : nom d'exécutable, casse indifférente, `.exe` facultatif. Pour chacun, la capture vise la racine de
  l'arbre de processus de l'appli (le processus de ce nom dont le parent n'a pas le même nom), enfants compris :
  un navigateur joue le son depuis un processus enfant. Deux instances indépendantes = deux racines.
- **Format** : f32 stéréo à 48 000 Hz imposé (`open(48000)`), converti par Windows ; ce mode ne donne ni format de
  mélange ni période.
- **Mélange** : un client par racine, sommés. Chacun remplit sa file ; la somme n'avance que jusqu'à la plus courte
  des files des clients qui jouent (paquet depuis 30 ms au plus). Un client muet compte comme du zéro et ne
  retient pas les autres ; un client en retard ne retient pas plus de 100 ms.
- **Suivi** : la liste des processus est revérifiée toutes les 2 s (appli lancée après coup : ajoutée ; fermée :
  retirée, une ligne de journal chaque fois). Appli absente, ou rien ne joue : du silence est remis, comme pour une
  sortie entière. Les ouvertures prennent 10 ms au plus par tour de boucle (la suite au tour suivant), pour que le
  son des applis déjà ouvertes continue de passer ; au-delà de 64 clients, les suivants sont lus à chaque réveil
  sans être attendus.
- **Repli** : au lancement, un essai de capture sur le processus appelant dit si Windows sait faire. S'il ne
  connaît pas cette capture (Windows trop ancien : `E_NOTIMPL`, `E_NOINTERFACE`, classe non enregistrée, non pris
  en charge) : une ligne de journal, puis toute la sortie par défaut jusqu'à l'arrêt de la capture. Tout autre
  refus (service audio pas encore prêt, relancé) : la sortie par défaut, et un nouvel essai toutes les 30 s dans
  un fil à part ; le flux de la sortie par défaut n'est quitté qu'une fois l'essai réussi (jusqu'à la 0.7.2, il
  était fermé et rouvert toutes les 30 s, et `open` remettait l'analyse à zéro). Le `Sink` est prévenu par
  `repli(true)` à l'entrée du repli et par `repli(false)` dès que la capture par application fonctionne, au
  premier essai réussi même sans repli (l'état part toujours d'une valeur sûre). Un processus précis que Windows
  refuse (protégé) : une ligne de journal, il est réessayé après 30 s, 60 s, 120 s… (10 min au plus), les autres
  sont écoutés.

`capture::applis_sonores()` liste les applications qui ont en ce moment une session audio sur une sortie active,
`(exécutable, nom affichable)` : sans les sons système (pid 0, programmes de `System32` et `SysWOW64`, processus d'un autre compte
dont le chemin est illisible : « Écouter ce périphérique » joue un micro depuis un `svchost.exe` de service) ni le processus appelant, une fois par exécutable,
triées par nom affichable (description de l'exécutable, « Google Chrome », à défaut nom de la session, à défaut
exécutable sans `.exe`). Appelable depuis n'importe quel fil, comme `sorties()`. Une appli garde sa session tant
qu'elle a un flux ouvert, même en pause : la liste dit « peut jouer », pas « joue ». `capture::applis_sonores_detail()`
(0.7.1) donne la même liste en `AppliSonore { exe, nom, joue }` : `joue` est vrai quand une de ses sessions est active
et que son niveau crête dépasse −60 dBFS.

Essai réel : `cargo test --release applis_reelles -- --ignored --nocapture` (liste les applis sonores, capture
600 ms d'une appli absente, de la première appli listée et de toutes ensemble).

## Niveaux stéréo (`capture`)

Le son est remis au `Sink` en mono, mais `Sink::stereo(gauche, droite)` reçoit après chaque paquet le niveau
efficace (RMS) de chaque canal : de quel côté joue le son. Méthode facultative (vide par défaut) ; Essaim
s'en sert pour pousser l'essaim vers le côté qui joue le plus fort.

## Son anormal (0.7.4)

Une appli peut émettre un échantillon NaN, infini ou démesuré. Avant la 0.7.4, un seul suffisait à arrêter pour
toujours le détecteur appris de `kick` (filtres figés à NaN), et un infini laissait le volume de `spectre` à NaN.
Chaque échantillon passe maintenant par `echantillon_sain` (0 s'il n'est pas fini, borné à ±`ECHANTILLON_MAX`,
+18 dB) : à la lecture de la capture, donc avant tout `Sink`, et à l'entrée de `kick::Analyzer::push` et de
`spectre::Analyzer::push` pour un son venu d'ailleurs. Sans effet sur un son normal : un échantillon fini sous la borne passe tel quel.
`spectre::start` publie en plus comme du silence une image d'analyse non finie et relance l'analyse si cela dure
une seconde (`AudioFeatures::all_finite`) ; `AudioFeatures::quiet` remet aussi à 0 un timbre ou une tonalité non
finis.

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

## Kick, caisse claire et temps de `spectre` (0.6.0)

Jusqu'à la 0.5.0, `spectre` trouvait la grosse caisse et la caisse claire par flux spectral (hausse des
graves de 40 à 120 Hz, des médiums de 1,5 à 5 kHz, au-dessus d'un seuil adaptatif). Depuis la 0.6.0, il fait
tourner le détecteur appris de `kick` sur les mêmes échantillons : `kicks`, `snares`, leurs enveloppes
(`kick`, `snare`) et `kick_hits` en viennent. La force d'un coup (0,4 à 1, valeur à laquelle saute
l'enveloppe) est l'assurance du réseau qui l'a reconnu (`kick::Analyzer::kick_force`, `caisse_force`) : un
coup douteux s'allume moins qu'un coup franc. Le charleston reste au flux spectral. `Options::kick` n'a
plus d'effet (le détecteur tourne toujours).

Mesuré le 2026-11-23 (`kick_bench --spectre` juge les compteurs de `spectre`), F1 kick / caisse claire, puis
faux coups par minute :

| | Flux spectral (0.5.0) | Détecteur appris (0.6.0) |
|---|---|---|
| 107 morceaux de l'auteur | 0,55 / 0,35 ; 67 / 104 | 0,91 / 0,82 ; 6,7 / 6,5 |
| 93 morceaux variés | 0,64 / 0,41 ; 63 / 99 | 0,89 / 0,85 ; 9,9 / 8,5 |
| MDB Drums | 0,62 / 0,44 ; 74 / 91 | 0,82 / 0,65 ; 10 / 14 |
| 277 morceaux de 42 styles | 0,64 / 0,40 ; 61 / 102 | 0,87 / 0,80 ; 8,6 / 9,2 |
| 59 sons sans batterie (voix, piano, ambiances) | 120 / 136 faux par minute | 1,0 / 0,5 |

Le suivi du tempo (`spectre/tempo.rs`) garde son signal d'attaques (flux global + montée des graves), plus
une impulsion à chaque kick reconnu (3 × sa force) ; ces kicks recalent aussi la phase, et font office de
temps quand aucun tempo n'est annoncé. `tempo_bench` le mesure sans temps annotés : la référence est la
grille que `pulsation` tire des kicks de référence, là où ils sont réguliers. Précision / rappel des temps
(de 50 ms avant à 90 ms après un temps de la grille), part du temps où le tempo annoncé est le bon, et temps
émis par minute quand aucun tempo n'est annoncé :

| | 0.5.0 | 0.6.0 |
|---|---|---|
| 107 morceaux de l'auteur (106 min de grille) | 0,66 / 0,62 ; 73 % ; 108 | 0,83 / 0,76 ; 77 % ; 32 |
| 93 morceaux variés (150 min) | 0,68 / 0,67 ; 81 % ; 138 | 0,85 / 0,82 ; 84 % ; 42 |
| 277 morceaux de 42 styles (205 min) | 0,74 / 0,77 ; 86 % ; 124 | 0,87 / 0,88 ; 86 % ; 38 |
| 59 sons sans batterie | 122 temps par minute | 1,1 |

Le décalage médian des temps passe de 48 à 37 ms. Étapes : les kicks appris à la place du flux (recalage de
la phase, temps sans tempo) donnent 0,68 / 0,62 sur les morceaux de l'auteur, l'impulsion dans le signal
d'attaques le reste (poids 1 : 0,77 / 0,70 ; 3 : 0,83 / 0,76 ; 6 et 12 : pareil, mais le tempo n'est plus
annoncé dans les passages sans kick régulier). L'impulsion ne remplace pas la montée des graves : poids 1
sans elle, 0,68 / 0,62.
Par style (32 styles ont au moins 30 s de grille) : la hausse est forte là où le kick porte le temps (house
0,62 → 0,89, techno 0,65 → 0,96, trance 0,63 → 0,86, indus et eurodance 0,57 → 0,97, pop latine 0,73 → 0,95,
reggae 0,26 → 0,55) ; cinq styles perdent un peu, sur 2 à 4 morceaux chacun (bass music 0,75 → 0,71 de
précision, swing 0,71 → 0,68, et 0,03 à 0,07 de rappel en folk, rock classique et trip-hop, où le tempo est
annoncé moins souvent).

Coût : l'analyse passe de 0,17 % à 0,40 % d'un cœur (Prisme ; Horizon faisait déjà tourner les deux).

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
depuis 4 s, le premier doit être franc ; après 2 s de silence, le contexte est oublié. Tout est mesuré par
rapport au niveau récent du morceau, seuil de silence compris : le volume d'écoute ne change rien (0.5.0).
`Analyzer::kicks` et `Analyzer::caisses` comptent les coups reconnus.

Le modèle est appris sur des morceaux de 42 styles et se juge style par style (« Par style ») : il sert à
tout le monde, pas aux goûts de son auteur.

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
un coup vu par les deux est sûr, un coup vu par une seule est douteux et ne compte ni juste ni faux.

Modèles livrés, mesurés par `kick_bench` (Rust) sur les mêmes fichiers (F1 kick / F1 caisse claire) :

| | 0.4.0 | 0.5.0 (42 styles, kits acoustiques) |
|---|---|---|
| 107 morceaux de l'auteur (électro, dubstep, hardstyle, rap, pop, rock) | 0,92 / 0,83 | 0,91 / 0,82 |
| 93 morceaux variés (metal, jazz, funk, reggae, drum and bass, rap, techno, hardstyle, pop…) | 0,91 / 0,86 | 0,89 / 0,85 |
| MDB Drums (23 morceaux acoustiques, annotations humaines) | 0,86 / 0,75 | 0,82 / 0,65 |
| 277 morceaux de 42 styles | 0,83 / 0,73 (jamais appris) | **0,87 / 0,80** |
| 107 remixages à instants exacts, boîtes à rythmes jamais apprises | 0,92 / 0,74 | 0,91 / 0,73 |
| 477 remixages à instants exacts, kits acoustiques jamais appris | 0,79 / 0,63 | **0,83 / 0,69** |
| 477 remixages à instants exacts, packs de production actuels jamais appris (mesuré le 2026-11-23) | | 0,79 / 0,62 |
| Faux coups par minute, 59 sons sans batterie (voix, piano, orgue, cordes, contrebasse, ASMR ; 214 min) | 0,6 / 0,6 | 1,0 / 0,5 |
| Faux coups par minute, 31 autres sons sans batterie (chœurs, clavecin, kora, pluie, orage, train ; 96 min) | 11,6 / 7,5 (jamais appris) | **5,7 / 1,7** |
| Faux kicks d'intro (avant le premier vrai kick, 107 morceaux) | 53 | **37** |
| Délai médian | 20 à 30 ms | 20 à 30 ms |
| Calcul (un cœur) | 0,26 % | 0,26 % |

Colonne 0.4.0 : ses poids, remesurés avec le seuil de silence de la 0.5.0 (voir « Volume d'écoute »). Sauf
mention, chaque modèle a appris sur les morceaux de sa ligne : ces chiffres disent ce qu'il a retenu, pas ce
qu'il vaut sur un morceau inconnu. La 0.5.0 y est un peu plus basse que la 0.4.0 parce que le même petit
réseau couvre trois fois plus de musique et retient moins chaque morceau par cœur. L'estimation honnête est
la validation croisée (5 groupes de morceaux, chaque morceau jugé par un modèle qui ne l'a pas appris, ni lui
ni ses remixages), F1 kick / F1 caisse claire :

| | Recette 0.4.0 | Recette 0.5.0 |
|---|---|---|
| 107 morceaux de l'auteur | 0,876 / 0,777 | **0,887** / 0,776 |
| 93 morceaux variés | 0,872 / 0,802 | **0,881 / 0,822** |
| MDB Drums | 0,751 / 0,592 | **0,783** / 0,594 |
| 277 morceaux de 42 styles | 0,834 / 0,731 (modèle livré, qui ne les a pas appris) | **0,843 / 0,758** |
| Remixages d'essai, boîtes à rythmes | 0,902 / 0,714 | 0,906 / **0,729** |
| Remixages d'essai, kits acoustiques (les 200 communs aux deux validations) | 0,748 / 0,613 | **0,802 / 0,672** |
| Faux coups par minute, 59 sons sans batterie | 2,5 / 0,7 | 4,1 / 0,4 |
| Faux coups par minute, 31 autres sons sans batterie | | 11,0 / 3,4 |

Sans contexte (réseaux de la 0.3.0, mêmes références) : 0,835 / 0,675 sur les morceaux de l'auteur,
0,789 / 0,703 sur les variés, 0,638 / 0,393 sur MDB. Sur les 59 sons sans batterie, 3,0 des 4,1 faux kicks
par minute viennent d'un seul son (des tapotements d'ASMR, que la transcription de référence prend elle
aussi pour des kicks).

#### Par style

Les 277 morceaux de 42 styles, tous jugés par un modèle qui ne les a pas appris : « avant », le modèle
livré en 0.4.0 ; « après », la recette de la 0.5.0 en validation croisée.

| Style | Morceaux | Kick avant | Kick après | Caisse claire avant | Caisse claire après |
|---|---|---|---|---|---|
| Afrobeat, afrobeats | 7 | 0,79 | 0,82 | 0,65 | 0,63 |
| Batterie seule (solos) | 6 | 0,60 | 0,55 | 0,55 | 0,58 |
| Blues | 6 | 0,91 | 0,93 | 0,77 | 0,80 |
| Bollywood, musique indienne | 6 | 0,75 | 0,74 | 0,72 | 0,74 |
| Breakbeat, UK garage, jungle | 7 | 0,79 | 0,79 | 0,72 | 0,76 |
| Celtique, flamenco | 6 | 0,78 | 0,77 | 0,79 | 0,84 |
| Country | 7 | 0,88 | 0,91 | 0,84 | 0,86 |
| Disco | 6 | 0,95 | 0,95 | 0,81 | 0,84 |
| Dubstep, bass music | 6 | 0,92 | 0,90 | 0,83 | 0,82 |
| Folk | 6 | 0,85 | 0,87 | 0,57 | 0,62 |
| Funk | 7 | 0,81 | 0,82 | 0,82 | 0,85 |
| Gospel | 4 | 0,87 | 0,86 | 0,90 | 0,88 |
| House | 7 | 0,90 | 0,93 | 0,75 | 0,74 |
| Indus, eurodance | 6 | 0,85 | 0,88 | 0,76 | 0,77 |
| Jazz : ballades | 6 | 0,60 | 0,49 | 0,39 | 0,51 |
| Jazz : bebop, hard bop | 7 | 0,46 | 0,47 | 0,52 | 0,61 |
| Jazz : swing, big band | 6 | 0,80 | 0,84 | 0,45 | 0,56 |
| Jazz moderne, fusion | 7 | 0,78 | 0,82 | 0,69 | 0,71 |
| K-pop, J-pop, mandopop | 8 | 0,92 | 0,91 | 0,74 | 0,76 |
| Metal | 9 | 0,88 | 0,88 | 0,85 | 0,88 |
| Musique brésilienne | 7 | 0,85 | 0,83 | 0,64 | 0,66 |
| Musique de film et de jeu | 6 | 0,73 | 0,79 | 0,62 | 0,62 |
| Orient, Balkans | 6 | 0,94 | 0,94 | 0,78 | 0,83 |
| Percussions seules | 6 | 0,50 | 0,62 | 0,48 | 0,55 |
| Pop latine, reggaeton | 8 | 0,77 | 0,81 | 0,59 | 0,61 |
| Pop rock | 7 | 0,86 | 0,87 | 0,76 | 0,79 |
| Punk | 6 | 0,92 | 0,93 | 0,87 | 0,89 |
| R&B | 8 | 0,91 | 0,91 | 0,75 | 0,77 |
| Rap boom bap | 7 | 0,86 | 0,88 | 0,82 | 0,84 |
| Reggae, dub, ska, dancehall | 8 | 0,85 | 0,84 | 0,71 | 0,72 |
| Rock classique | 8 | 0,77 | 0,81 | 0,82 | 0,84 |
| Rock indépendant | 7 | 0,93 | 0,93 | 0,86 | 0,88 |
| Rock progressif | 6 | 0,87 | 0,88 | 0,88 | 0,90 |
| Salsa, musique cubaine | 6 | 0,55 | 0,59 | 0,23 | 0,30 |
| Soul | 7 | 0,80 | 0,84 | 0,78 | 0,84 |
| Synthpop, new wave | 6 | 0,94 | 0,94 | 0,80 | 0,84 |
| Techno | 6 | 0,77 | 0,68 | 0,53 | 0,48 |
| Trance | 6 | 0,89 | 0,88 | 0,79 | 0,76 |
| Trap, drill | 7 | 0,95 | 0,96 | 0,66 | 0,68 |
| Trip-hop, lo-fi | 6 | 0,76 | 0,80 | 0,78 | 0,80 |
| Variété française | 7 | 0,83 | 0,84 | 0,68 | 0,72 |
| Électronique expérimentale | 6 | 0,80 | 0,80 | 0,75 | 0,79 |
| **Ensemble** | 277 | 0,83 | 0,84 | 0,73 | 0,76 |

Lecture : la 0.4.0 n'était pas réglée sur quelques styles (0,83 sur des styles jamais appris, pour 0,88 en
validation croisée sur ceux de l'auteur), et les nouvelles données rapportent surtout à la caisse claire
(+0,03) et à la batterie acoustique. Avec 6 à 9 morceaux par style, un écart de 0,05 sur une ligne est du
bruit : la baisse de la techno tient à deux morceaux (Kraftwerk, Jeff Mills) et ne se retrouve pas sur les
5 morceaux de techno du jeu varié (0,91 → 0,89 d'une validation à l'autre). Les styles faibles le restent,
et la référence y est elle-même fragile (une transcription de batterie devant des congas, des timbales ou
des balais) : jazz acoustique, salsa, percussions seules, solos de batterie.

#### Volume d'écoute

Le son capté est celui que Windows envoie à la sortie, après le curseur de volume de l'application : un
lecteur réglé très bas donne un signal 30 à 50 dB plus faible. Jusqu'à la 0.4.0, le seuil de silence était
fixe (-60 dB) : en dessous, plus rien n'était jugé. Il suit maintenant le niveau récent (52 dB sous le pic,
entre -100 et -60 dB). 16 morceaux variés rejoués à cinq volumes (F1 kick / F1 caisse claire) :

| Volume | 0 dB | -12 dB | -24 dB | -36 dB | -48 dB |
|---|---|---|---|---|---|
| 0.4.0 (seuil fixe) | 0,93 / 0,85 | 0,93 / 0,85 | 0,93 / 0,86 | 0,91 / 0,84 | 0,75 / 0,69 |
| 0.5.0 | 0,92 / 0,84 | 0,92 / 0,84 | 0,92 / 0,84 | 0,91 / 0,84 | 0,90 / 0,84 |

Après une baisse brutale du volume, les références de niveau retombent de 2 dB par seconde : 40 dB sont
rattrapés en 20 s (test `coups_trouves_a_volume_tres_bas`).

#### Ce qui a compté (validation croisée)

- le contexte (GRU) à lui seul, à données égales : kick +0,01 sur les morceaux de l'auteur, +0,05 sur
  MDB, +0,10 sur les remixages d'essai ; caisse claire +0,04 à +0,07, et deux fois moins de fausses caisses
  claires sur les sons sans batterie ;
- les remixages à instants exacts et les sons sans batterie dans l'apprentissage : encore +0,05 sur les
  remixages d'essai, faux coups sur les sons sans batterie divisés par 2 (kick) et 5 (caisse claire) ;
- les références de consensus : les coups douteux n'apprennent plus rien de faux, et ne faussent plus la
  mesure ;
- un apprentissage dix fois plus long (40 → 400 passes) avec un GRU de 48 au lieu de 32 : kick +0,05,
  caisse claire +0,06 ;
- les 42 styles et les kits acoustiques (0.5.0) : kick +0,01 et caisse claire +0,02 sur les vrais morceaux,
  kick +0,03 sur MDB, +0,05 et +0,06 sur les kits acoustiques jamais entendus ;
- le seuil de silence relatif (0.5.0) : kick +0,16 et caisse claire +0,15 à -48 dB (mêmes poids), rien de
  changé à volume normal.

Le dernier complément des données de synthèse (kicks longs, extraits sans caisse claire, ajoutés en 0.4.0
pour le test `kicks_seuls_ne_donnent_aucune_caisse_claire`) avait coûté au kick 0,015 sur les morceaux de
l'auteur et 0,04 sur MDB, et rapporté 0,03 à la caisse claire sur MDB.

Essayé sans gain : GRU de 64, réseaux de décision de 48, 800 passes, d'autres entrées pour le GRU, attendre
50 ms au lieu de 30, une référence de kick par séparation de batterie pièce par pièce (DrumSep : pas
meilleure que le consensus), supprimer la règle « sans kick depuis 4 s, le premier doit être franc » (kick
+0,003, et 1 faux kick de plus par minute sur les sons sans batterie). Le banc de filtres sous-échantillonné
donne les mêmes scores que le calcul à pleine cadence.

Essayé le 2026-11-23, non livré : **apprendre aussi sur des packs de production actuels** (`kick_remix.py
--packs` : 6 000 coups isolés de 578 kits, packs gratuits d'éditeurs et Freesound ; kicks de hardstyle, de
techno, de trap, caisses claires et claps d'électro ; voir « Données »), 477 remixages de plus à 0,5. Le
modèle 0.5.0 y est plus faible qu'ailleurs (0,79 / 0,62 sur les kits tenus à l'écart : il manque surtout
les kicks longs de hardstyle, dont le pic vient 100 à 500 ms après l'attaque, et les claps joués seuls, dont
il trouve 30 %), et l'ajout corrige en partie ces sons-là, mais pas la vraie musique. Validation croisée,
les deux recettes refaites le même jour (F1 kick / F1 caisse claire) :

| | Recette 0.5.0 | + remixages aux packs |
|---|---|---|
| 107 morceaux de l'auteur | 0,887 / 0,786 | 0,882 / 0,773 |
| 93 morceaux variés | 0,881 / 0,830 | 0,874 / 0,819 |
| MDB Drums | 0,797 / 0,592 | 0,779 / 0,588 |
| 277 morceaux de 42 styles | 0,845 / 0,757 | 0,841 / 0,755 |
| Remixages d'essai, boîtes à rythmes | 0,899 / 0,742 | 0,909 / 0,740 |
| Remixages d'essai, packs (107 extraits, kits tenus à l'écart) | 0,759 / 0,637 | **0,797 / 0,652** |
| Faux coups par minute, 59 sons sans batterie | 5,8 / 0,7 | 4,4 / 0,8 |
| Faux coups par minute, 31 autres sons sans batterie | 13,1 / 3,5 | 11,6 / 4,2 |

Lecture : +0,04 au kick sur des sons de packs jamais entendus, rien sur les vrais morceaux (−0,004 à −0,018,
du même ordre que l'écart entre deux apprentissages de la même recette : la colonne de gauche diffère de
0,00 à 0,014 de celle du tableau plus haut). Par style, des écarts des deux signes sans tendance (house
−0,02 / −0,02, techno −0,03 au kick, trap +0,07 à la caisse claire). Le modèle appris sur toutes les données
(gardé hors du dépôt) passe les tests et donne au banc 0,82 / 0,66 sur les 477 remixages aux packs, 0,92 /
0,75 sur ceux aux boîtes à rythmes, et sur les morceaux appris 0,90 / 0,81 (auteur), 0,89 / 0,85 (variés),
0,82 / 0,64 (MDB), 0,87 / 0,79 (styles), avec plus de faux kicks d'intro (48 au lieu de 37) et de faux coups
sur les sons sans batterie appris (1,3 / 0,6 et 8,0 / 2,3 par minute au lieu de 1,0 / 0,5 et 5,7 / 1,7). Un
gain sur des sons isolés sans gain mesurable en musique n'en est pas un : la 0.5.0 reste livrée. Ce que
cela dit pour la suite : la limite sur la vraie musique n'est plus la variété des sons de batterie, mais la
référence (transcription automatique) et les cas de « Pistes suivantes ».

Limites connues : le kick effleuré et les balais du jazz acoustique (bebop, ballades : kick 0,47 à 0,49,
caisse claire 0,51 à 0,61) ; les musiques sans batterie mais à percussions (salsa, percussions seules) ;
les notes fantômes de la batterie acoustique (rappel de la caisse claire 0,46 sur MDB en validation
croisée) ; la double pédale très rapide du metal (plus de 10 coups par seconde : l'écart minimal entre deux
kicks, 0,1 s par défaut, les réunit) ; les sons graves et percutants sans batterie (contrebasse de
bluegrass, kalimba grave, tonnerre), encore pris pour des kicks ; un charleston fort frappé exactement avec
un kick long (808), parfois compté comme caisse claire.

### Données

Hors du dépôt (droits des morceaux) ; tout se refait avec les scripts.

- **Référence d'un morceau quelconque** (`kick_bench.py ref --demucs`) : ADTOF
  (<https://github.com/xavriley/ADTOF-pytorch>) transcrit la batterie séparée par Demucs (`htdemucs`) et, à
  part, le mélange complet. Vu par les deux : coup sûr (`.kicks`, `.caisses` ; précision 0,92 face aux
  annotations de MDB). Vu par une seule : douteux (`.kicks_doute`, `.caisses_doute`), ignoré à
  l'apprentissage comme à l'évaluation. `--pistes` garde les 4 pistes séparées pour les remixages.
- **MDB Drums** (<https://github.com/CarlSouthall/MDBDrums>) : annotations humaines, et 4 remixages de
  chaque morceau à partir de ses pistes (`kick_modele.py variantes`).
- **Morceaux** : trois jeux de vrais morceaux, 200 s chacun au plus. 107 morceaux de l'auteur (électro,
  dubstep, hardstyle, rap, pop, rock), 93 morceaux variés, et depuis la 0.5.0 **277 morceaux de 42 styles**
  (4 à 9 par style : du bebop à la trap, salsa, afrobeat, country, K-pop, Bollywood, musique de film,
  batterie et percussions seules… la liste est dans le tableau par style), pour que le modèle ne soit pas
  réglé sur les goûts d'une seule personne.
- **Remixages à instants exacts** (`kick_remix.py`) : l'accompagnement réel d'un morceau (basse + voix +
  reste, sans la batterie) reçoit une batterie refaite avec des sons isolés (boîtes à rythmes de
  <https://github.com/geikha/tidal-drum-machines>, banque <https://github.com/tidalcycles/Dirt-Samples>,
  kicks de synthèse) posés aux instants de la batterie d'origine. Deux versions par morceau, plus une sans
  batterie du tout (aucun coup). Dix boîtes à rythmes sont tenues à l'écart pour l'évaluation (`--essai`).
- **Remixages à batterie acoustique** (`kick_remix.py --acoustique`, 0.5.0) : la même chose avec des kits
  de batterie acoustique échantillonnés, où chaque coup est une autre frappe du même fût (nuances et
  variantes du kit, micros mélangés), avec des notes fantômes à la caisse claire (douteuses : ni justes ni
  fausses) et parfois une ride au lieu du charleston. 23 kits pour l'apprentissage, 5 tenus à l'écart
  pour l'évaluation. Tous libres : AVL Drumkits, Karoryfer (Big Rusty Drums, Swirly Drums, Frankensnare,
  Unruly Drums), Virtuosity Drums, MuldjordKit et les kits acoustiques de Hydrogen (adresses dans
  `acoustic_kits`, `kick_remix.py`).
- **Remixages aux packs de production** (`kick_remix.py --packs`, essai du 2026-11-23, hors de la recette
  livrée) : la même chose avec des coups isolés actuels. `kick_freesound.py` les cherche sur Freesound
  (5 200 sons CC0 ou CC BY de 743 auteurs, un kit par auteur, classés d'après leur nom ; écoutes Ogg, la
  clé d'API ne donnant pas les originaux) et `kick_packs.py ranger` range des packs gratuits d'éditeurs et
  de dépôts libres (27 packs : MusicRadar SampleRadar, dépôts GitHub, archive.org ; liste et licences à
  noter à côté des données, les packs SampleRadar ne se redistribuent pas). `kick_packs.py trier` écarte les
  kicks, caisses claires et claps à l'attaque lente ou au début trop faible (282 sur 5 380). Le kick est
  monophonique (le suivant coupe la queue d'un kick long), la caisse claire vient une fois sur deux d'un
  autre kit, un clap la double (40 %) ou la remplace (20 %). Un kit sur cinq, tiré de son nom, est tenu à
  l'écart pour l'évaluation (152 kits, 2 300 sons) ; les sons des boîtes à rythmes d'essai (909, LinnDrum…)
  ne sont jamais joués. `<nom>_v0.sons` nomme les sons de chaque extrait.
- **Sons sans batterie** : voix parlée et chantée, chœurs, piano, clavecin, orgue, accordéon, cordes,
  guitare, contrebasse, kora, sitar, didgeridoo, ASMR, nappes, pluie, orage, train, foule (fichiers `.kicks`
  et `.caisses` vides) : tout coup y est faux. 59 sons depuis la 0.4.0, 31 de plus en 0.5.0.
- **Boîte à rythmes de synthèse** (`kick_modele.py synth`) : les cas que la vraie musique montre peu ou
  étiquette mal : kick de sinus pur, kick long qui ondule (808), caisses claires seules, roulements de
  montée sans kick, extraits sans aucune caisse claire.

```
python scripts/kick_bench.py mdb <MDB Drums> <données>/mdb                               # annotations humaines
python scripts/kick_bench.py ref <données>/perso --demucs --pistes <pistes>/perso <dossier de musique>   # consensus ADTOF (GPU)
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remix     # + <données>/remixsb (sans batterie)
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remixtest 1 --essai
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remixac 1 --acoustique   # kits acoustiques
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remixactest 2 --acoustique --essai
python scripts/kick_freesound.py <fichier de la clé d'API Freesound> <banques>/packs      # coups isolés de Freesound
python scripts/kick_packs.py ranger <packs téléchargés> <banques>/packs                   # packs d'éditeurs -> kits
python scripts/kick_packs.py trier <banques>/packs                                        # écarte les sons qui ne sont pas des coups francs
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remixpk 1 --packs
python scripts/kick_remix.py <données>/perso <pistes>/perso <banques> <données>/remixpktest 2 --packs --essai
cargo run --release --example kick_bench -- <données>/perso [sensibilité] [délai] [-v] [--detail <fichier.tsv>]
```

Un morceau nommé `<style>__<titre>` porte son style : `--detail` (banc Rust comme `kick_modele.py valider`)
écrit une ligne par morceau et par détecteur (coups trouvés, annoncés, attendus, durée), de quoi refaire le
tableau par style. Un deuxième appel de `kick_remix.py` vers le même dossier y ajoute ses morceaux.

### Réapprendre le modèle

```
python scripts/kick_modele.py variantes <MDB Drums> <données>/mdbaug   # remixages de MDB
python scripts/kick_modele.py synth <données>/synth 300                # boîte à rythmes, étiquettes exactes
cd <données>
python <dépôt>/scripts/kick_modele.py valider perso varie mdb styles remix@.5 remixv@.5 remixsb@.5 remixvsb@.5 remixst@.5 remixstsb@.5 remixac@.5 neg@.5 stylesneg@.5 --leger mdbaug synth@0.25/0.1 --essai remixtest remixactest neg stylesneg --detail <fichier.tsv>
python <dépôt>/scripts/kick_modele.py apprendre <dépôt>/src/kick_modele.rs perso varie mdb styles remix@.5 remixv@.5 remixsb@.5 remixvsb@.5 remixst@.5 remixstsb@.5 remixac@.5 neg@.5 stylesneg@.5 --leger mdbaug synth@0.25/0.1
python <dépôt>/scripts/kick_modele.py detecter <dépôt>/src/kick_modele.rs <morceau.f32>   # doit donner les coups de :
cargo run --release --example kick_mesures -- <morceau.f32>
```

`perso`, `varie` et `styles` sont les vrais morceaux (poids 1) ; `remix`, `remixv` et `remixst` leurs
remixages aux boîtes à rythmes, `remix*sb` les mêmes sans batterie, `remixac` les remixages à batterie
acoustique (0,5) ; `neg` et `stylesneg` les sons sans batterie (0,5) ; les données d'appoint (`--leger`,
0,25) apportent le rock acoustique (remixages de MDB) et la boîte à rythmes de synthèse, qui ne pèse que 0,1
pour la caisse claire (`@0.25/0.1`). Sur une carte graphique (RTX 4070), un apprentissage dure 6 minutes
et une validation croisée 30 (dont 12 de mesures la première fois) ; les mesures de tous les morceaux
(2 600 pour l'apprentissage, 76 heures) tiennent en mémoire vive, pas sur la carte : compter 6 Go de
mémoire vive et 4 Go de mémoire graphique, et ne pas en lancer deux à la fois. Avec `--reprise <fichier>`,
une validation interrompue puis relancée telle quelle reprend au groupe suivant. Les mesures de `kick_modele.py`
(`levels`, `macro`, `prep`, `candidates`, `features`, `frame_inputs`, `decisions`) sont reproduites à
l'identique dans `kick.rs` : les changer des deux côtés, puis réapprendre ; `detecter` et `kick_mesures`
doivent donner les mêmes coups aux mêmes instants (vérifié sur 5 morceaux : aucun écart). Les niveaux par
bande sont gardés dans `<dossier>/.mesures/` (à effacer si `levels` change). Environnement utilisé :
Python 3.11, torch 2.6 (CUDA), demucs, adtof-pytorch, scipy, soundfile ; ffmpeg.

### Pistes suivantes (non faites, notées le 2026-11-21)

Par ordre de gain attendu. Chacune se juge comme les précédentes : `kick_modele.py valider`, puis tests
synthétiques et `kick_bench`, chiffres ici.

1. **Jazz acoustique, notes fantômes, caisse claire des boîtes à rythmes** : les scores les plus bas (kick
   0,47 en bebop, rappel de la caisse claire 0,46 sur MDB, caisse claire 0,73 sur les remixages d'essai). Les
   remixages à batterie acoustique de la 0.5.0 ont aidé sur les kits inconnus, pas sur le jeu effleuré.
   Pistes : des annotations humaines de jazz (la transcription automatique y est trop incertaine pour servir
   de référence), des frappes très douces et des balais dans les remixages, une cible élargie aux candidats
   voisins d'un vrai coup.
2. **Toms et charleston** : ADTOF les transcrit aussi (notes 47 et 42) ; un détecteur de plus coûterait
   trois petits réseaux, le contexte étant déjà calculé.
3. **Démarrage** : à 90 BPM, le test d'Essaim `tempo_trouve_sur_des_kicks_reguliers` montre que les 2
   premiers kicks d'un flux sont manqués, à cause de la règle « sans kick depuis 4 s, le premier doit être
   franc ». Supprimer la règle ne rapporte presque rien en moyenne (kick +0,003, un faux kick de plus par
   minute sur les sons sans batterie) : reste à essayer un relèvement plus court, jugé sur les faux kicks
   d'intro (37 sur 107 morceaux aujourd'hui).
4. **Tempo (`pulsation`)** sur l'activité continue de la batterie (sortie des réseaux à chaque attaque)
   plutôt que sur les seuls instants des kicks, à la manière de BeatNet+ (Heydari 2024, filtre particulaire
   en ligne) : plus robuste quand le kick manque (breaks, rap sans kick régulier).
5. **Niveau et hauteur de la basse** : inchangés depuis la 0.2.0, faute de référence objective. La piste
   `bass` de Demucs (gardée par `--pistes`) en fournirait une : niveau de la basse seule, hauteur par pYIN
   hors ligne.

Lectures : Böck et Widmer 2013 (détection d'attaques en ligne), Vogl et al. 2017 (transcription de
batterie), Zehren et al. (ADTOF), Weber et al. (STAR Drums : batterie refaite sur accompagnement réel ;
astuces d'entraînement de 2024), Heydari et al. 2024 (BeatNet+).
