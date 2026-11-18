//! Attaques par flux spectral : somme des hausses de magnitude (log-compressée) d'une
//! plage de fréquences, comparée à un seuil adaptatif (moyenne + K·écart-type récents).

/// Flux spectral d'une plage de cases FFT.
pub(crate) struct Flux {
    bins: (usize, usize),
    /// Compression log : `ln(1 + γ·a/gain)`. Plus γ est grand, plus les sons faibles comptent.
    gamma: f32,
    prev: Vec<f32>,
}

impl Flux {
    pub fn new(bins: (usize, usize), gamma: f32) -> Self {
        let (k0, k1) = (bins.0, bins.1.max(bins.0));
        Self {
            bins: (k0, k1),
            gamma,
            prev: vec![0.0; k1 - k0 + 1],
        }
    }

    /// Hausse moyenne de la magnitude log (relative à `gain`) depuis l'image précédente.
    pub fn update(&mut self, power: &[f32], gain: f32, silent: bool) -> f32 {
        let (k0, k1) = self.bins;
        let mut flux = 0.0f32;
        for (p, prev) in power[k0..=k1].iter().zip(self.prev.iter_mut()) {
            let mag = if silent {
                0.0
            } else {
                (1.0 + self.gamma * p.sqrt() / gain).ln()
            };
            flux += (mag - *prev).max(0.0);
            *prev = mag;
        }
        flux / self.prev.len() as f32
    }
}

/// Seuil adaptatif + période réfractaire ; produit une force d'attaque et une enveloppe.
pub(crate) struct Onset {
    hist: Vec<f32>,
    pos: usize,
    count: usize,
    k: f32,
    min_flux: f32,
    refractory_s: f32,
    since_s: f32,
    decay: f32,
    /// Enveloppe publiée : saute à la force de l'attaque, retombe exponentiellement.
    pub envelope: f32,
    /// Nombre d'attaques détectées (boucle).
    pub count_total: u32,
}

impl Onset {
    /// `history_s` : mémoire du seuil ; `k` : écarts-types au-dessus de la moyenne ;
    /// `min_flux` : flux minimal absolu ; `decay_s` : constante de temps de l'enveloppe.
    pub fn new(
        dt: f32,
        history_s: f32,
        k: f32,
        min_flux: f32,
        refractory_s: f32,
        decay_s: f32,
    ) -> Self {
        Self {
            hist: vec![0.0; ((history_s / dt).round() as usize).max(4)],
            pos: 0,
            count: 0,
            k,
            min_flux,
            refractory_s,
            since_s: refractory_s,
            decay: (-dt / decay_s.max(1e-3)).exp(),
            envelope: 0.0,
            count_total: 0,
        }
    }

    /// Une image : renvoie la force de l'attaque (0 = aucune, sinon 0,4–1).
    pub fn update(&mut self, flux: f32, dt: f32, silent: bool) -> f32 {
        let h = self.count.max(1) as f32;
        let past = &self.hist[..self.count];
        let mean = past.iter().sum::<f32>() / h;
        let var = past.iter().map(|f| (f - mean) * (f - mean)).sum::<f32>() / h;
        let threshold = (mean + self.k * var.sqrt()).max(self.min_flux);
        self.hist[self.pos] = flux;
        self.pos = (self.pos + 1) % self.hist.len();
        self.count = (self.count + 1).min(self.hist.len());

        self.since_s += dt;
        self.envelope *= self.decay;
        if silent || flux <= threshold || self.since_s < self.refractory_s {
            return 0.0;
        }
        self.since_s = 0.0;
        self.count_total = self.count_total.wrapping_add(1);
        // Une attaque juste au-dessus du seuil reste visible ; 2,5× le seuil = pleine force.
        let strength = (0.4 + 0.4 * (flux / threshold - 1.0)).clamp(0.4, 1.0);
        self.envelope = self.envelope.max(strength);
        strength
    }
}
