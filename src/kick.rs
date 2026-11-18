//! Analyse du son : kick (grosse caisse), niveau de basse et hauteur de la note de basse.
//!
//! Méthode (voir README, section « Kick ») :
//! - kick : le son est découpé en 5 bandes (sub sous 90 Hz, 90–200 Hz, 200 Hz–1 kHz, 1–4 kHz,
//!   au-delà ; biquads, pas de FFT) dont on suit le niveau en dB tous les 5 ms. Une attaque
//!   candidate est un pic de la montée d'énergie toutes bandes confondues (connu 5 ms après
//!   l'attaque). Elle est ensuite jugée par un petit réseau de neurones (une couche cachée de 16)
//!   sur la forme de l'attaque : montée de chaque bande, niveau par rapport au morceau, équilibre
//!   du spectre, excès du sub sur sa moyenne lente. Trois réseaux décident 10, 20 et 30 ms après
//!   l'attaque : un kick franc part tout de suite, un cas douteux attend d'en entendre plus.
//!   Sans kick depuis 4 s (intro, montée, après une pause), le premier doit être franc.
//!   Les poids (`kick_modele.rs`) sont appris par `scripts/kick_modele.py` sur des morceaux réels
//!   annotés (MDB Drums, et des morceaux dont la batterie est transcrite par ADTOF).
//!   Pourquoi pas seulement le sub : en musique électronique, l'attaque du kick est dans les
//!   médiums et les aigus et le sub n'arrive que 50 à 100 ms plus tard (il commence même par se
//!   creuser, à cause du sidechain) ; un détecteur de sub y est en retard et confond la basse.
//! - basse : niveau du grave en dB, rapporté au pic récent du morceau (plage fixe de 20 dB).
//! - hauteur : autocorrélation du grave sous-échantillonné à ~1 kHz, entre 25 et 83 Hz.
//!
//! Tout est causal et tourne échantillon par échantillon ; les décisions sont prises tous les
//! `HOP_S` (5 ms).

use crate::kick_modele as m;
use std::collections::VecDeque;

const HOP_S: f64 = 0.005;
const BASS_WIN_S: f64 = 0.0107; // ~512 échantillons à 48 kHz
const PITCH_RATE: f64 = 1000.0;
const PITCH_N: usize = 48; // fenêtre d'autocorrélation : 48 ms
const LAG_MIN: usize = 12; // 83 Hz
const LAG_MAX: usize = 40; // 25 Hz

/// Bandes du kick : (passe-haut, passe-bas) en Hz, chacun d'ordre 4 (deux biquads).
const BANDS: [(Option<f64>, Option<f64>); 5] = [
    (None, Some(90.0)),
    (Some(90.0), Some(200.0)),
    (Some(200.0), Some(1000.0)),
    (Some(1000.0), Some(4000.0)),
    (Some(4000.0), None),
];
/// Pas (5 ms) entre l'attaque et chaque décision.
const STAGES: [usize; 3] = [2, 4, 6];
/// Seuils de chaque décision à sensibilité 5, en logit (ln p/(1-p)) : probabilité 0,8, 0,7 et
/// 0,5. Une décision précoce doit être plus sûre d'elle.
const STAGE_LOGIT: [f64; 3] = [1.386, 0.847, 0.0];
/// Effet d'un cran de sensibilité sur les seuils (logit) : de 0,73 (1) à 0,22 (10) au dernier.
const SENS_STEP: f64 = 0.25;
/// Sans kick depuis `QUIET_S`, le premier doit être franc : seuils relevés de `QUIET_RAISE`
/// (logit). Écarte une voix grave ou une basse en intro, une caisse claire seule dans une montée ;
/// mesuré sur le banc : 70 % de faux kicks d'intro en moins, F1 inchangé (−0,003).
const QUIET_S: f64 = 4.0;
const QUIET_RAISE: f64 = 1.5;
/// Fenêtre (en pas) où l'on cherche le niveau d'avant l'attaque : de `p - PRE` à `p - 2`.
const PRE: u64 = 8;
/// Images gardées en mémoire : assez pour remonter de la dernière décision au début de `PRE`.
const HIST: usize = 16;
/// Pic récent des médiums pour le rapport sub / médiums : 120 ms.
const MID_WIN: usize = 25;
/// Références de départ (niveaux médians d'un morceau, en dB par bande ; pic du flux ; pic de
/// l'excès du sub) : au lancement, rien n'a encore été entendu pour juger du niveau.
const START_PEAK: [f64; 5] = [-8.0, -11.0, -12.0, -16.0, -18.0];
const START_FLUX_PK: f64 = 35.0;
const START_SUB_EXC_PK: f64 = 0.015;
/// En dessous (dB, toutes bandes), c'est le silence.
const SILENCE_DB: f64 = -60.0;

#[derive(Clone, Copy)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn new(sr: f64, f0: f64, high: bool) -> Self {
        // Formules RBJ, Q = 0,707 (Butterworth).
        let w = std::f64::consts::TAU * f0 / sr;
        let (c, a) = (w.cos(), w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2));
        let a0 = 1.0 + a;
        let (b0, b1, b2) = if high {
            ((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0)
        } else {
            ((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0)
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - a) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }
    pub fn lowpass(sr: f64, f0: f64) -> Self {
        Self::new(sr, f0, false)
    }
    pub fn highpass(sr: f64, f0: f64) -> Self {
        Self::new(sr, f0, true)
    }
    #[inline]
    pub fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Énergie moyenne glissante (moyenne des carrés sur une fenêtre fixe).
struct MeanSquare {
    ring: Vec<f64>,
    pos: usize,
    sum: f64,
    since_resync: usize,
}

impl MeanSquare {
    fn new(len: usize) -> Self {
        Self { ring: vec![0.0; len.max(1)], pos: 0, sum: 0.0, since_resync: 0 }
    }
    #[inline]
    fn push(&mut self, x: f64) {
        let sq = x * x;
        self.sum += sq - self.ring[self.pos];
        self.ring[self.pos] = sq;
        self.pos = (self.pos + 1) % self.ring.len();
        self.since_resync += 1;
        if self.since_resync >= 1 << 16 {
            // recalcul exact de temps en temps : pas de dérive de la somme glissante
            self.sum = self.ring.iter().sum();
            self.since_resync = 0;
        }
    }
    fn value(&self) -> f64 {
        (self.sum / self.ring.len() as f64).max(0.0)
    }
}

/// Une bande du kick : ses filtres et l'énergie accumulée du pas en cours.
struct Band {
    filters: Vec<Biquad>,
    acc: f64,
    prev_block: Option<f64>,
}

/// Mesures d'un pas de 5 ms.
#[derive(Clone, Copy, Default)]
struct Frame {
    /// Niveau de chaque bande (dB, fenêtre 10 ms).
    l: [f64; 5],
    /// Montée d'énergie toutes bandes (somme des montées en dB sur 10 ms).
    flux: f64,
    /// Pic récent du flux (retombe en ~2 s).
    flux_pk: f64,
    /// Pic de chaque bande dans le morceau (retombe de 2 dB/s).
    peak: [f64; 5],
    /// Niveau toutes bandes (dB).
    tot: f64,
    /// Excès du sub sur sa moyenne lente, rapporté à son pic récent (0..1).
    sub_onset: f64,
    /// Sub par rapport au pic récent des médiums (dB).
    sub_vs_mid: f64,
}

/// Attaque candidate en attente de décision : pas de l'attaque, étape de décision suivante.
struct Pending {
    hop: u64,
    stage: usize,
}

/// Petit réseau de décision (`N` entrées → 16 tanh → 1), poids dans `kick_modele.rs`.
pub(crate) struct Net<const N: usize> {
    /// Première couche, normalisation des entrées comprise.
    pub w: [[f32; N]; 16],
    pub b: [f32; 16],
    pub v: [f32; 16],
    pub c: f32,
    /// Plage des entrées vue à l'apprentissage : une entrée hors de cette plage (signal
    /// synthétique, sub muet avant l'attaque…) y est ramenée au lieu d'être extrapolée.
    pub lo: [f32; N],
    pub hi: [f32; N],
}

impl<const N: usize> Net<N> {
    /// Score du kick en logit (ln p/(1-p)) : positif = plus probable qu'improbable.
    fn logit(&self, x: &[f64]) -> f64 {
        let x: Vec<f64> = (0..N).map(|i| x[i].clamp(self.lo[i] as f64, self.hi[i] as f64)).collect();
        let mut z = self.c as f64;
        for j in 0..16 {
            let h: f64 = self.w[j].iter().zip(&x).map(|(&wi, &xi)| wi as f64 * xi).sum::<f64>() + self.b[j] as f64;
            z += self.v[j] as f64 * h.tanh();
        }
        z
    }
}

pub struct Analyzer {
    low: [Biquad; 3],
    low_e: MeanSquare,
    hop: usize,
    hop_count: usize,
    dt: f64,
    pub t: f64,
    // kick
    bands: Vec<Band>,
    frames: VecDeque<Frame>,
    /// Nombre de pas calculés (indice du prochain).
    hops: u64,
    mids: VecDeque<f64>,
    flux_pk: f64,
    peak: [f64; 5],
    sub_slow: f64,
    sub_exc_pk: f64,
    pending: Vec<Pending>,
    /// Instant de l'attaque du dernier kick.
    last_attack: f64,
    /// Délai minimal entre deux kicks (s), mesuré entre leurs attaques.
    pub min_gap_s: f64,
    last_kick: f64,
    /// Sensibilité du kick, 1 (peu sensible) à 10 (très sensible).
    pub sensitivity: f32,
    /// Nombre total de kicks détectés.
    pub kicks: u32,
    // basse
    peak_db: f64,
    bass_target: f64,
    /// Niveau de basse lissé, 0..1.
    pub bass: f32,
    // hauteur
    dec: usize,
    dec_count: usize,
    dec_rate: f64,
    pitch_buf: VecDeque<f64>,
    /// Hauteur de la dernière note de basse fiable, en Hz (0 = jamais mesurée).
    pub pitch: f32,
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f64;
        let dec = (sr / PITCH_RATE).round().max(1.0) as usize;
        let hop = (sr * HOP_S).round().max(1.0) as usize;
        let bands = BANDS
            .iter()
            .map(|&(hp, lp)| {
                let mut filters = Vec::new();
                for _ in 0..2 {
                    filters.extend(hp.map(|f| Biquad::highpass(sr, f)));
                    filters.extend(lp.map(|f| Biquad::lowpass(sr, f)));
                }
                Band { filters, acc: 0.0, prev_block: None }
            })
            .collect();
        Self {
            low: [Biquad::lowpass(sr, 90.0); 3],
            low_e: MeanSquare::new((sr * BASS_WIN_S).round() as usize),
            hop,
            hop_count: 0,
            dt: hop as f64 / sr,
            t: 0.0,
            bands,
            frames: VecDeque::with_capacity(HIST + 1),
            hops: 0,
            mids: VecDeque::with_capacity(MID_WIN + 1),
            flux_pk: START_FLUX_PK,
            peak: START_PEAK,
            sub_slow: 0.0,
            sub_exc_pk: START_SUB_EXC_PK,
            pending: Vec::new(),
            last_attack: -9.0,
            min_gap_s: 0.1,
            last_kick: -9.0,
            sensitivity: 5.0,
            kicks: 0,
            peak_db: -60.0,
            bass_target: 0.0,
            bass: 0.0,
            dec,
            dec_count: 0,
            dec_rate: sr / dec as f64,
            pitch_buf: VecDeque::with_capacity(PITCH_N + LAG_MAX + 2),
            pitch: 0.0,
        }
    }

    /// Ajoute des échantillons mono.
    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            self.sample(s as f64);
        }
    }

    /// Ajoute `n` échantillons de silence (Windows n'envoie rien quand rien ne joue).
    pub fn push_silence(&mut self, n: usize) {
        for _ in 0..n {
            self.sample(0.0);
        }
    }

    #[inline]
    fn sample(&mut self, x: f64) {
        let l = self.low.iter_mut().fold(x, |v, f| f.run(v));
        self.low_e.push(l);
        for b in &mut self.bands {
            let y = b.filters.iter_mut().fold(x, |v, f| f.run(v));
            b.acc += y * y;
        }
        self.dec_count += 1;
        if self.dec_count >= self.dec {
            self.dec_count = 0;
            if self.pitch_buf.len() == PITCH_N + LAG_MAX + 1 {
                self.pitch_buf.pop_front();
            }
            self.pitch_buf.push_back(l);
        }
        self.hop_count += 1;
        if self.hop_count >= self.hop {
            self.hop_count = 0;
            self.t += self.dt;
            self.kick_step();
            self.bass_step();
        }
    }

    /// Mesures du pas `i` (indice absolu), encore en mémoire.
    fn frame(&self, i: u64) -> &Frame {
        let first = self.hops - self.frames.len() as u64;
        &self.frames[(i - first) as usize]
    }

    fn kick_step(&mut self) {
        let dt = self.dt;
        let i = self.hops;
        let mut f = Frame::default();
        for (b, band) in self.bands.iter_mut().enumerate() {
            let block = band.acc / self.hop as f64;
            band.acc = 0.0;
            let e = band.prev_block.map_or(block, |p| (block + p) / 2.0); // fenêtre 10 ms
            band.prev_block = Some(block);
            f.l[b] = 10.0 * (e + 1e-12).log10();
        }
        if i >= 2 {
            let old = self.frame(i - 2).l;
            f.flux = (0..5).map(|b| (f.l[b] - old[b]).max(0.0)).sum();
        }
        let lin = f.l.map(|v| 10f64.powf(v / 10.0));
        f.tot = 10.0 * lin.iter().sum::<f64>().log10();
        // Les références du morceau ne retombent pas pendant un silence : après une pause, le
        // morceau suivant est jugé par rapport au précédent, pas par rapport au silence.
        let decay = f.tot >= SILENCE_DB;
        self.flux_pk = f.flux.max(if decay { self.flux_pk * (-dt / 2.0).exp() } else { self.flux_pk });
        f.flux_pk = self.flux_pk;
        for b in 0..5 {
            self.peak[b] = f.l[b].max(if decay { self.peak[b] - 2.0 * dt } else { self.peak[b] });
        }
        f.peak = self.peak;
        // excès du sub sur sa moyenne lente (350 ms), rapporté à son pic récent (3 s)
        let exc = (lin[0] - self.sub_slow).max(0.0);
        self.sub_slow += (lin[0] - self.sub_slow) * (dt / 0.35).min(1.0);
        self.sub_exc_pk = exc.max(if decay { self.sub_exc_pk * (-dt / 3.0).exp() } else { self.sub_exc_pk });
        f.sub_onset = exc / self.sub_exc_pk;
        if self.mids.len() == MID_WIN {
            self.mids.pop_front();
        }
        self.mids.push_back(lin[1] + lin[2]);
        let mid_max = self.mids.iter().fold(0.0f64, |a, &v| a.max(v));
        f.sub_vs_mid = 10.0 * (lin[0] / (mid_max + 1e-12) + 1e-9).log10();
        if self.frames.len() == HIST {
            self.frames.pop_front();
        }
        self.frames.push_back(f);
        self.hops += 1;

        // pic du flux au pas précédent : attaque candidate
        if i > PRE + 1 {
            let p = i - 1;
            let (a, prev) = (self.frame(p), self.frame(p - 1));
            if a.flux > prev.flux && a.flux >= f.flux && a.flux >= 0.15 * prev.flux_pk && a.tot > -70.0 {
                self.pending.push(Pending { hop: p, stage: 0 });
            }
        }
        self.decide(i);
    }

    /// Juge les candidats dont une étape de décision tombe sur le pas `i`.
    fn decide(&mut self, i: u64) {
        let shift = (5.0 - self.sensitivity.clamp(1.0, 10.0) as f64) * SENS_STEP;
        let mut k = 0;
        while k < self.pending.len() {
            let Pending { hop: p, stage } = self.pending[k];
            if p + STAGES[stage] as u64 != i {
                k += 1;
                continue;
            }
            let attack = p as f64 * self.dt;
            let since = attack - self.last_attack;
            let quiet = if since > QUIET_S { QUIET_RAISE } else { 0.0 };
            // La sensibilité règle le dernier seuil (combien de kicks) ; les décisions précoces
            // gardent le leur (à quel point il faut être sûr pour ne pas attendre), jamais plus bas.
            let last = STAGE_LOGIT[STAGES.len() - 1] + shift;
            let thr = if stage + 1 == STAGES.len() { last } else { STAGE_LOGIT[stage].max(last) } + quiet;
            let fires = since > self.min_gap_s.max(0.05) && self.score(p, STAGES[stage]) > thr;
            if fires {
                self.kicks = self.kicks.wrapping_add(1);
                self.last_kick = self.t;
                self.last_attack = attack;
                self.pending.swap_remove(k);
            } else if stage + 1 < STAGES.len() {
                self.pending[k].stage += 1;
                k += 1;
            } else {
                self.pending.swap_remove(k);
            }
        }
    }

    /// Score (logit) de l'attaque du pas `p` jugée `s` pas après.
    fn score(&self, p: u64, s: usize) -> f64 {
        let x = self.features(p, s);
        match s {
            2 => m::NET2.logit(&x),
            4 => m::NET4.logit(&x),
            _ => m::NET6.logit(&x),
        }
    }

    /// Entrées du réseau de l'étape `s` (dans l'ordre de `scripts/kick_modele.py`).
    fn features(&self, p: u64, s: usize) -> [f64; m::N6] {
        let now = self.frame(p + s as u64);
        let before = self.frame(p - 1);
        let mut pre = [f64::MAX; 5];
        for q in p - PRE..p - 1 {
            for (v, l) in pre.iter_mut().zip(self.frame(q).l) {
                *v = v.min(l);
            }
        }
        let mut x = [0.0; m::N6];
        let mut n = 0;
        let mut put = |v: f64| {
            x[n] = v;
            n += 1;
        };
        // montée de chaque bande, 5, 10, 20 et 30 ms après l'attaque
        for j in [1u64, 2, 4, 6].into_iter().filter(|&j| j <= s as u64) {
            let l = self.frame(p + j).l;
            (0..5).for_each(|b| put(l[b] - pre[b]));
        }
        (0..5).for_each(|b| put(now.l[b] - before.peak[b])); // niveau par rapport au morceau
        (0..5).for_each(|b| put(now.l[b] - now.tot)); // équilibre du spectre
        let loudest = before.peak.iter().fold(f64::MIN, |a, &v| a.max(v));
        put((self.frame(p).flux / before.flux_pk + 1e-3).ln());
        put(now.tot - loudest);
        put(pre[0] - before.peak[0]);
        put(pre[1] - before.peak[1]);
        put(now.sub_onset);
        put(now.sub_vs_mid);
        x
    }

    fn bass_step(&mut self) {
        let (t, dt) = (self.t, self.dt);
        let e = self.low_e.value();
        let db = 10.0 * (e + 1e-12).log10();
        self.peak_db = db.max(self.peak_db - dt * 1.5);
        self.bass_target = if db < -70.0 {
            0.0
        } else {
            (1.0 - (self.peak_db - db) / 20.0).clamp(0.0, 1.0).powf(1.6)
        };
        let rate = if self.bass_target > self.bass as f64 { 40.0 } else { 8.0 };
        self.bass += ((self.bass_target - self.bass as f64) * (dt * rate).min(1.0)) as f32;

        // hauteur (hors des kicks, qui occupent la même zone)
        if self.bass_target > 0.1 && t - self.last_kick > 0.15 {
            if let Some(f) = self.estimate_pitch() {
                self.pitch = f as f32;
            }
        }
    }

    /// Autocorrélation normalisée ; garde la plus petite période presque aussi bonne que la
    /// meilleure (évite de répondre une octave trop bas).
    fn estimate_pitch(&self) -> Option<f64> {
        if self.pitch_buf.len() < PITCH_N + LAG_MAX + 1 {
            return None;
        }
        let (a, b) = self.pitch_buf.as_slices();
        let buf: Vec<f64> = a.iter().chain(b).copied().collect();
        let x = &buf[buf.len() - PITCH_N - LAG_MAX - 1..];
        let base = &x[LAG_MAX + 1..]; // les PITCH_N échantillons les plus récents
        let e0: f64 = base.iter().map(|v| v * v).sum();
        if e0 < 1e-12 {
            return None;
        }
        let mut r = [0.0f64; LAG_MAX + 2];
        for (lag, rl) in r.iter_mut().enumerate().skip(LAG_MIN - 1) {
            let start = LAG_MAX + 1 - lag;
            let past = &x[start..start + PITCH_N];
            let (mut num, mut e1) = (0.0, 0.0);
            for (p, q) in base.iter().zip(past) {
                num += p * q;
                e1 += q * q;
            }
            *rl = if e1 > 0.0 { num / (e0 * e1).sqrt() } else { 0.0 };
        }
        let best = (LAG_MIN..=LAG_MAX).map(|l| r[l]).fold(f64::MIN, f64::max);
        if best < 0.6 {
            return None;
        }
        let lag = (LAG_MIN..=LAG_MAX).find(|&l| r[l] >= best * 0.92 && r[l] >= r[l - 1] && r[l] >= r[l + 1])?;
        let (ym, y0, yp) = (r[lag - 1], r[lag], r[lag + 1]);
        let den = ym - 2.0 * y0 + yp;
        let off = if den < 0.0 { 0.5 * (ym - yp) / den } else { 0.0 };
        Some(self.dec_rate / (lag as f64 + off.clamp(-0.5, 0.5)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48000;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
        }
    }

    /// 8 s à 120 BPM : caisse claire (corps à `snare_hz`) sur 2 et 4, charleston en croches,
    /// kicks sur chaque temps à partir de 4 s, basse tenue optionnelle (attaque douce).
    fn render(snare_hz: f64, bass: bool) -> Vec<f32> {
        let n = SR as usize * 8;
        let mut rng = Rng(1);
        let mut ph = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / SR as f64;
                let (beat, bar) = (t % 0.5, t % 1.0);
                let mut v = rng.next() * 0.01;
                let ts = (bar + 0.5) % 1.0;
                if ts < 0.2 {
                    v += (rng.next() * 0.5 + (std::f64::consts::TAU * snare_hz * ts).sin() * 0.6) * (-ts * 25.0).exp();
                }
                let th = t % 0.25;
                if th < 0.05 {
                    v += rng.next() * 0.15 * (-th * 80.0).exp();
                }
                if t >= 4.0 && beat < 0.25 {
                    let phase = 50.0 * beat + 60.0 * (1.0 - (-beat * 30.0).exp()) / 30.0;
                    v += (std::f64::consts::TAU * phase).sin() * 0.9 * (-beat * 12.0).exp();
                }
                if bass {
                    let tn = t % 0.75;
                    let f = [55.0, 41.0, 49.0, 62.0][(t / 0.75) as usize % 4];
                    ph += std::f64::consts::TAU * f / SR as f64;
                    v += ph.sin() * 0.3 * (tn / 0.05).min(1.0) * (-tn * 1.2).exp();
                }
                v as f32
            })
            .collect()
    }

    /// Kicks longs et « vibrants » (type 808) : sinus grave qui descend lentement, avec une
    /// ondulation d'amplitude, un par temps à 120 BPM pendant 6 s, sur fond de charleston.
    fn render_long_kicks(wobble_hz: f64) -> Vec<f32> {
        let n = SR as usize * 6;
        let mut rng = Rng(7);
        (0..n)
            .map(|i| {
                let t = i as f64 / SR as f64;
                let beat = t % 0.5;
                let mut v = rng.next() * 0.01;
                let th = t % 0.25;
                if th < 0.05 {
                    v += rng.next() * 0.15 * (-th * 80.0).exp();
                }
                let phase = 45.0 * beat + 40.0 * (1.0 - (-beat * 20.0).exp()) / 20.0;
                let wobble = 0.6 + 0.4 * (std::f64::consts::TAU * wobble_hz * beat).cos();
                v += (std::f64::consts::TAU * phase).sin() * 0.9 * (-beat * 2.5).exp() * wobble;
                v as f32
            })
            .collect()
    }

    #[test]
    fn kick_long_compte_une_seule_fois() {
        for wobble in [6.0, 9.0, 14.0] {
            for sens in [1.0, 5.0, 10.0] {
                let hits = kick_times(&render_long_kicks(wobble), sens);
                // temps de 1,0 à 5,5 s (10 temps). La première seconde est ignorée : les
                // moyennes s'y calent sur le morceau, et à sensibilité 10 un kick long peut y
                // être compté deux fois.
                let counted = hits.iter().filter(|&&t| t > 0.95).count();
                assert_eq!(counted, 10, "ondulation {wobble} Hz, sens {sens} : {counted} kicks pour 10 temps ({hits:?})");
            }
        }
    }

    /// Instants (s) où le compteur de kicks augmente.
    fn kick_times(sig: &[f32], sens: f32) -> Vec<f64> {
        let mut a = Analyzer::new(SR);
        a.sensitivity = sens;
        let mut out = Vec::new();
        for chunk in sig.chunks(480) {
            let before = a.kicks;
            a.push(chunk);
            if a.kicks != before {
                out.push(a.t);
            }
        }
        out
    }

    #[test]
    fn caisse_claire_seule_ne_donne_aucun_kick() {
        for snare in [140.0, 190.0, 230.0] {
            for sens in [1.0, 5.0, 10.0] {
                let sig = render(snare, false);
                let early: Vec<_> = kick_times(&sig, sens).into_iter().filter(|&t| t < 4.0).collect();
                assert!(early.is_empty(), "caisse {snare} Hz, sens {sens} : faux kicks {early:?}");
            }
        }
    }

    #[test]
    fn kicks_trouves_vite_avec_ou_sans_basse() {
        let expected = [4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5];
        for bass in [false, true] {
            for sens in [1.0, 5.0, 10.0] {
                let hits = kick_times(&render(190.0, bass), sens);
                let mut delays = Vec::new();
                for k in expected {
                    if let Some(h) = hits.iter().find(|&&h| h >= k && h < k + 0.1) {
                        delays.push(h - k);
                    }
                }
                assert!(delays.len() >= 7, "basse {bass}, sens {sens} : {}/8 kicks ({hits:?})", delays.len());
                let extra = hits.iter().filter(|&&h| h >= 4.0).count() - delays.len();
                assert!(extra <= 1, "basse {bass}, sens {sens} : {extra} kicks en trop ({hits:?})");
                delays.sort_by(f64::total_cmp);
                let median = delays[delays.len() / 2];
                assert!(median < 0.03, "basse {bass}, sens {sens} : délai médian {:.0} ms", median * 1000.0);
            }
        }
    }

    #[test]
    fn hauteur_de_la_note_de_basse() {
        for f in [30.0f64, 41.0, 55.0, 73.0] {
            let mut a = Analyzer::new(SR);
            let sig: Vec<f32> = (0..SR as usize)
                .map(|i| ((std::f64::consts::TAU * f * i as f64 / SR as f64).sin() * 0.3) as f32)
                .collect();
            a.push(&sig);
            assert!((a.pitch as f64 - f).abs() < f * 0.04, "{f} Hz mesuré {:.1} Hz", a.pitch);
        }
    }

    #[test]
    fn niveau_de_basse_monte_et_retombe() {
        let mut a = Analyzer::new(SR);
        a.push_silence(SR as usize / 2);
        assert!(a.bass < 0.01);
        let tone: Vec<f32> = (0..SR as usize / 2)
            .map(|i| ((std::f64::consts::TAU * 45.0 * i as f64 / SR as f64).sin() * 0.3) as f32)
            .collect();
        a.push(&tone);
        assert!(a.bass > 0.8, "basse {}", a.bass);
        a.push_silence(SR as usize);
        assert!(a.bass < 0.05, "basse après silence {}", a.bass);
    }
}
