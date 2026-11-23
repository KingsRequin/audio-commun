//! Chaîne d'analyse audio (inspirée de LedFx, WLED-MM, scottlawsonbc, librosa), indépendante
//! de WASAPI. À chaque image :
//!
//! - **grande FFT** (~85 ms, Hamming, compensation de pente « bruit rose plat ») → banc de
//!   filtres mel → bandes, graves / médiums / aigus, centroïde spectral, chromagramme ;
//! - **petite FFT** (~21 ms) → flux spectral net dans le temps : charleston et signal
//!   d'attaques du suivi de tempo, renforcé par la montée des graves de la grande FFT (seule
//!   assez fine sous 120 Hz) ;
//! - grosse caisse et caisse claire : le détecteur appris de [`crate::kick`], qui tourne sur
//!   les mêmes échantillons (un flux spectral par plage de fréquences annonçait six fois plus
//!   de faux coups, voir le README) ; ses kicks recalent aussi la grille du tempo ;
//! - niveaux **normalisés par canal** : chaque bande est jugée par rapport à sa propre
//!   habitude (moyenne glissante en dB) et à son plafond récent, bornés par le niveau global
//!   pour ne pas amplifier une bande presque vide. Une bande stable reste discrète, ses
//!   pointes ressortent : c'est ce contraste qui fait « danser » les LED.
//!
//! Aucune allocation après `new` : tous les tampons sont pré-alloués.

use super::onset;
use super::tempo;

use onset::{Flux, Onset};
use super::AudioFeatures;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::f32::consts::PI;
use std::sync::Arc;
use tempo::Tempo;

/// Réglages de la chaîne.
mod tuning {
    /// Fenêtres d'analyse (arrondies à la puissance de 2 supérieure : 4096 et 1024 à 48 kHz).
    pub const WINDOW_S: f32 = 0.08;
    pub const ONSET_WINDOW_S: f32 = 0.02;
    pub const F_MIN: f32 = 20.0;
    pub const F_MAX: f32 = 16_000.0;
    pub const BASS_MAX: f32 = 150.0;
    pub const TREBLE_MIN: f32 = 4_000.0;
    /// Plages des attaques.
    pub const KICK: (f32, f32) = (40.0, 120.0);
    pub const HIHAT: (f32, f32) = (8_000.0, 16_000.0);
    pub const OSS_MAX: f32 = 8_000.0;
    pub const OSS_KICK_WEIGHT: f32 = 2.0;
    /// Poids d'une grosse caisse reconnue (× sa force) dans le signal du tempo. Mesuré au banc
    /// (`tempo_bench`) : la justesse des temps monte jusqu'à 3 puis plafonne, et au-delà le
    /// tempo n'est plus annoncé dans les passages sans kick régulier.
    pub const OSS_HIT_WEIGHT: f32 = 3.0;
    /// Normalisation par canal (dB) : plafond (montée / descente), moyenne glissante,
    /// dynamique sous le plafond, écart à la moyenne, et portée sous le canal le plus fort
    /// au-delà de laquelle une bande est considérée vide.
    pub const PEAK_RISE_S: f32 = 0.05;
    pub const PEAK_FALL_S: f32 = 3.0;
    /// Habitude : rattrape vite une hausse durable (1 s), oublie lentement (3 s).
    pub const MEAN_RISE_S: f32 = 1.0;
    pub const MEAN_FALL_S: f32 = 3.0;
    pub const LEVEL_RANGE_DB: f32 = 30.0;
    /// Écart à la moyenne : −`DEV_BELOW` dB → 0, +`DEV_ABOVE` dB → 1.
    pub const DEV_BELOW_DB: f32 = 4.0;
    pub const DEV_ABOVE_DB: f32 = 10.0;
    /// Part du niveau absolu (le reste = écart à l'habitude).
    pub const LEVEL_WEIGHT: f32 = 0.35;
    pub const GATE_SPAN_DB: f32 = 42.0;
    /// Amplitude minimale prise en compte (−70 dBFS) : en dessous, rien n'est amplifié.
    pub const FLOOR_DB: f32 = -70.0;
    /// Lissage des sorties : attaque / relâchement (s).
    pub const ATTACK_S: f32 = 0.015;
    pub const RELEASE_S: f32 = 0.10;
    pub const VOLUME_RELEASE_S: f32 = 0.2;
    /// Silence : RMS sous −60 dBFS pendant 250 ms ; sortie dès −55 dBFS.
    pub const SILENT_ENTER: f32 = 1e-3;
    pub const SILENT_EXIT: f32 = 1.78e-3;
    pub const SILENT_HOLD_S: f32 = 0.25;
    /// Gain de référence des flux (plafond global, linéaire) : descente lente, plancher.
    pub const GAIN_FALL_S: f32 = 4.0;
    pub const GAIN_FLOOR: f32 = 3e-4;
    /// Intensité : volume court vs long (dB), ±`INTENSITY_DB` → 0 / 1.
    pub const LOUD_SHORT_S: f32 = 0.5;
    pub const LOUD_LONG_S: f32 = 12.0;
    pub const INTENSITY_DB: f32 = 8.0;
    /// Le niveau long suit le court pendant la première seconde de son (amorçage).
    pub const LOUD_WARMUP_S: f32 = 1.0;
    /// Drop : intensité > `DROP_HIGH` moins de `DROP_WINDOW_S` après un passage < `DROP_LOW`.
    pub const DROP_LOW: f32 = 0.32;
    pub const DROP_HIGH: f32 = 0.66;
    pub const DROP_WINDOW_S: f32 = 4.0;
    pub const DROP_MIN_GAP_S: f32 = 8.0;
    /// Centroïde et chromagramme.
    pub const CENTROID: (f32, f32) = (80.0, 12_000.0);
    pub const CENTROID_S: f32 = 0.3;
    pub const CHROMA: (f32, f32) = (260.0, 4_200.0);
    pub const CHROMA_S: f32 = 1.5;
}
use tuning::*;

fn mel(f: f32) -> f32 {
    2595.0 * (1.0 + f / 700.0).log10()
}

fn mel_inv(m: f32) -> f32 {
    700.0 * (10f32.powf(m / 2595.0) - 1.0)
}

/// Coefficient d'un filtre exponentiel pour une constante de temps `tau` et un pas `dt`.
fn alpha(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt / tau.max(1e-6)).exp()
}

fn db(a: f32) -> f32 {
    20.0 * a.max(1e-9).log10()
}

/// Lissage asymétrique : montée rapide, descente lente.
#[derive(Clone, Copy)]
struct Asym {
    up: f32,
    down: f32,
}

impl Asym {
    fn new(dt: f32, attack: f32, release: f32) -> Self {
        Self {
            up: alpha(dt, attack),
            down: alpha(dt, release),
        }
    }
    fn apply(self, state: &mut f32, target: f32) {
        *state += (target - *state) * if target > *state { self.up } else { self.down };
    }
}

/// FFT réelle fenêtrée des `n` échantillons les plus récents ; `power` = puissance par case,
/// normalisée (sinusoïde pleine échelle = 1) et compensée en pente (+3 dB/octave).
struct Fft {
    window: Vec<f32>,
    frame: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    gain: f32,
    tilt: Vec<f32>,
    power: Vec<f32>,
    hz_per_bin: f32,
}

impl Fft {
    fn new(planner: &mut RealFftPlanner<f32>, n: usize, sample_rate: f32) -> Self {
        let bins = n / 2 + 1;
        let hz_per_bin = sample_rate / n as f32;
        let window: Vec<f32> = (0..n)
            .map(|i| 0.54 - 0.46 * (2.0 * PI * i as f32 / (n - 1) as f32).cos())
            .collect();
        let amp = 2.0 / window.iter().sum::<f32>();
        let fft = planner.plan_fft_forward(n);
        Self {
            frame: vec![0.0; n],
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            gain: amp * amp,
            tilt: (0..bins)
                .map(|k| (k as f32 * hz_per_bin).max(F_MIN) / 1000.0)
                .collect(),
            power: vec![0.0; bins],
            hz_per_bin,
            window,
        }
    }

    fn bin(&self, f: f32) -> usize {
        ((f / self.hz_per_bin).round() as usize).min(self.power.len() - 1)
    }

    fn bins(&self, (lo, hi): (f32, f32)) -> (usize, usize) {
        (self.bin(lo).max(1), self.bin(hi))
    }

    /// Analyse les `n` derniers échantillons du tampon circulaire `ring` (fin en `pos`).
    fn process(&mut self, ring: &[f32], pos: usize) {
        let (len, n) = (ring.len(), self.frame.len());
        let start = (pos + len - n) % len;
        for (i, (f, w)) in self.frame.iter_mut().zip(&self.window).enumerate() {
            *f = ring[(start + i) % len] * w;
        }
        if self
            .fft
            .process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch)
            .is_err()
        {
            return; // tailles fixes : n'arrive pas
        }
        for ((p, c), t) in self.power.iter_mut().zip(&self.spectrum).zip(&self.tilt) {
            *p = c.norm_sqr() * self.gain * t;
        }
    }

    /// Amplitude moyenne (racine de la puissance moyenne) d'une plage de cases.
    fn range_amp(&self, (k0, k1): (usize, usize)) -> f32 {
        if k1 < k0 {
            return 0.0;
        }
        let p = &self.power[k0..=k1];
        (p.iter().sum::<f32>() / p.len() as f32).sqrt()
    }
}

/// Filtre triangulaire d'une bande mel : poids normalisés (somme 1) à partir de `start`.
struct MelFilter {
    start: usize,
    weights: Vec<f32>,
}

fn mel_filters(fft: &Fft, bands: usize, f_max: f32) -> Vec<MelFilter> {
    let (m_lo, m_hi) = (mel(F_MIN), mel(f_max));
    let edges: Vec<f32> = (0..bands + 2)
        .map(|i| mel_inv(m_lo + (m_hi - m_lo) * i as f32 / (bands + 1) as f32))
        .collect();
    (0..bands)
        .map(|b| {
            let (l, c, r) = (edges[b], edges[b + 1], edges[b + 2]);
            let (k0, k1) = (fft.bin(l), fft.bin(r));
            let mut weights: Vec<f32> = (k0..=k1)
                .map(|k| {
                    let f = k as f32 * fft.hz_per_bin;
                    if f <= c {
                        (f - l) / (c - l)
                    } else {
                        (r - f) / (r - c)
                    }
                    .max(0.0)
                })
                .collect();
            let sum: f32 = weights.iter().sum();
            if sum > 0.0 {
                weights.iter_mut().for_each(|w| *w /= sum);
                MelFilter { start: k0, weights }
            } else {
                // Bande plus étroite qu'une case (graves) : la case la plus proche.
                MelFilter {
                    start: fft.bin(c),
                    weights: vec![1.0],
                }
            }
        })
        .collect()
}

/// Normalisation par canal (voir l'en-tête du module). Les `gated` premiers canaux sont
/// bornés par le plus fort d'entre eux ; les suivants (volume) ne le sont pas.
struct Normalizer {
    peak: Vec<f32>,
    mean: Vec<f32>,
    gated: usize,
    peak_up: f32,
    peak_down: f32,
    mean_up: f32,
    mean_down: f32,
}

impl Normalizer {
    fn new(dt: f32, channels: usize, gated: usize) -> Self {
        Self {
            peak: vec![FLOOR_DB; channels],
            mean: vec![FLOOR_DB; channels],
            gated,
            peak_up: alpha(dt, PEAK_RISE_S),
            peak_down: alpha(dt, PEAK_FALL_S),
            mean_up: alpha(dt, MEAN_RISE_S),
            mean_down: alpha(dt, MEAN_FALL_S),
        }
    }

    /// `raw` = amplitudes linéaires → `out` = valeurs cibles dans `[0,1]`.
    fn process(&mut self, raw: &[f32], out: &mut [f32], silent: bool) {
        if silent {
            out.iter_mut().for_each(|o| *o = 0.0);
            return;
        }
        for ((&a, peak), mean) in raw.iter().zip(&mut self.peak).zip(&mut self.mean) {
            let d = db(a).max(FLOOR_DB);
            if *mean <= FLOOR_DB {
                *mean = d; // premier son du canal : son habitude part de là
            }
            *peak += (d - *peak)
                * if d > *peak {
                    self.peak_up
                } else {
                    self.peak_down
                };
            *mean += (d - *mean)
                * if d > *mean {
                    self.mean_up
                } else {
                    self.mean_down
                };
        }
        let loudest = self.peak[..self.gated]
            .iter()
            .copied()
            .fold(FLOOR_DB, f32::max);
        let gate = loudest - GATE_SPAN_DB;
        for (i, (&a, o)) in raw.iter().zip(out.iter_mut()).enumerate() {
            let d = db(a);
            let floor = if i < self.gated {
                gate.max(FLOOR_DB)
            } else {
                FLOOR_DB
            };
            if d <= floor {
                *o = 0.0;
                continue;
            }
            let peak = self.peak[i].max(floor + LEVEL_RANGE_DB * 0.5);
            let mean = self.mean[i].max(floor);
            let level = (1.0 + (d - peak) / LEVEL_RANGE_DB).clamp(0.0, 1.0);
            let dev = ((d - mean + DEV_BELOW_DB) / (DEV_BELOW_DB + DEV_ABOVE_DB)).clamp(0.0, 1.0);
            // Près du plancher, la valeur s'efface progressivement (pas de bord net).
            let fade = ((d - floor) / 6.0).clamp(0.0, 1.0);
            *o = fade * (LEVEL_WEIGHT * level + (1.0 - LEVEL_WEIGHT) * dev);
        }
    }
}

/// Indices des canaux supplémentaires après les bandes mel.
const BASS: usize = 0;
const MID: usize = 1;
const TREBLE: usize = 2;
const VOLUME: usize = 3;
const EXTRA: usize = 4;

pub struct Analyzer {
    dt: f32,
    ring: Vec<f32>,
    pos: usize,
    big: Fft,
    small: Fft,
    filters: Vec<MelFilter>,
    ranges: [(usize, usize); 3],
    centroid_bins: (usize, usize),
    /// Classe de hauteur (0–11, do = 0) de chaque case du chromagramme.
    chroma_bins: (usize, usize),
    chroma_class: Vec<u8>,
    norm: Normalizer,
    raw: Vec<f32>,
    target: Vec<f32>,
    /// Valeurs lissées publiées (bandes puis canaux supplémentaires).
    value: Vec<f32>,
    smooth: Asym,
    smooth_volume: Asym,
    gain: f32,
    gain_down: f32,
    quiet_s: f32,
    silent: bool,
    /// Montée des graves : renfort du signal d'attaques du tempo.
    kick_flux: Flux,
    hihat_flux: Flux,
    oss_flux: Flux,
    /// Grosse caisse et caisse claire (détecteur appris), et leurs enveloppes.
    drums: crate::kick::Analyzer,
    kick: Hits,
    snare: Hits,
    hihat: Onset,
    tempo: Tempo,
    beats: u32,
    loud_short: f32,
    loud_long: f32,
    loud_warm_s: f32,
    intensity: f32,
    since_low_s: f32,
    since_drop_s: f32,
    drops: u32,
    centroid: f32,
    chroma: [f32; 12],
    tone: f32,
    tone_strength: f32,
}

/// Coups du détecteur appris vus d'une image à l'autre, et leur enveloppe (saute à la force du
/// coup, retombe exponentiellement).
struct Hits {
    seen: u32,
    decay: f32,
    envelope: f32,
}

impl Hits {
    fn new(dt: f32, decay_s: f32) -> Self {
        Self {
            seen: 0,
            decay: (-dt / decay_s).exp(),
            envelope: 0.0,
        }
    }

    /// Une image : `count` et `force` sont le compteur et la force du dernier coup du
    /// détecteur. Renvoie vrai si un coup est tombé depuis l'image précédente.
    fn update(&mut self, count: u32, force: f32) -> bool {
        let hit = count != self.seen;
        self.seen = count;
        self.envelope *= self.decay;
        if hit {
            self.envelope = self.envelope.max(force);
        }
        hit
    }
}

impl Analyzer {
    /// `sample_rate` en Hz, `bands` bandes mel, `rate` analyses par seconde.
    pub fn new(sample_rate: u32, bands: usize, rate: f32) -> Self {
        let sr = sample_rate as f32;
        let mut planner = RealFftPlanner::<f32>::new();
        let n = ((sr * WINDOW_S) as usize).next_power_of_two().max(256);
        let n_small = ((sr * ONSET_WINDOW_S) as usize)
            .next_power_of_two()
            .clamp(128, n);
        let big = Fft::new(&mut planner, n, sr);
        let small = Fft::new(&mut planner, n_small, sr);
        let bands = bands.max(1);
        let f_max = F_MAX.min(sr * 0.48);
        let filters = mel_filters(&big, bands, f_max);
        let ranges = [
            (big.bin(F_MIN).max(1), big.bin(BASS_MAX)),
            (big.bin(BASS_MAX) + 1, big.bin(TREBLE_MIN)),
            (big.bin(TREBLE_MIN) + 1, big.bin(f_max)),
        ];
        let chroma_bins = big.bins(CHROMA);
        let chroma_class = (chroma_bins.0..=chroma_bins.1)
            .map(|k| {
                let f = k as f32 * big.hz_per_bin;
                // La (440 Hz) = classe 9.
                ((12.0 * (f / 440.0).log2()).round() as i32 + 9).rem_euclid(12) as u8
            })
            .collect();
        let dt = 1.0 / rate.max(1.0);
        let hihat_hi = HIHAT.1.min(f_max);
        let channels = bands + EXTRA;
        Self {
            dt,
            ring: vec![0.0; n],
            pos: 0,
            // Graves : compression faible, seules les hausses franches comptent (pas les fuites
            // d'une attaque d'aigus). Aigus : forte, ils sont faibles dans un mix.
            kick_flux: Flux::new(big.bins(KICK), 10.0),
            hihat_flux: Flux::new(small.bins((HIHAT.0.min(hihat_hi * 0.6), hihat_hi)), 100.0),
            oss_flux: Flux::new(small.bins((F_MIN, OSS_MAX.min(f_max))), 100.0),
            centroid_bins: big.bins(CENTROID),
            chroma_bins,
            chroma_class,
            big,
            small,
            filters,
            ranges,
            norm: Normalizer::new(dt, channels, bands + 3),
            raw: vec![0.0; channels],
            target: vec![0.0; channels],
            value: vec![0.0; channels],
            smooth: Asym::new(dt, ATTACK_S, RELEASE_S),
            smooth_volume: Asym::new(dt, ATTACK_S, VOLUME_RELEASE_S),
            gain: GAIN_FLOOR,
            gain_down: alpha(dt, GAIN_FALL_S),
            quiet_s: SILENT_HOLD_S,
            silent: true,
            drums: crate::kick::Analyzer::new(sample_rate),
            kick: Hits::new(dt, 0.14),
            snare: Hits::new(dt, 0.10),
            hihat: Onset::new(dt, 0.75, 2.0, 0.05, 0.06, 0.06),
            tempo: Tempo::new(rate),
            beats: 0,
            loud_short: FLOOR_DB,
            loud_long: FLOOR_DB,
            loud_warm_s: 0.0,
            intensity: 0.5,
            since_low_s: DROP_WINDOW_S,
            since_drop_s: DROP_MIN_GAP_S,
            drops: 0,
            centroid: 0.5,
            chroma: [0.0; 12],
            tone: 0.0,
            tone_strength: 0.0,
        }
    }

    /// Ajoute des échantillons mono à la fenêtre glissante.
    pub fn push(&mut self, samples: &[f32]) {
        self.drums.push(samples);
        let n = self.ring.len();
        let mut s = samples;
        if s.len() > n {
            s = &s[s.len() - n..];
        }
        let first = (n - self.pos).min(s.len());
        self.ring[self.pos..self.pos + first].copy_from_slice(&s[..first]);
        self.ring[..s.len() - first].copy_from_slice(&s[first..]);
        self.pos = (self.pos + s.len()) % n;
    }

    /// Ajoute `count` échantillons nuls (pas de paquet WASAPI : rien n'est joué).
    pub fn push_silence(&mut self, count: usize) {
        self.drums.push_silence(count);
        let n = self.ring.len();
        for _ in 0..count.min(n) {
            self.ring[self.pos] = 0.0;
            self.pos = (self.pos + 1) % n;
        }
    }

    /// Une image d'analyse : met à jour `out` (sans réallouer après la première fois).
    pub fn analyze(&mut self, out: &mut AudioFeatures) {
        let n = self.ring.len();
        let rms = (self.ring.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
        self.big.process(&self.ring, self.pos);
        self.small.process(&self.ring, self.pos);
        self.update_silence(rms);
        let silent = self.silent;
        let bands = self.filters.len();

        // Amplitudes brutes : bandes mel, plages, volume.
        let mut peak = 0.0f32;
        for (f, raw) in self.filters.iter().zip(self.raw.iter_mut()) {
            let p = &self.big.power[f.start..f.start + f.weights.len()];
            *raw = p
                .iter()
                .zip(&f.weights)
                .map(|(p, w)| p * w)
                .sum::<f32>()
                .sqrt();
            peak = peak.max(*raw);
        }
        for (i, r) in self.ranges.iter().enumerate() {
            self.raw[bands + i] = self.big.range_amp(*r);
        }
        self.raw[bands + VOLUME] = rms;
        self.norm.process(&self.raw, &mut self.target, silent);
        for (i, (v, t)) in self.value.iter_mut().zip(&self.target).enumerate() {
            let s = if i == bands + VOLUME {
                self.smooth_volume
            } else {
                self.smooth
            };
            s.apply(v, *t);
        }

        // Gain de référence des flux : suit le plafond global (montée immédiate).
        if !silent {
            self.gain = if peak > self.gain {
                peak
            } else {
                self.gain + (peak - self.gain) * self.gain_down
            }
            .max(GAIN_FLOOR);
        }
        let dt = self.dt;
        let gain = self.gain;
        let kick_flux = self.kick_flux.update(&self.big.power, gain, silent);
        let kick = self.kick.update(self.drums.kicks, self.drums.kick_force);
        self.snare.update(self.drums.caisses, self.drums.caisse_force);
        let hihat_flux = self.hihat_flux.update(&self.small.power, gain, silent);
        self.hihat.update(hihat_flux, dt, silent);
        // Signal du tempo : flux global + graves renforcés (sinon les charlestons, plus
        // nombreux, poussent vers la croche plutôt que vers le temps) + une impulsion à chaque
        // grosse caisse reconnue, qui cale le peigne de la phase sur les vrais kicks.
        let hit = if kick { self.drums.kick_force } else { 0.0 };
        let oss = self.oss_flux.update(&self.small.power, gain, silent)
            + OSS_KICK_WEIGHT * kick_flux
            + OSS_HIT_WEIGHT * hit;
        // Grille du tempo si elle est fiable, sinon les grosses caisses font office de temps.
        let grid_beat = self.tempo.push(oss, kick, silent);
        if (self.tempo.locked() && grid_beat) || (!self.tempo.locked() && kick) {
            self.beats = self.beats.wrapping_add(1);
        }

        self.update_intensity(rms, silent);
        self.update_timbre(silent);

        out.bands.clear(); // garde la capacité : pas de réallocation
        out.bands.extend_from_slice(&self.value[..bands]);
        out.bass = self.value[bands + BASS];
        out.mid = self.value[bands + MID];
        out.treble = self.value[bands + TREBLE];
        out.volume = self.value[bands + VOLUME];
        out.kick = self.kick.envelope;
        out.snare = self.snare.envelope;
        out.hihat = self.hihat.envelope;
        out.kicks = self.drums.kicks;
        out.snares = self.drums.caisses;
        out.hihats = self.hihat.count_total;
        out.beats = self.beats;
        out.bpm = self.tempo.bpm();
        out.beat_phase = self.tempo.phase();
        out.tempo_confidence = self.tempo.confidence();
        out.intensity = self.intensity;
        out.drops = self.drops;
        out.centroid = self.centroid;
        out.tone = self.tone;
        out.tone_strength = self.tone_strength;
        out.silent = silent;
        out.kick_hits = self.drums.kicks;
        out.kick_bass = self.drums.bass;
    }

    /// Silence avec hystérésis.
    fn update_silence(&mut self, rms: f32) {
        if rms >= SILENT_EXIT {
            self.quiet_s = 0.0;
            self.silent = false;
        } else if rms < SILENT_ENTER {
            self.quiet_s += self.dt;
            if self.quiet_s >= SILENT_HOLD_S {
                self.silent = true;
            }
        }
    }

    /// Intensité (volume court vs long) et détection des drops.
    fn update_intensity(&mut self, rms: f32, silent: bool) {
        let dt = self.dt;
        self.since_low_s += dt;
        self.since_drop_s += dt;
        if silent {
            // Le morceau suivant aura son propre niveau de référence.
            self.loud_warm_s = 0.0;
            self.intensity += (0.5 - self.intensity) * alpha(dt, 1.0);
            return;
        }
        let d = db(rms).max(FLOOR_DB);
        if self.loud_warm_s == 0.0 {
            self.loud_short = d;
        }
        self.loud_short += (d - self.loud_short) * alpha(dt, LOUD_SHORT_S);
        if self.loud_warm_s < LOUD_WARMUP_S {
            self.loud_warm_s += dt;
            self.loud_long = self.loud_short;
        } else {
            self.loud_long += (d - self.loud_long) * alpha(dt, LOUD_LONG_S);
        }
        self.intensity =
            (0.5 + (self.loud_short - self.loud_long) / (2.0 * INTENSITY_DB)).clamp(0.0, 1.0);
        if self.intensity < DROP_LOW {
            self.since_low_s = 0.0;
        } else if self.intensity > DROP_HIGH
            && self.since_low_s < DROP_WINDOW_S
            && self.since_drop_s > DROP_MIN_GAP_S
        {
            self.since_drop_s = 0.0;
            self.since_low_s = DROP_WINDOW_S;
            self.drops = self.drops.wrapping_add(1);
        }
    }

    /// Centroïde spectral et chromagramme (note dominante).
    fn update_timbre(&mut self, silent: bool) {
        if silent {
            return;
        }
        let (k0, k1) = self.centroid_bins;
        let (mut num, mut den) = (0.0f32, 0.0f32);
        for k in k0..=k1 {
            let p = self.big.power[k];
            num += p * (k as f32 * self.big.hz_per_bin).log2();
            den += p;
        }
        if den > 0.0 {
            let (lo, hi) = (CENTROID.0.log2(), CENTROID.1.log2());
            let c = ((num / den - lo) / (hi - lo)).clamp(0.0, 1.0);
            self.centroid += (c - self.centroid) * alpha(self.dt, CENTROID_S);
        }

        let k = alpha(self.dt, CHROMA_S);
        let mut frame = [0.0f32; 12];
        let (c0, _) = self.chroma_bins;
        for (i, &class) in self.chroma_class.iter().enumerate() {
            frame[class as usize] += self.big.power[c0 + i].sqrt();
        }
        let total: f32 = frame.iter().sum();
        if total > 0.0 {
            for (c, f) in self.chroma.iter_mut().zip(frame) {
                *c += (f / total - *c) * k;
            }
        }
        // Moyenne circulaire sur le cycle des quintes : la teinte glisse d'une tonalité à sa
        // voisine ; la longueur du vecteur résultant dit si une tonalité se dégage.
        let (mut x, mut y) = (0.0f32, 0.0f32);
        for (class, &c) in self.chroma.iter().enumerate() {
            let a = 2.0 * PI * ((class * 7) % 12) as f32 / 12.0;
            x += c * a.cos();
            y += c * a.sin();
        }
        let len = (x * x + y * y).sqrt();
        if len > 1e-6 {
            self.tone = (y.atan2(x) / (2.0 * PI)).rem_euclid(1.0);
        }
        self.tone_strength = (len * 2.0).clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    const RATE: f32 = 60.0;
    const HOP: usize = (SR as f32 / RATE) as usize;

    /// Fait tourner l'analyse sur `secs` secondes du signal `f(t)` ; renvoie la dernière
    /// image, les instants des grosses caisses et ceux des temps.
    fn run(secs: f32, f: impl Fn(f32) -> f32) -> (AudioFeatures, Vec<f32>, Vec<f32>) {
        let mut a = Analyzer::new(SR, 32, RATE);
        let mut out = AudioFeatures::default();
        let (mut kicks, mut beats) = (Vec::new(), Vec::new());
        let (mut last_k, mut last_b) = (0, 0);
        let mut buf = vec![0.0; HOP];
        let frames = (secs * RATE) as usize;
        for i in 0..frames {
            for (j, s) in buf.iter_mut().enumerate() {
                *s = f((i * HOP + j) as f32 / SR as f32);
            }
            a.push(&buf);
            a.analyze(&mut out);
            let t = (i + 1) as f32 * HOP as f32 / SR as f32;
            if out.kicks != last_k {
                kicks.push(t);
            }
            if out.beats != last_b {
                beats.push(t);
            }
            (last_k, last_b) = (out.kicks, out.beats);
        }
        (out, kicks, beats)
    }

    fn sine(freq: f32, amp: f32) -> impl Fn(f32) -> f32 {
        move |t| amp * (2.0 * PI * freq * t).sin()
    }

    /// Grosse caisse synthétique (sinus 55 Hz amorti) toutes les `period` s.
    fn kick(t: f32, period: f32) -> f32 {
        let tk = t % period;
        0.8 * (-tk / 0.08).exp() * (2.0 * PI * 55.0 * tk).sin()
    }

    #[test]
    fn sinus_60hz_graves_seules() {
        let (o, _, _) = run(1.5, sine(60.0, 0.3));
        assert!(!o.silent);
        assert!(o.bass > 0.3, "bass = {}", o.bass);
        assert!(
            o.mid < 0.05 && o.treble < 0.05,
            "mid {} treble {}",
            o.mid,
            o.treble
        );
        assert!(o.bands[0] > 0.3 && o.bands[31] < 0.01, "{:?}", o.bands);
    }

    #[test]
    fn sinus_5khz_aigus_seuls() {
        let (o, _, _) = run(1.5, sine(5_000.0, 0.3));
        assert!(o.treble > 0.3, "treble = {}", o.treble);
        assert!(o.bass < 0.01, "bass {}", o.bass);
        let max = o.bands.iter().copied().fold(0.0, f32::max);
        let argmax = o.bands.iter().position(|&b| b == max).unwrap();
        assert!(argmax >= 18, "bande max {argmax}: {:?}", o.bands);
    }

    #[test]
    fn un_son_stable_reste_discret_ses_pointes_ressortent() {
        // Nappe constante puis +12 dB : la bande saute, puis revient vers son niveau habituel.
        let (steady, _, _) = run(4.0, sine(440.0, 0.05));
        let (burst, _, _) = run(
            4.15,
            |t| if t < 4.0 { 0.05 } else { 0.2 } * (2.0 * PI * 440.0 * t).sin(),
        );
        let b = |o: &AudioFeatures| o.bands.iter().copied().fold(0.0, f32::max);
        assert!(b(&steady) < 0.6, "nappe stable : {}", b(&steady));
        assert!(b(&burst) > 0.9, "pointe : {}", b(&burst));
    }

    #[test]
    fn silence() {
        let (o, kicks, _) = run(1.0, |_| 0.0);
        assert!(o.silent);
        assert!(kicks.is_empty());
        assert_eq!(o.volume, 0.0);
        assert!(o.bands.iter().all(|&b| b == 0.0));
        // Un bruit très faible (−70 dBFS) reste « silence » : pas d'amplification du bruit.
        let (o, _, _) = run(1.0, sine(1_000.0, 3e-4));
        assert!(o.silent);
    }

    #[test]
    fn retour_au_silence() {
        let (o, _, _) = run(2.0, |t| {
            if t < 1.0 {
                0.3 * (2.0 * PI * 200.0 * t).sin()
            } else {
                0.0
            }
        });
        assert!(o.silent && o.bass < 0.01 && o.volume < 0.05);
    }

    #[test]
    fn grosses_caisses_a_la_bonne_periode() {
        let period = 0.5;
        let (_, kicks, _) = run(6.0, move |t| {
            kick(t, period) + 0.05 * (2.0 * PI * 880.0 * t).sin()
        });
        assert!(
            (10..=13).contains(&kicks.len()),
            "grosses caisses : {kicks:?}"
        );
        for w in kicks.windows(2).skip(1) {
            assert!(((w[1] - w[0]) - period).abs() < 0.04, "{kicks:?}");
        }
    }

    #[test]
    fn tempo_et_grille_reguliere_meme_avec_des_trous() {
        // 125 BPM ; une grosse caisse sur 8 manque : la grille, elle, ne saute pas de temps.
        let period = 60.0 / 125.0;
        let (o, _, beats) = run(12.0, move |t| {
            let n = (t / period) as u32;
            let k = if n % 8 == 5 { 0.0 } else { kick(t, period) };
            k + 0.03 * (2.0 * PI * 660.0 * t).sin()
        });
        assert!(
            (o.bpm - 125.0).abs() < 4.0,
            "bpm {} (confiance {})",
            o.bpm,
            o.tempo_confidence
        );
        let late: Vec<f32> = beats.into_iter().filter(|&b| b > 6.0).collect();
        for w in late.windows(2) {
            assert!(((w[1] - w[0]) - period).abs() < 0.05, "{late:?}");
        }
        assert!(late.len() >= 11, "{late:?}");
    }

    #[test]
    fn charleston_detecte_dans_les_aigus() {
        // Bruit pseudo-aléatoire filtré grossièrement (dérivée) en salves de 30 ms toutes les 0,25 s.
        let (o, _, _) = run(3.0, |t| {
            let burst = (t % 0.25) < 0.03;
            let i = (t * SR as f32) as u32;
            let n = ((i.wrapping_mul(1_103_515_245).wrapping_add(12_345) >> 8) & 0xffff) as f32
                / 32_768.0
                - 1.0;
            let m = (((i + 1).wrapping_mul(1_103_515_245).wrapping_add(12_345) >> 8) & 0xffff)
                as f32
                / 32_768.0
                - 1.0;
            let hiss = if burst { 0.3 * (n - m) } else { 0.0 };
            hiss + 0.05 * (2.0 * PI * 110.0 * t).sin()
        });
        assert!(o.hihats >= 8, "charleston : {}", o.hihats);
        // La toute première salve, avant que le seuil ait un historique, peut passer.
        assert!(o.kicks <= 1, "grosses caisses dans des aigus : {}", o.kicks);
    }

    #[test]
    fn la_note_dominante_suit_la_hauteur() {
        let (la, _, _) = run(3.0, sine(440.0, 0.2));
        let (mi, _, _) = run(3.0, sine(659.25, 0.2));
        // Cycle des quintes : la = 9 → 9·7 mod 12 = 3 ; mi = 4 → 4·7 mod 12 = 4 (voisins).
        assert!((la.tone - 3.0 / 12.0).abs() < 0.03, "la → {}", la.tone);
        assert!((mi.tone - 4.0 / 12.0).abs() < 0.03, "mi → {}", mi.tone);
        assert!(la.tone_strength > 0.5, "netteté {}", la.tone_strength);
    }

    #[test]
    fn fenetre_glissante() {
        let mut a = Analyzer::new(SR, 8, RATE);
        let n = a.ring.len();
        a.push(&vec![1.0; n + 10]);
        assert!(a.ring.iter().all(|&v| v == 1.0));
        a.push_silence(5);
        assert_eq!(a.ring.iter().filter(|&&v| v == 0.0).count(), 5);
    }
}
