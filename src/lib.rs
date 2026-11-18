//! Son de Windows partagé par Prisme (LED), Horizon (barre) et Essaim (curseur).
//!
//! - [`capture`] : lit la sortie son par défaut en loopback (WASAPI, mode événement, mono) dans
//!   un fil dédié, avec priorité audio (MMCSS), relance quand la sortie change et silence injecté
//!   quand rien ne joue. Ce qu'on fait des échantillons est laissé au [`capture::Sink`].
//! - [`spectre`] : analyse par FFT (bandes mel, attaques, tempo, intensité, tonalité), à cadence
//!   fixe ; [`spectre::start`] la branche sur la capture.
//! - [`kick`] : détection de grosse caisse à très faible délai (biquads, sans FFT), niveau et
//!   hauteur de la basse ; utilisée par Essaim, qui veut réagir en 10 à 20 ms.
//!
//! Chaque application garde sa propre capture : partager l'analyse entre processus ajouterait
//! du délai et une dépendance au lancement d'un autre programme, pour un gain négligeable
//! (une capture loopback ne fait que lire le mélange déjà calculé par Windows).

#[cfg(feature = "capture")]
pub mod capture;
pub mod kick;
mod kick_modele;
#[cfg(feature = "spectre")]
pub mod spectre;
