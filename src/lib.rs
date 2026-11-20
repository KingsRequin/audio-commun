//! Son de Windows partagé par Prisme (LED), Horizon (barre) et Essaim (curseur).
//!
//! - [`capture`] : lit une sortie son en loopback (WASAPI, mode événement, mono) dans un fil
//!   dédié, avec priorité audio (MMCSS), relance quand la sortie change et silence injecté quand
//!   rien ne joue ; sortie par défaut ou choisie ([`capture::sorties`], [`capture::start_sur`]).
//!   Ce qu'on fait des échantillons est laissé au [`capture::Sink`].
//! - [`spectre`] : analyse par FFT (bandes mel, attaques, tempo, intensité, tonalité), à cadence
//!   fixe ; [`spectre::start`] la branche sur la capture.
//! - [`kick`] : détection de grosse caisse et de caisse claire à très faible délai (biquads,
//!   sans FFT), niveau et hauteur de la basse ; utilisée par Essaim, qui veut réagir en 10 à 20 ms.
//! - [`pulsation`] : tempo (BPM) et phase du temps d'après les seuls instants des kicks, sans
//!   FFT ; relais pendant un passage sans kick.
//!
//! Chaque application garde sa propre capture : partager l'analyse entre processus ajouterait
//! du délai et une dépendance au lancement d'un autre programme, pour un gain négligeable
//! (une capture loopback ne fait que lire le mélange déjà calculé par Windows).

#[cfg(feature = "capture")]
pub mod capture;
pub mod kick;
mod kick_modele;
pub mod pulsation;
#[cfg(feature = "spectre")]
pub mod spectre;
