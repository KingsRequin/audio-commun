# audio-commun

Bibliothèque partagée par Prisme, Horizon et Essaim (dépendance git, ou chemin vers un clone voisin). Lire le
README. Consignes propres à un poste : `CLAUDE.local.md` (non suivi).

- Vérification : `cargo test --release`, `cargo clippy --all-targets -- -D warnings`,
  `cargo clippy --no-default-features --all-targets -- -D warnings`.
- Toute modification touche trois applications : les recompiler (leurs propres vérifications) et les réinstaller
  (`scripts\install.ps1` de chacune).
- `kick` (Essaim) est un modèle appris (README, section « Kick ») : tests synthétiques de référence (aucune
  caisse claire comptée comme kick, kicks trouvés avec et sans basse, délai médian < 30 ms ; caisses claires
  trouvées seules et avec les kicks, aucune sur des kicks seuls) **et** banc sur
  vraie musique (`--example kick_bench`, données hors du dépôt). Tout
  changement de mesure ou de seuil : passer les deux, et noter les chiffres du banc dans le README. Les poids
  `src/kick_modele.rs` sont générés (`scripts/kick_modele.py apprendre`), jamais modifiés à la main.
- `spectre` est réglé pour les LED de Prisme : ne pas changer son comportement sans relancer sa sonde
  (`--example audio_probe -- 15` dans Prisme).
