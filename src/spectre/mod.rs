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
    /// Enveloppes d'attaque : sautent à la force de l'attaque puis retombent en ~0,1 s.
    /// Grosse caisse (40–120 Hz), caisse claire / claps (1,5–5 kHz), charleston (> 8 kHz).
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
    /// Détecteur fin d'Essaim ([`crate::kick`], seulement avec `Options::kick`) : compteur de
    /// grosses caisses (sans les caisses claires, délai 10–20 ms) et basse lissée (0–1).
    pub kick_hits: u32,
    pub kick_bass: f32,
}

impl AudioFeatures {
    /// Remet les niveaux à zéro (silence, capture interrompue) sans toucher aux compteurs.
    pub fn quiet(&mut self) {
        self.bands.iter_mut().for_each(|b| *b = 0.0);
        (self.volume, self.bass, self.mid, self.treble) = (0.0, 0.0, 0.0, 0.0);
        (self.kick, self.snare, self.hihat) = (0.0, 0.0, 0.0);
        self.kick_bass = 0.0;
        (self.bpm, self.beat_phase, self.tempo_confidence) = (0.0, 0.0, 0.0);
        self.intensity = 0.5;
        self.silent = true;
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
    /// Fait aussi tourner le détecteur de kick d'Essaim (`kick_hits`, `kick_bass`).
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
        analyzer: None,
        kick: None,
        features: AudioFeatures::default(),
        next_tick: std::time::Instant::now(),
        out,
    };
    Ok(crate::capture::start(name, sink)?)
}

/// Analyse à cadence fixe branchée sur la capture.
#[cfg(feature = "capture")]
struct Runner {
    opts: Options,
    dt: std::time::Duration,
    analyzer: Option<Analyzer>,
    /// Détecteur fin, si `opts.kick`.
    kick: Option<crate::kick::Analyzer>,
    features: AudioFeatures,
    next_tick: std::time::Instant,
    out: std::sync::Arc<std::sync::Mutex<AudioFeatures>>,
}

#[cfg(feature = "capture")]
impl Runner {
    fn publish(&self) {
        if let Ok(mut f) = self.out.lock() {
            f.clone_from(&self.features); // réutilise la capacité de `bands`
        }
    }
}

#[cfg(feature = "capture")]
impl crate::capture::Sink for Runner {
    fn open(&mut self, sample_rate: u32) {
        self.analyzer = Some(Analyzer::new(sample_rate, self.opts.bands, self.opts.rate));
        self.kick = self.opts.kick.then(|| crate::kick::Analyzer::new(sample_rate));
        self.next_tick = std::time::Instant::now() + self.dt;
    }

    fn samples(&mut self, mono: &[f32]) {
        if let Some(a) = &mut self.analyzer {
            a.push(mono);
        }
        if let Some(k) = &mut self.kick {
            k.push(mono);
        }
    }

    fn silence(&mut self, frames: usize) {
        if let Some(a) = &mut self.analyzer {
            a.push_silence(frames);
        }
        if let Some(k) = &mut self.kick {
            k.push_silence(frames);
        }
    }

    fn tick(&mut self, now: std::time::Instant) -> std::time::Duration {
        if now >= self.next_tick {
            if let Some(a) = &mut self.analyzer {
                a.analyze(&mut self.features);
                if let Some(k) = &self.kick {
                    (self.features.kick_hits, self.features.kick_bass) = (k.kicks, k.bass);
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
