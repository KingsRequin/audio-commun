//! Analyse spectrale du son (voir [`Analyzer`]) et ses résultats ([`AudioFeatures`]).
//!
//! [`start`] branche l'analyse sur la capture : l'analyse tourne à cadence fixe (`rate` images
//! par seconde), quelle que soit la taille des paquets WASAPI, et chaque image est recopiée dans
//! un `Arc<Mutex<AudioFeatures>>` partagé.

mod analyse;
mod onset;
mod tempo;

pub use analyse::Analyzer;

/// Caractéristiques du son à un instant.
///
/// Les niveaux sont relatifs à l'habitude de chaque bande (gain automatique par bande) :
/// une bande à son niveau moyen vaut ~0,3, ses pointes s'approchent de 1. Les événements
/// (attaques, temps, drops) sont des **compteurs** : l'analyse et le rendu ne tournent pas
/// en phase, un consommateur compare donc le compteur à la dernière valeur vue (voir
/// [`EventCounter`]) au lieu de lire un drapeau qui pourrait être manqué ou vu deux fois.
#[derive(Clone, Debug, Default)]
pub struct AudioFeatures {
    /// Énergie par bande (échelle mel, des graves aux aigus).
    pub bands: Vec<f32>,
    /// Niveau global.
    pub volume: f32,
    /// Graves (< 150 Hz), médiums, aigus (> 4 kHz).
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    /// Enveloppes d'attaque : sautent à la force de l'attaque (0,4 à 1) puis retombent en
    /// ~0,1 s. Grosse caisse et caisse claire / claps : détecteur appris ([`crate::kick`]) ;
    /// charleston : flux spectral au-dessus de 8 kHz.
    pub kick: f32,
    pub snare: f32,
    pub hihat: f32,
    /// Compteurs d'attaques (bouclent).
    pub kicks: u32,
    pub snares: u32,
    pub hihats: u32,
    /// Temps de la grille rythmique (suivi du tempo) ; sans tempo fiable, suit les grosses caisses.
    pub beats: u32,
    /// Tempo estimé (BPM), 0 si inconnu.
    pub bpm: f32,
    /// Position dans le temps en cours : 0 sur le temps, → 1 juste avant le suivant.
    pub beat_phase: f32,
    /// Confiance dans le tempo (0 = aucune régularité, > 0,3 = rythme net).
    pub tempo_confidence: f32,
    /// Intensité du passage : 0,5 = niveau habituel du morceau, > 0,7 = montée / refrain,
    /// < 0,3 = couplet calme ou pont.
    pub intensity: f32,
    /// Compteur de « drops » (retour brutal de l'énergie après un passage calme).
    pub drops: u32,
    /// Brillance du timbre (centroïde spectral, échelle logarithmique 80 Hz → 12 kHz).
    pub centroid: f32,
    /// Note dominante sur le cycle des quintes (0–1, se lit comme une teinte : deux
    /// tonalités voisines donnent deux teintes voisines) et sa netteté.
    pub tone: f32,
    pub tone_strength: f32,
    /// Sous le seuil de silence.
    pub silent: bool,
    /// Le même compteur que `kicks` (gardé pour les applications écrites quand `kicks` venait
    /// du flux spectral), et la basse lissée du détecteur appris (0–1).
    pub kick_hits: u32,
    pub kick_bass: f32,
}

impl AudioFeatures {
    /// Remet les niveaux à zéro (silence, capture interrompue) sans toucher aux compteurs. Le
    /// timbre et la tonalité gardent leur dernière valeur, sauf si elle n'est pas finie.
    pub fn quiet(&mut self) {
        self.bands.iter_mut().for_each(|b| *b = 0.0);
        (self.volume, self.bass, self.mid, self.treble) = (0.0, 0.0, 0.0, 0.0);
        (self.kick, self.snare, self.hihat) = (0.0, 0.0, 0.0);
        self.kick_bass = 0.0;
        (self.bpm, self.beat_phase, self.tempo_confidence) = (0.0, 0.0, 0.0);
        self.intensity = 0.5;
        self.silent = true;
        for v in [&mut self.centroid, &mut self.tone, &mut self.tone_strength] {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
    }

    /// Toutes les valeurs flottantes sont finies. Déstructuration complète : un champ ajouté
    /// doit être examiné ici (et dans [`AudioFeatures::quiet`]).
    pub fn all_finite(&self) -> bool {
        let AudioFeatures {
            bands,
            volume,
            bass,
            mid,
            treble,
            kick,
            snare,
            hihat,
            kicks: _,
            snares: _,
            hihats: _,
            beats: _,
            bpm,
            beat_phase,
            tempo_confidence,
            intensity,
            drops: _,
            centroid,
            tone,
            tone_strength,
            silent: _,
            kick_hits: _,
            kick_bass,
        } = self;
        bands.iter().all(|b| b.is_finite())
            && [
                volume,
                bass,
                mid,
                treble,
                kick,
                snare,
                hihat,
                bpm,
                beat_phase,
                tempo_confidence,
                intensity,
                centroid,
                tone,
                tone_strength,
                kick_bass,
            ]
            .iter()
            .all(|v| v.is_finite())
    }
}

/// Durée pendant laquelle l'analyse peut rendre des valeurs non finies avant d'être relancée.
#[cfg(feature = "capture")]
const RELANCE_NAN: std::time::Duration = std::time::Duration::from_secs(1);

/// Garde-fou des images d'analyse : celles qui ne sont pas finies sont publiées comme du
/// silence (compteurs gardés), et l'analyse est relancée quand cela dure plus de [`RELANCE_NAN`].
/// Les échantillons sont déjà nettoyés à l'entrée : il ne sert que si un calcul déraille.
#[cfg(feature = "capture")]
#[derive(Default)]
struct GardeNan {
    /// Première image non finie de la série en cours.
    depuis: Option<std::time::Instant>,
    /// Dernier avertissement écrit au journal (au plus un par minute).
    averti: Option<std::time::Instant>,
}

#[cfg(feature = "capture")]
impl GardeNan {
    /// Neutralise `f` s'il n'est pas fini ; `true` quand l'analyse doit être relancée.
    fn verifier(&mut self, f: &mut AudioFeatures, now: std::time::Instant) -> bool {
        if f.all_finite() {
            self.depuis = None;
            return false;
        }
        f.quiet();
        let depuis = *self.depuis.get_or_insert(now);
        if now.duration_since(depuis) < RELANCE_NAN {
            return false;
        }
        self.depuis = None;
        if self.averti.is_none_or(|t| now.duration_since(t).as_secs() >= 60) {
            self.averti = Some(now);
            log::warn!("analyse audio : valeurs non finies depuis 1 s, analyse relancée");
        }
        true
    }
}

/// Détecte les nouveaux événements d'un compteur de [`AudioFeatures`].
#[derive(Clone, Copy, Debug, Default)]
pub struct EventCounter {
    last: Option<u32>,
}

impl EventCounter {
    /// Nombre d'événements depuis l'appel précédent (0 au premier appel : pas de rafale
    /// quand un consommateur démarre en cours de morceau).
    pub fn take(&mut self, count: u32) -> u32 {
        let n = self.last.map_or(0, |l| count.wrapping_sub(l));
        self.last = Some(count);
        // Plus de 8 d'un coup = compteur réinitialisé (nouvelle capture) : on l'ignore.
        if n > 8 {
            0
        } else {
            n
        }
    }
}

/// Réglages de [`start`].
#[derive(Clone, Debug)]
pub struct Options {
    /// Nombre de bandes mel produites.
    pub bands: usize,
    /// Images d'analyse par seconde.
    pub rate: f32,
    /// Sans effet depuis la 0.6.0 : le détecteur appris tourne toujours. Gardé pour les
    /// applications qui le règlent encore.
    pub kick: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bands: 32,
            rate: 60.0,
            kick: false,
        }
    }
}

/// Lance capture et analyse dans un fil nommé `name` ; `out` reçoit chaque image d'analyse.
/// L'arrêt se fait en lâchant la [`Capture`](crate::capture::Capture) rendue.
#[cfg(feature = "capture")]
pub fn start(
    name: &str,
    opts: Options,
    out: std::sync::Arc<std::sync::Mutex<AudioFeatures>>,
) -> anyhow::Result<crate::capture::Capture> {
    anyhow::ensure!(
        opts.rate.is_finite() && opts.rate > 0.0 && opts.bands > 0,
        "options audio invalides : {opts:?}"
    );
    let sink = Runner {
        dt: std::time::Duration::from_secs_f32(1.0 / opts.rate),
        opts,
        sample_rate: 0,
        analyzer: None,
        features: AudioFeatures::default(),
        next_tick: std::time::Instant::now(),
        out,
        garde: GardeNan::default(),
    };
    Ok(crate::capture::start(name, sink)?)
}

/// Analyse à cadence fixe branchée sur la capture.
#[cfg(feature = "capture")]
struct Runner {
    opts: Options,
    dt: std::time::Duration,
    sample_rate: u32,
    analyzer: Option<Analyzer>,
    features: AudioFeatures,
    next_tick: std::time::Instant,
    out: std::sync::Arc<std::sync::Mutex<AudioFeatures>>,
    garde: GardeNan,
}

#[cfg(feature = "capture")]
impl Runner {
    fn nouvel_analyseur(&self) -> Analyzer {
        Analyzer::new(self.sample_rate, self.opts.bands, self.opts.rate)
    }

    /// Recopie l'image dans `out`, même si un consommateur a paniqué en tenant le verrou (sinon
    /// l'analyse cesserait d'être publiée pour toujours).
    fn publish(&self) {
        let mut f = self.out.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.clone_from(&self.features); // réutilise la capacité de `bands`
    }
}

#[cfg(feature = "capture")]
impl crate::capture::Sink for Runner {
    fn open(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate;
        self.analyzer = Some(self.nouvel_analyseur());
        self.next_tick = std::time::Instant::now() + self.dt;
    }

    fn samples(&mut self, mono: &[f32]) {
        if let Some(a) = &mut self.analyzer {
            a.push(mono);
        }
    }

    fn silence(&mut self, frames: usize) {
        if let Some(a) = &mut self.analyzer {
            a.push_silence(frames);
        }
    }

    fn tick(&mut self, now: std::time::Instant) -> std::time::Duration {
        if now >= self.next_tick {
            if let Some(a) = &mut self.analyzer {
                a.analyze(&mut self.features);
                if self.garde.verifier(&mut self.features, now) {
                    self.analyzer = Some(self.nouvel_analyseur());
                }
                self.publish();
            }
            self.next_tick += self.dt;
            if self.next_tick < now {
                self.next_tick = now + self.dt; // retard (machine chargée) : pas de rafale
            }
        }
        self.next_tick.saturating_duration_since(now)
    }

    fn lost(&mut self) {
        // Plus de données : les consommateurs doivent voir du silence pendant la reconnexion.
        self.features.quiet();
        self.publish();
    }
}

#[cfg(all(test, feature = "capture"))]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Caractéristiques dont toutes les valeurs flottantes sont NaN.
    fn toutes_nan() -> AudioFeatures {
        let n = f32::NAN;
        AudioFeatures {
            bands: vec![n; 4],
            volume: n,
            bass: n,
            mid: n,
            treble: n,
            kick: n,
            snare: n,
            hihat: n,
            kicks: 3,
            beats: 7,
            bpm: n,
            beat_phase: n,
            tempo_confidence: n,
            intensity: n,
            centroid: n,
            tone: n,
            tone_strength: n,
            kick_bass: n,
            ..AudioFeatures::default()
        }
    }

    /// Images non finies : publiées comme du silence (compteurs gardés) aussitôt, relance
    /// demandée une fois qu'elles durent depuis `RELANCE_NAN`, série oubliée dès une image finie.
    #[test]
    fn garde_neutralise_puis_relance() {
        let mut g = GardeNan::default();
        let t0 = Instant::now();
        let mut f = toutes_nan();
        assert!(!g.verifier(&mut f, t0));
        assert!(f.all_finite() && f.silent && f.volume == 0.0);
        assert_eq!((f.kicks, f.beats), (3, 7), "compteurs gardés");
        let mut f = toutes_nan();
        assert!(!g.verifier(&mut f, t0 + RELANCE_NAN / 2));
        let mut f = toutes_nan();
        assert!(g.verifier(&mut f, t0 + RELANCE_NAN), "relance");
        assert!(f.all_finite());
        let mut f = toutes_nan();
        assert!(!g.verifier(&mut f, t0 + RELANCE_NAN * 2));
        let mut fini = AudioFeatures::default();
        assert!(!g.verifier(&mut fini, t0 + RELANCE_NAN * 3));
        let mut f = toutes_nan();
        assert!(!g.verifier(&mut f, t0 + RELANCE_NAN * 4), "série interrompue : délai repris");
    }
}
