//! Son de Windows partagé par Prisme (LED), Horizon (barre) et Essaim (curseur).
//!
//! - [`capture`] : lit une sortie son en loopback (WASAPI, mode événement, mono) dans un fil
//!   dédié, avec priorité audio (MMCSS), relance quand la sortie change et silence injecté quand
//!   rien ne joue ; sortie par défaut ou choisie ([`capture::sorties`], [`capture::start_sur`]),
//!   ou son de certaines applications ([`capture::applis_sonores`], [`capture::start_ecoute`]).
//!   Ce qu'on fait des échantillons est laissé au [`capture::Sink`].
//! - [`spectre`] : analyse par FFT (bandes mel, charleston, tempo, intensité, tonalité), à cadence
//!   fixe, avec la grosse caisse et la caisse claire de [`kick`] ; [`spectre::start`] la branche
//!   sur la capture.
//! - [`kick`] : détection de grosse caisse et de caisse claire à très faible délai (biquads,
//!   sans FFT), niveau et hauteur de la basse ; utilisée par Essaim, qui veut réagir en 10 à 20 ms,
//!   et par [`spectre`].
//! - [`pulsation`] : tempo (BPM) et phase du temps d'après les seuls instants des kicks, sans
//!   FFT ; relais pendant un passage sans kick.
//!
//! Chaque application garde sa propre capture : partager l'analyse entre processus ajouterait
//! du délai et une dépendance au lancement d'un autre programme, pour un gain négligeable
//! (une capture loopback ne fait que lire le mélange déjà calculé par Windows).

/// Amplitude maximale gardée d'un échantillon (+18 dB au-dessus de la pleine échelle : un mélange
/// en flottants peut dépasser 1, jamais de loin). Au-delà, les carrés des calculs de puissance
/// débordent en infini.
pub const ECHANTILLON_MAX: f32 = 8.0;

/// Échantillon ramené dans des bornes sûres, 0 s'il n'est pas fini. Une appli peut émettre un
/// NaN ou un infini : un seul suffirait à bloquer pour toujours les filtres et les moyennes
/// glissantes de l'analyse. Sans effet sur un son normal.
#[inline]
pub fn echantillon_sain(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-ECHANTILLON_MAX, ECHANTILLON_MAX)
    } else {
        0.0
    }
}

#[cfg(feature = "capture")]
pub mod capture;
pub mod kick;
mod kick_modele;
pub mod pulsation;
#[cfg(feature = "spectre")]
pub mod spectre;
