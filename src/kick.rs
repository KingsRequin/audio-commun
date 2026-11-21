//! Analyse du son : kick (grosse caisse), caisse claire, niveau de basse et hauteur de la note
//! de basse.
//!
//! Méthode (voir README, section « Kick ») :
//! - kick : le son est découpé en 10 bandes (sous 50 Hz, 50–90, 90–140, 140–200, 200–450,
//!   450 Hz–1 kHz, 1–2 kHz, 2–4 kHz, 4–8 kHz, au-delà ; biquads, pas de FFT) dont on suit le
//!   niveau en dB tous les 5 ms ; deux bandes voisines forment une des 5 bandes larges (sub,
//!   90–200 Hz, 200 Hz–1 kHz, 1–4 kHz, aigus). Les bandes sous 4 kHz sont calculées sur le son
//!   sous-échantillonné (~12 kHz de 1 à 4 kHz, ~6 kHz en dessous) : mêmes niveaux, deux fois
//!   moins de calcul. Une attaque candidate est un pic de la montée d'énergie des bandes larges
//!   (connu 5 ms après l'attaque). Elle est jugée par un petit réseau de neurones (une couche
//!   cachée de 32) sur deux choses : la forme de l'attaque (montée de chaque bande, niveau par
//!   rapport au morceau, équilibre du spectre, excès du sub sur sa moyenne lente) et le
//!   contexte, c'est-à-dire l'état d'un petit réseau récurrent (GRU) qui lit les niveaux des
//!   bandes à chaque pas et retient ce qui précède : le rythme, les coups récents, la nature du
//!   son (musique, voix seule, basse tenue). Trois réseaux décident 10, 20 et 30 ms après
//!   l'attaque : un kick franc part tout de suite, un cas douteux attend d'en entendre plus.
//!   Sans kick depuis 4 s (intro, montée, après une pause), le premier doit être franc.
//!   Les poids (`kick_modele.rs`) sont appris par `scripts/kick_modele.py` sur des morceaux réels
//!   annotés (MDB Drums ; morceaux dont la batterie est transcrite par ADTOF), sur ces mêmes
//!   morceaux rebatterisés (accompagnement réel, batterie refaite avec des sons isolés posés à
//!   des instants connus) et sur des sons sans batterie (voix, piano, orgue, ambiances).
//!   Pourquoi pas seulement le sub : en musique électronique, l'attaque du kick est dans les
//!   médiums et les aigus et le sub n'arrive que 50 à 100 ms plus tard (il commence même par se
//!   creuser, à cause du sidechain) ; un détecteur de sub y est en retard et confond la basse.
//! - caisse claire (et claps) : mêmes candidats, mêmes mesures, même contexte, trois réseaux à
//!   elle, mêmes seuils qu'un kick à sensibilité 5 ; une attaque peut être à la fois un kick et
//!   une caisse claire (frappés ensemble).
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

/// Bandes fines : (passe-haut, passe-bas) en Hz, chacun d'ordre 4 (deux biquads). Les bandes
/// `2b` et `2b + 1` forment la bande large `b`.
const BANDS: [(Option<f64>, Option<f64>); NF] = [
    (None, Some(50.0)),
    (Some(50.0), Some(90.0)),
    (Some(90.0), Some(140.0)),
    (Some(140.0), Some(200.0)),
    (Some(200.0), Some(450.0)),
    (Some(450.0), Some(1000.0)),
    (Some(1000.0), Some(2000.0)),
    (Some(2000.0), Some(4000.0)),
    (Some(4000.0), Some(8000.0)),
    (Some(8000.0), None),
];
/// Nombre de bandes fines.
const NF: usize = 10;
/// Étage de calcul de chaque bande : 0 = cadence d'origine, 1 = sous-échantillonné de `d1`
/// (~12 kHz), 2 = encore de 2 (~6 kHz). Une bande grave n'a pas besoin de 48 000 échantillons
/// par seconde : calculée à 6 kHz, elle coûte 8 fois moins.
const BAND_STAGE: [usize; NF] = [2, 2, 2, 2, 2, 2, 1, 1, 0, 0];
/// Facteurs de qualité des trois biquads d'un passe-bas de Butterworth d'ordre 6 (avant chaque
/// sous-échantillonnage).
const Q6: [f64; 3] = [0.5176380902050415, std::f64::consts::FRAC_1_SQRT_2, 1.9318516525781366];
/// Coupure de ces passe-bas, en part de la cadence d'arrivée.
const AA1: f64 = 0.40;
const AA2: f64 = 0.37;
/// Écart minimal entre deux caisses claires (s), mesuré entre leurs attaques : un roulement de
/// 20 coups par seconde passe.
const SNARE_GAP_S: f64 = 0.05;
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
/// Seuil de silence (dB, toutes bandes) à volume normal. Il suit le volume d'écoute : `SILENCE_BELOW` dB
/// sous le plus fort pic récent, sans monter au-dessus de `SILENCE_DB` ni descendre sous `SILENCE_MIN`.
/// Sous `SILENCE_MIN`, rien ne joue : les pics ne retombent plus.
const SILENCE_DB: f64 = -60.0;
const SILENCE_BELOW: f64 = 52.0;
const SILENCE_MIN: f64 = -100.0;
/// Durée de silence (s) après laquelle le contexte est oublié.
const RESET_S: f64 = 2.0;

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
    fn new(sr: f64, f0: f64, high: bool, q: f64) -> Self {
        // Formules RBJ ; la fréquence reste sous 0,45 × la cadence (filtre stable à toute cadence).
        let w = std::f64::consts::TAU * f0.min(0.45 * sr) / sr;
        let (c, a) = (w.cos(), w.sin() / (2.0 * q));
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
    /// Passe-bas et passe-haut du second ordre, Q = 0,707 (Butterworth).
    pub fn lowpass(sr: f64, f0: f64) -> Self {
        Self::new(sr, f0, false, std::f64::consts::FRAC_1_SQRT_2)
    }
    pub fn highpass(sr: f64, f0: f64) -> Self {
        Self::new(sr, f0, true, std::f64::consts::FRAC_1_SQRT_2)
    }
    /// Passe-bas de Butterworth d'ordre 6 (trois biquads en série).
    fn lowpass6(sr: f64, f0: f64) -> [Self; 3] {
        Q6.map(|q| Self::new(sr, f0, false, q))
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

/// Une bande du kick : ses filtres (les `n` premiers servent) et l'énergie accumulée du pas en cours.
struct Band {
    filters: [Biquad; 4],
    n: usize,
    acc: f64,
    prev_block: Option<f64>,
}

impl Band {
    fn new(sr: f64, hp: Option<f64>, lp: Option<f64>) -> Self {
        let mut filters = [Biquad::lowpass(sr, 1.0); 4];
        let mut n = 0;
        for _ in 0..2 {
            for f in [hp.map(|f| Biquad::highpass(sr, f)), lp.map(|f| Biquad::lowpass(sr, f))].into_iter().flatten() {
                filters[n] = f;
                n += 1;
            }
        }
        Self { filters, n, acc: 0.0, prev_block: None }
    }
    #[inline]
    fn run(&mut self, x: f64) {
        let y = self.filters[..self.n].iter_mut().fold(x, |v, f| f.run(v));
        self.acc += y * y;
    }
}

/// Mesures d'un pas de 5 ms.
#[derive(Clone, Copy, Default)]
struct Frame {
    /// Niveau de chaque bande fine (dB, fenêtre 10 ms).
    lf: [f64; NF],
    /// Niveau de chaque bande large (dB) : somme des puissances de ses deux bandes fines.
    l: [f64; 5],
    /// Montée d'énergie toutes bandes (somme des montées en dB sur 10 ms).
    flux: f64,
    /// Pic récent du flux (retombe en ~2 s).
    flux_pk: f64,
    /// Pic de chaque bande dans le morceau (retombe de 2 dB/s).
    peak: [f64; 5],
    /// Niveau toutes bandes (dB).
    tot: f64,
    /// Seuil de silence à ce pas (dB).
    floor: f64,
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

/// Ce que reconnaît un détecteur.
#[derive(Clone, Copy, PartialEq)]
enum Coup {
    Kick,
    Caisse,
}

/// Un détecteur (kick ou caisse claire) : ses candidats en attente de décision.
struct Juge {
    coup: Coup,
    pending: Vec<Pending>,
    /// Instant de l'attaque du dernier coup reconnu.
    last_attack: f64,
}

impl Juge {
    fn new(coup: Coup) -> Self {
        Self { coup, pending: Vec::new(), last_attack: -9.0 }
    }
}

/// Petit réseau de décision (`N` mesures de l'attaque + état du contexte → `m::H` tanh → 1),
/// poids dans `kick_modele.rs`.
pub(crate) struct Net<const N: usize> {
    /// Première couche : part des mesures (normalisation comprise) et part de l'état du contexte.
    pub w: [[f32; N]; m::H],
    pub u: [[f32; m::G]; m::H],
    pub b: [f32; m::H],
    pub v: [f32; m::H],
    pub c: f32,
    /// Plage des mesures vue à l'apprentissage : une mesure hors de cette plage (signal
    /// synthétique, sub muet avant l'attaque…) y est ramenée au lieu d'être extrapolée.
    pub lo: [f32; N],
    pub hi: [f32; N],
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl<const N: usize> Net<N> {
    /// Score du coup en logit (ln p/(1-p)) : positif = plus probable qu'improbable.
    fn logit(&self, x: &[f64], state: &[f32; m::G]) -> f64 {
        let mut xs = [0f32; N];
        for (i, v) in xs.iter_mut().enumerate() {
            *v = (x[i] as f32).clamp(self.lo[i], self.hi[i]);
        }
        let mut z = self.c;
        for j in 0..m::H {
            z += self.v[j] * (dot(&self.w[j], &xs) + dot(&self.u[j], state) + self.b[j]).tanh();
        }
        z as f64
    }
}

/// Mémoire du contexte : un petit réseau récurrent (GRU) avancé à chaque pas de 5 ms sur les
/// niveaux des bandes. Son état résume ce qui précède (rythme, coups récents, nature du son) ;
/// les réseaux de décision le lisent en plus des mesures de l'attaque. Poids dans
/// `kick_modele.rs`, portes dans l'ordre de PyTorch (r, z, n).
pub(crate) struct Gru {
    pub wi: [[f32; m::NI]; 3 * m::G],
    pub wh: [[f32; m::G]; 3 * m::G],
    pub bi: [f32; 3 * m::G],
    pub bh: [f32; 3 * m::G],
}

impl Gru {
    fn step(&self, x: &[f32; m::NI], h: &mut [f32; m::G]) {
        const G: usize = m::G;
        let (mut gi, mut gh) = (self.bi, self.bh);
        for (g, row) in gi.iter_mut().zip(&self.wi) {
            *g += dot(row, x);
        }
        for (g, row) in gh.iter_mut().zip(&self.wh) {
            *g += dot(row, h);
        }
        let sigmoid = |v: f32| 1.0 / (1.0 + (-v).exp());
        for j in 0..G {
            let r = sigmoid(gi[j] + gh[j]);
            let z = sigmoid(gi[G + j] + gh[G + j]);
            let n = (gi[2 * G + j] + r * gh[2 * G + j]).tanh();
            h[j] = (1.0 - z) * n + z * h[j];
        }
    }
}

/// Entrées du GRU pour un pas (dans l'ordre de `scripts/kick_modele.py`, `frame_inputs`) : niveau
/// de chaque bande fine par rapport au plus fort pic du morceau, montée de chaque bande sur
/// 10 ms (`old` : niveaux deux pas plus tôt), excès du sub, rapport sub / médiums.
fn context_inputs(f: &Frame, old: Option<&[f64; NF]>) -> [f32; m::NI] {
    let loud = f.peak.iter().fold(f64::MIN, |a, &v| a.max(v));
    let mut x = [0f32; m::NI];
    for b in 0..NF {
        x[b] = ((f.lf[b] - loud) / 20.0).clamp(-4.0, 0.5) as f32;
        x[NF + b] = old.map_or(0.0, |o| ((f.lf[b] - o[b]).max(0.0) / 10.0).min(4.0)) as f32;
    }
    x[2 * NF] = f.sub_onset.min(1.0) as f32;
    x[2 * NF + 1] = (f.sub_vs_mid / 20.0).clamp(-3.0, 3.0) as f32;
    x
}

pub struct Analyzer {
    low: [Biquad; 3],
    low_e: MeanSquare,
    /// Passe-bas avant chaque sous-échantillonnage, facteur du premier (le second vaut 2) et
    /// compteurs d'échantillons.
    aa1: [Biquad; 3],
    aa2: [Biquad; 3],
    d1: usize,
    c1: usize,
    c2: bool,
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
    kick: Juge,
    snare: Juge,
    /// État du contexte (GRU), et nombre de pas de silence de suite.
    state: [f32; m::G],
    silent_hops: u32,
    /// Délai minimal entre deux kicks (s), mesuré entre leurs attaques.
    pub min_gap_s: f64,
    last_kick: f64,
    /// Sensibilité du kick, 1 (peu sensible) à 10 (très sensible).
    pub sensitivity: f32,
    /// Nombre total de kicks détectés.
    pub kicks: u32,
    /// Nombre total de caisses claires (et claps) détectées.
    pub caisses: u32,
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
        // cadences : origine, ~12 kHz (÷ d1), ~6 kHz (÷ 2 d1) ; le pas est un multiple de 2 d1
        let d1 = (sr / 12000.0).round().max(1.0) as usize;
        let rates = [sr, sr / d1 as f64, sr / (2 * d1) as f64];
        let hop = 2 * d1 * ((sr * HOP_S / (2 * d1) as f64).round().max(1.0) as usize);
        let dec = (rates[2] / PITCH_RATE).round().max(1.0) as usize;
        let bands = BANDS.iter().zip(BAND_STAGE).map(|(&(hp, lp), stage)| Band::new(rates[stage], hp, lp)).collect();
        Self {
            low: [Biquad::lowpass(rates[2], 90.0); 3],
            low_e: MeanSquare::new((rates[2] * BASS_WIN_S).round() as usize),
            aa1: Biquad::lowpass6(sr, AA1 * rates[1]),
            aa2: Biquad::lowpass6(rates[1], AA2 * rates[2]),
            d1,
            c1: 0,
            c2: false,
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
            kick: Juge::new(Coup::Kick),
            snare: Juge::new(Coup::Caisse),
            state: [0.0; m::G],
            silent_hops: 0,
            min_gap_s: 0.1,
            last_kick: -9.0,
            sensitivity: 5.0,
            kicks: 0,
            caisses: 0,
            peak_db: -60.0,
            bass_target: 0.0,
            bass: 0.0,
            dec,
            dec_count: 0,
            dec_rate: rates[2] / dec as f64,
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
        for (b, _) in self.bands.iter_mut().zip(BAND_STAGE).filter(|&(_, s)| s == 0) {
            b.run(x);
        }
        let y = self.aa1.iter_mut().fold(x, |v, f| f.run(v));
        self.c1 += 1;
        if self.c1 >= self.d1 {
            self.c1 = 0;
            self.sample_mid(y);
        }
        self.hop_count += 1;
        if self.hop_count >= self.hop {
            self.hop_count = 0;
            self.t += self.dt;
            self.kick_step();
            self.bass_step();
        }
    }

    /// Un échantillon à ~12 kHz : bandes de 1 à 4 kHz, puis un sur deux vers le grave.
    #[inline]
    fn sample_mid(&mut self, x: f64) {
        for (b, _) in self.bands.iter_mut().zip(BAND_STAGE).filter(|&(_, s)| s == 1) {
            b.run(x);
        }
        let y = self.aa2.iter_mut().fold(x, |v, f| f.run(v));
        self.c2 = !self.c2;
        if !self.c2 {
            self.sample_low(y);
        }
    }

    /// Un échantillon à ~6 kHz : bandes sous 1 kHz, niveau du grave, tampon de la hauteur.
    #[inline]
    fn sample_low(&mut self, x: f64) {
        for (b, _) in self.bands.iter_mut().zip(BAND_STAGE).filter(|&(_, s)| s == 2) {
            b.run(x);
        }
        let l = self.low.iter_mut().fold(x, |v, f| f.run(v));
        self.low_e.push(l);
        self.dec_count += 1;
        if self.dec_count >= self.dec {
            self.dec_count = 0;
            if self.pitch_buf.len() == PITCH_N + LAG_MAX + 1 {
                self.pitch_buf.pop_front();
            }
            self.pitch_buf.push_back(l);
        }
    }

    /// Nombre de pas calculés et niveaux (dB) des bandes fines au dernier pas : pour comparer les
    /// mesures avec celles de `scripts/kick_modele.py` (exemple `kick_mesures`).
    #[doc(hidden)]
    pub fn niveaux(&self) -> (u64, [f64; NF]) {
        (self.hops, self.frames.back().map_or([0.0; NF], |f| f.lf))
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
        let per_stage = [self.hop, self.hop / self.d1, self.hop / (2 * self.d1)];
        for (b, band) in self.bands.iter_mut().enumerate() {
            let block = band.acc / per_stage[BAND_STAGE[b]] as f64;
            band.acc = 0.0;
            let e = band.prev_block.map_or(block, |p| (block + p) / 2.0); // fenêtre 10 ms
            band.prev_block = Some(block);
            f.lf[b] = 10.0 * (e + 1e-12).log10();
        }
        let fine = f.lf.map(|v| 10f64.powf(v / 10.0));
        f.l = std::array::from_fn(|b| 10.0 * (fine[2 * b] + fine[2 * b + 1]).log10());
        if i >= 2 {
            let old = self.frame(i - 2).l;
            f.flux = (0..5).map(|b| (f.l[b] - old[b]).max(0.0)).sum();
        }
        let lin = f.l.map(|v| 10f64.powf(v / 10.0));
        f.tot = 10.0 * lin.iter().sum::<f64>().log10();
        // Silence : loin sous le pic récent, donc relatif au volume d'écoute. Les références du morceau
        // ne retombent pas quand rien ne joue : après une pause, le morceau suivant est jugé par rapport
        // au précédent. Elles retombent dès que quelque chose joue, même très bas : un volume baissé de
        // 40 dB est rattrapé en 20 s.
        f.floor = (self.peak.iter().copied().fold(f64::MIN, f64::max) - SILENCE_BELOW).clamp(SILENCE_MIN, SILENCE_DB);
        let decay = f.tot >= f.floor;
        self.flux_pk = f.flux.max(if decay { self.flux_pk * (-dt / 2.0).exp() } else { self.flux_pk });
        f.flux_pk = self.flux_pk;
        for b in 0..5 {
            self.peak[b] = f.l[b].max(if f.tot >= SILENCE_MIN { self.peak[b] - 2.0 * dt } else { self.peak[b] });
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
        // contexte : le GRU avance d'un pas ; après `RESET_S` de silence, il repart de zéro
        // (comme au début de chaque morceau d'apprentissage)
        self.silent_hops = if decay { 0 } else { self.silent_hops + 1 };
        if self.silent_hops as f64 * dt >= RESET_S {
            self.state = [0.0; m::G];
        } else {
            let old = if i >= 2 { Some(self.frame(i - 2).lf) } else { None };
            let x = context_inputs(&f, old.as_ref());
            m::GRU.step(&x, &mut self.state);
        }
        if self.frames.len() == HIST {
            self.frames.pop_front();
        }
        self.frames.push_back(f);
        self.hops += 1;

        // pic du flux au pas précédent : attaque candidate
        if i > PRE + 1 {
            let p = i - 1;
            let (a, prev) = (self.frame(p), self.frame(p - 1));
            if a.flux > prev.flux && a.flux >= f.flux && a.flux >= 0.15 * prev.flux_pk && a.tot > a.floor - 10.0 {
                self.kick.pending.push(Pending { hop: p, stage: 0 });
                self.snare.pending.push(Pending { hop: p, stage: 0 });
            }
        }
        let mut juge = std::mem::replace(&mut self.kick, Juge::new(Coup::Kick));
        let kicks = self.decide(&mut juge, i);
        self.kick = juge;
        if kicks > 0 {
            self.kicks = self.kicks.wrapping_add(kicks);
            self.last_kick = self.t;
        }
        let mut juge = std::mem::replace(&mut self.snare, Juge::new(Coup::Caisse));
        self.caisses = self.caisses.wrapping_add(self.decide(&mut juge, i));
        self.snare = juge;
    }

    /// Juge les candidats de `juge` dont une étape de décision tombe sur le pas `i`, du plus
    /// ancien au plus récent (ordre repris par `decisions` du script) ; renvoie le nombre de coups
    /// reconnus.
    fn decide(&self, juge: &mut Juge, i: u64) -> u32 {
        let kick = juge.coup == Coup::Kick;
        // La sensibilité règle le dernier seuil du kick (combien de kicks) ; les décisions
        // précoces gardent le leur (à quel point il faut être sûr pour ne pas attendre), jamais
        // plus bas. La caisse claire garde les seuils d'un kick à sensibilité 5.
        let shift = if kick { (5.0 - self.sensitivity.clamp(1.0, 10.0) as f64) * SENS_STEP } else { 0.0 };
        let gap = if kick { self.min_gap_s.max(0.05) } else { SNARE_GAP_S };
        let mut found = 0;
        let mut k = 0;
        while k < juge.pending.len() {
            let Pending { hop: p, stage } = juge.pending[k];
            if p + STAGES[stage] as u64 != i {
                k += 1;
                continue;
            }
            let attack = p as f64 * self.dt;
            let since = attack - juge.last_attack;
            let quiet = if kick && since > QUIET_S { QUIET_RAISE } else { 0.0 };
            let last = STAGE_LOGIT[STAGES.len() - 1] + shift;
            let thr = if stage + 1 == STAGES.len() { last } else { STAGE_LOGIT[stage].max(last) } + quiet;
            if since > gap && self.score(juge.coup, p, STAGES[stage]) > thr {
                found += 1;
                juge.last_attack = attack;
                juge.pending.remove(k);
            } else if stage + 1 < STAGES.len() {
                juge.pending[k].stage += 1;
                k += 1;
            } else {
                juge.pending.remove(k);
            }
        }
        found
    }

    /// Score (logit) de l'attaque du pas `p` jugée `s` pas après.
    fn score(&self, coup: Coup, p: u64, s: usize) -> f64 {
        let (x, h) = (self.features(p, s), &self.state);
        match (coup, s) {
            (Coup::Kick, 2) => m::KICK2.logit(&x, h),
            (Coup::Kick, 4) => m::KICK4.logit(&x, h),
            (Coup::Kick, _) => m::KICK6.logit(&x, h),
            (Coup::Caisse, 2) => m::CAISSE2.logit(&x, h),
            (Coup::Caisse, 4) => m::CAISSE4.logit(&x, h),
            (Coup::Caisse, _) => m::CAISSE6.logit(&x, h),
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
        let mut pref = [f64::MAX; NF];
        for q in p - PRE..p - 1 {
            for (v, l) in pref.iter_mut().zip(self.frame(q).lf) {
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
        // la même chose en bandes fines : montée, puis spectre
        for j in [1u64, 2, 4, 6].into_iter().filter(|&j| j <= s as u64) {
            let l = self.frame(p + j).lf;
            (0..NF).for_each(|b| put(l[b] - pref[b]));
        }
        (0..NF).for_each(|b| put(now.lf[b] - now.tot));
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
        times(sig, sens, |a| a.kicks)
    }

    /// Instants (s) où le compteur `count` augmente.
    fn times(sig: &[f32], sens: f32, count: impl Fn(&Analyzer) -> u32) -> Vec<f64> {
        let mut a = Analyzer::new(SR);
        a.sensitivity = sens;
        let mut out = Vec::new();
        for chunk in sig.chunks(480) {
            let before = count(&a);
            a.push(chunk);
            if count(&a) != before {
                out.push(a.t);
            }
        }
        out
    }

    #[test]
    fn caisses_claires_trouvees_seules_et_avec_les_kicks() {
        // caisses claires à 0,5 s, 1,5 s… 7,5 s ; à partir de 4 s, deux tombent avec un kick
        let expected: Vec<f64> = (0..8).map(|k| k as f64 + 0.5).collect();
        for snare in [140.0, 190.0, 230.0] {
            for bass in [false, true] {
                let hits = times(&render(snare, bass), 5.0, |a| a.caisses);
                let found = expected.iter().filter(|&&k| hits.iter().any(|&h| h >= k && h < k + 0.06)).count();
                assert!(found >= 7, "caisse {snare} Hz, basse {bass} : {found}/8 ({hits:?})");
                assert!(hits.len() <= found + 1, "caisse {snare} Hz, basse {bass} : caisses en trop ({hits:?})");
            }
        }
    }

    #[test]
    fn kicks_seuls_ne_donnent_aucune_caisse_claire() {
        for wobble in [6.0, 9.0, 14.0] {
            let hits = times(&render_long_kicks(wobble), 5.0, |a| a.caisses);
            assert!(hits.len() <= 1, "ondulation {wobble} Hz : caisses claires {hits:?}");
        }
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

    /// Le même morceau joué 40 dB plus bas (volume d'écoute presque coupé) donne les mêmes coups, une
    /// fois les références du niveau retombées (20 s : le morceau est joué quatre fois, la dernière
    /// est jugée).
    #[test]
    fn coups_trouves_a_volume_tres_bas() {
        let once = render(190.0, true);
        let start = 3.0 * 8.0;
        for gain in [1.0f32, 0.01] {
            let sig: Vec<f32> = once.iter().cycle().take(once.len() * 4).map(|v| v * gain).collect();
            let kicks: Vec<f64> = kick_times(&sig, 5.0).into_iter().filter(|&t| t >= start).map(|t| t - start).collect();
            let found = [4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5].iter().filter(|&&k| kicks.iter().any(|&h| h >= k && h < k + 0.1)).count();
            assert!(found >= 7, "gain {gain} : {found}/8 kicks ({kicks:?})");
            assert!(kicks.len() <= found + 1, "gain {gain} : kicks en trop ({kicks:?})");
            let snares: Vec<f64> = times(&sig, 5.0, |a| a.caisses).into_iter().filter(|&t| t >= start).map(|t| t - start).collect();
            let found = (0..8).filter(|&k| snares.iter().any(|&h| h >= k as f64 + 0.5 && h < k as f64 + 0.56)).count();
            assert!(found >= 7, "gain {gain} : {found}/8 caisses claires ({snares:?})");
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
