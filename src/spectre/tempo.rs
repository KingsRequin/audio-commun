//! Suivi du tempo et de la phase du temps, en continu et sans allocation.
//!
//! Le signal d'attaques (flux spectral, une valeur par image d'analyse) est gardé sur
//! quelques secondes. Toutes les ~0,25 s : autocorrélation → période la plus probable
//! (avec un a priori log-normal autour de 120 BPM et le renfort de la double période,
//! comme librosa / BTrack), puis peigne d'impulsions pour trouver où tombent les temps.
//! Entre deux estimations, un oscillateur avance seul et en est rapproché doucement (PLL) :
//! la phase reste continue, les temps arrivent même quand une grosse caisse manque.

/// Réglages.
mod tuning {
    pub const HISTORY_S: f32 = 6.0;
    /// Historique minimal avant une première estimation.
    pub const WARMUP_S: f32 = 2.5;
    pub const UPDATE_S: f32 = 0.25;
    pub const MIN_BPM: f32 = 70.0;
    pub const MAX_BPM: f32 = 185.0;
    /// A priori : centre et écart-type (en octaves) de la distribution des tempos.
    pub const PRIOR_BPM: f32 = 120.0;
    pub const PRIOR_OCTAVES: f32 = 0.8;
    /// Verrouillage (confiance) avec hystérésis.
    pub const LOCK_ON: f32 = 0.22;
    pub const LOCK_OFF: f32 = 0.10;
    /// Part de l'erreur de phase corrigée à chaque estimation et à chaque grosse caisse proche.
    pub const PHASE_GAIN: f32 = 0.25;
    pub const KICK_GAIN: f32 = 0.3;
    pub const KICK_WINDOW: f32 = 0.18;
    pub const PERIOD_GAIN: f32 = 0.03;
    /// Écart relatif de période jugé « le même tempo », et estimations concordantes
    /// nécessaires pour changer de tempo.
    pub const SAME_TEMPO: f32 = 0.05;
    pub const SWITCH_HITS: u32 = 4;
    /// Le tempo courant est gardé tant que son score atteint cette part du meilleur
    /// (évite d'osciller entre deux lectures plausibles, 3:2 ou 2:1).
    pub const KEEP_RATIO: f32 = 0.8;
    /// Anticipation de l'affichage : compense la latence de capture + d'envoi aux LED.
    pub const LEAD_S: f32 = 0.03;
}
use tuning::*;

/// Différence circulaire `a − b` ramenée dans `[-0,5 ; 0,5[`.
fn wrap(d: f32) -> f32 {
    (d + 0.5).rem_euclid(1.0) - 0.5
}

pub(crate) struct Tempo {
    rate: f32,
    oss: Vec<f32>,
    pos: usize,
    count: usize,
    x: Vec<f32>,
    acf: Vec<f32>,
    lags: (usize, usize),
    since_update: usize,
    update_every: usize,
    /// Période en images d'analyse (0 = inconnue).
    period: f32,
    candidate: f32,
    candidate_hits: u32,
    confidence: f32,
    /// Fraction du temps écoulée depuis le dernier temps.
    phase: f32,
    locked: bool,
}

impl Tempo {
    pub fn new(rate: f32) -> Self {
        let n = ((HISTORY_S * rate).round() as usize).max(64);
        let lag_min = ((rate * 60.0 / MAX_BPM).floor() as usize).max(2);
        let lag_max = ((rate * 60.0 / MIN_BPM).ceil() as usize).min(n / 2 - 1);
        Self {
            rate,
            oss: vec![0.0; n],
            pos: 0,
            count: 0,
            x: vec![0.0; n],
            acf: vec![0.0; 2 * lag_max + 2],
            lags: (lag_min, lag_max),
            since_update: 0,
            update_every: ((UPDATE_S * rate).round() as usize).max(1),
            period: 0.0,
            candidate: 0.0,
            candidate_hits: 0,
            confidence: 0.0,
            phase: 0.0,
            locked: false,
        }
    }

    pub fn bpm(&self) -> f32 {
        if self.locked {
            self.rate * 60.0 / self.period
        } else {
            0.0
        }
    }

    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Phase affichée (anticipée de la latence de sortie).
    pub fn phase(&self) -> f32 {
        if self.period > 0.0 {
            (self.phase + LEAD_S * self.rate / self.period).fract()
        } else {
            0.0
        }
    }

    /// Une image : `oss` = force d'attaque globale, `kick` = une grosse caisse vient d'être
    /// détectée. Renvoie vrai si un temps de la grille tombe sur cette image.
    pub fn push(&mut self, oss: f32, kick: bool, silent: bool) -> bool {
        let n = self.oss.len();
        self.oss[self.pos] = if silent { 0.0 } else { oss };
        self.pos = (self.pos + 1) % n;
        self.count = (self.count + 1).min(n);
        if silent {
            // Plus de musique : on oublie le tempo (le morceau suivant aura le sien).
            self.confidence *= 0.97;
            if self.confidence < LOCK_OFF {
                self.locked = false;
            }
        }

        let mut beat = false;
        if self.period > 0.0 {
            self.phase += 1.0 / self.period;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                beat = self.locked;
            }
            // Une grosse caisse tout près d'un temps attendu recale la phase sur elle.
            if kick && self.locked {
                let err = wrap(-self.phase);
                if err.abs() < KICK_WINDOW {
                    self.phase = (self.phase + KICK_GAIN * err).max(0.0);
                    // Grosse caisse en avance (err > 0) → période un peu trop longue, et inversement.
                    self.period *= 1.0 - PERIOD_GAIN * err;
                }
            }
        }

        self.since_update += 1;
        if self.since_update >= self.update_every && self.count as f32 >= WARMUP_S * self.rate {
            self.since_update = 0;
            if !silent {
                self.estimate();
            }
        }
        beat
    }

    /// Valeur du signal d'attaques `back` images avant la plus récente (interpolée).
    fn oss_back(&self, back: f32) -> f32 {
        let n = self.oss.len();
        let i = back.floor() as usize;
        let t = back - i as f32;
        let at = |k: usize| self.oss[(self.pos + n - 1 - k.min(n - 1)) % n];
        at(i) * (1.0 - t) + at(i + 1) * t
    }

    fn estimate(&mut self) {
        let n = self.count;
        let len = self.oss.len();
        let start = (self.pos + len - n) % len;
        let mut mean = 0.0;
        for i in 0..n {
            self.x[i] = self.oss[(start + i) % len];
            mean += self.x[i];
        }
        mean /= n as f32;
        let mut r0 = 0.0;
        for v in &mut self.x[..n] {
            *v -= mean;
            r0 += *v * *v;
        }
        if r0 <= 1e-9 {
            return;
        }
        let (lmin, lmax) = self.lags;
        let top = (2 * lmax + 1).min(n - 1);
        for l in 1..=top {
            let s: f32 = self.x[..n - l]
                .iter()
                .zip(&self.x[l..n])
                .map(|(a, b)| a * b)
                .sum();
            // Corrigée du biais (moins de produits aux grands décalages).
            self.acf[l] = s / r0 * n as f32 / (n - l) as f32;
        }
        let score = |l: usize| {
            let double = if 2 * l <= top { self.acf[2 * l] } else { 0.0 };
            let bpm = self.rate * 60.0 / l as f32;
            let z = (bpm / PRIOR_BPM).log2() / PRIOR_OCTAVES;
            (self.acf[l] + 0.5 * double).max(0.0) * (-0.5 * z * z).exp()
        };
        let Some(mut best) = (lmin..=lmax.min(top)).max_by(|&a, &b| score(a).total_cmp(&score(b)))
        else {
            return;
        };
        if self.locked {
            let cur = (self.period.round() as usize).clamp(lmin, lmax.min(top));
            // Sommet local le plus proche de la période courante.
            let cur = (cur.saturating_sub(1).max(lmin)..=(cur + 1).min(lmax.min(top)))
                .max_by(|&a, &b| score(a).total_cmp(&score(b)))
                .unwrap_or(cur);
            if score(cur) >= KEEP_RATIO * score(best) {
                best = cur;
            }
        }
        // Affinage parabolique entre les décalages voisins.
        let mut p = best as f32;
        if best > lmin && best < lmax.min(top) {
            let (a, b, c) = (score(best - 1), score(best), score(best + 1));
            let den = a - 2.0 * b + c;
            if den.abs() > 1e-9 {
                p += (0.5 * (a - c) / den).clamp(-0.5, 0.5);
            }
        }
        let raw_conf = self.acf[best].clamp(0.0, 1.0);
        self.confidence += (raw_conf - self.confidence) * 0.3;
        self.update_period(p);

        if self.confidence > LOCK_ON {
            let was_locked = self.locked;
            self.locked = true;
            let measured = self.measure_phase();
            if was_locked {
                let err = wrap(measured - self.phase);
                // Correction vers l'arrière bornée à 0 : pas de temps compté deux fois.
                self.phase = (self.phase + PHASE_GAIN * err).max(0.0);
            } else {
                self.phase = measured;
            }
        } else if self.confidence < LOCK_OFF {
            self.locked = false;
        }
    }

    fn update_period(&mut self, p: f32) {
        if self.period <= 0.0 {
            self.period = p;
            return;
        }
        let ratio = p / self.period;
        if (ratio - 1.0).abs() < SAME_TEMPO {
            self.period += (p - self.period) * 0.3;
            self.candidate_hits = 0;
            return;
        }
        // Moitié / double du tempo courant : erreur d'octave classique, on garde le tempo.
        if (ratio - 2.0).abs() < 0.12 || (ratio - 0.5).abs() < 0.03 {
            return;
        }
        if self.candidate > 0.0 && (p / self.candidate - 1.0).abs() < SAME_TEMPO {
            self.candidate_hits += 1;
        } else {
            self.candidate = p;
            self.candidate_hits = 1;
        }
        if self.candidate_hits >= SWITCH_HITS || !self.locked {
            self.period = p;
            self.candidate_hits = 0;
        }
    }

    /// Fraction du temps écoulée depuis le dernier temps, d'après un peigne d'impulsions
    /// espacées d'une période, pondérées (les plus récentes comptent davantage).
    fn measure_phase(&self) -> f32 {
        let p = self.period;
        let span = (self.count - 2) as f32;
        let teeth = ((span / p).floor() as usize).clamp(1, 8);
        let steps = p.ceil() as usize;
        let mut best = (0usize, f32::MIN);
        for phi in 0..steps {
            let mut s = 0.0;
            let mut w = 1.0;
            for k in 0..teeth {
                let back = phi as f32 + k as f32 * p;
                if back >= span {
                    break;
                }
                s += w * self.oss_back(back);
                w *= 0.8;
            }
            if s > best.1 {
                best = (phi, s);
            }
        }
        best.0 as f32 / p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Impulsions régulières (`bpm`) dans le signal d'attaques, sur `secs` secondes.
    fn run(bpm: f32, secs: f32, rate: f32) -> (Tempo, Vec<f32>) {
        let mut t = Tempo::new(rate);
        let period = rate * 60.0 / bpm;
        let mut beats = Vec::new();
        let frames = (secs * rate) as usize;
        for i in 0..frames {
            let on = (i as f32 % period) < 1.0;
            if t.push(if on { 1.0 } else { 0.05 }, on, false) {
                beats.push(i as f32 / rate);
            }
        }
        (t, beats)
    }

    #[test]
    fn trouve_le_tempo() {
        for bpm in [90.0, 120.0, 128.0, 150.0] {
            let (t, _) = run(bpm, 8.0, 60.0);
            assert!(t.locked(), "{bpm} : pas verrouillé ({})", t.confidence());
            assert!((t.bpm() - bpm).abs() < bpm * 0.03, "{bpm} → {}", t.bpm());
        }
    }

    #[test]
    fn temps_reguliers_et_en_phase() {
        let (_, beats) = run(120.0, 10.0, 60.0);
        let late: Vec<f32> = beats.iter().copied().filter(|&b| b > 5.0).collect();
        assert!(late.len() >= 9, "{beats:?}");
        for w in late.windows(2) {
            assert!((w[1] - w[0] - 0.5).abs() < 0.04, "{late:?}");
        }
        // Les temps tombent sur les impulsions (multiples de 0,5 s), à ±2 images près.
        for b in late {
            let off = wrap(b / 0.5);
            assert!(off.abs() < 0.08, "temps à {b} (décalage {off})");
        }
    }

    #[test]
    fn aucun_tempo_sur_du_bruit_constant() {
        let mut t = Tempo::new(60.0);
        let mut beats = 0;
        for i in 0..600 {
            // Pseudo-aléa déterministe (hachage), sans régularité.
            let mut h = i as u32;
            h ^= h >> 16;
            h = h.wrapping_mul(0x7feb_352d);
            h ^= h >> 15;
            h = h.wrapping_mul(0x846c_a68b);
            h ^= h >> 16;
            let v = ((h >> 8) as f32 / (1u32 << 24) as f32).powi(3);
            beats += u32::from(t.push(v, false, false));
        }
        assert!(
            beats <= 2,
            "{beats} temps sur du bruit (confiance {})",
            t.confidence()
        );
    }
}
