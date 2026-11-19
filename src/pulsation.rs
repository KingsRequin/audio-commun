//! Tempo et phase du temps d'après les seuls instants des kicks (sans FFT).
//!
//! Pensé pour [`crate::kick`] : une application qui détecte déjà les grosses caisses en tire le
//! tempo pour presque rien (quelques milliers d'opérations par kick).
//!
//! Méthode : on garde les kicks des 8 dernières secondes. À chaque kick, pour chaque tempo de
//! 70 à 180 BPM (pas de 0,5), on mesure si les kicks tombent sur une grille de cette période :
//! leur phase moyenne (longueur du vecteur moyen des phases, 1 = tous alignés) multipliée par la
//! part des temps de la grille qui ont leur kick. Ce second facteur écarte le double tempo (une
//! case sur deux vide) ; le premier écarte la moitié du tempo et les rapports 3:2 (phases qui
//! s'annulent). La meilleure grille est ensuite ajustée par moindres carrés sur les kicks qui y
//! tombent : période et origine précises. Elle n'est retenue que si les kicks sont réguliers
//! (80 % au moins sur la grille, écart moyen sous 4 % de la période, 60 % des temps marqués).
//! Le tempo n'est annoncé qu'après trois estimations concordantes, et un autre ne le remplace
//! qu'après trois confirmations : un roulement ou un kick décalé ne le fait pas sauter.
//!
//! Le tempo est gardé pendant un passage sans kick (break, couplet sans grosse caisse) tant que
//! la musique joue, 30 s au plus ; il est oublié après 3 s de silence (morceau suivant) ou si
//! les kicks deviennent durablement irréguliers.

use std::collections::VecDeque;

/// Fenêtre d'analyse.
const FENETRE_S: f64 = 8.0;
/// Kicks nécessaires dans la fenêtre pour estimer un tempo.
const MIN_KICKS: usize = 8;
const MIN_BPM: f64 = 70.0;
const MAX_BPM: f64 = 180.0;
const PAS_BPM: f64 = 0.5;
/// Un kick est « sur la grille » à moins de cette part de période d'un temps.
const TOLERANCE: f64 = 0.12;
/// Part des kicks qui doivent tomber sur la grille.
const PART_SUR_GRILLE: f64 = 0.8;
/// Part des temps de la grille qui doivent avoir leur kick.
const COUVERTURE_MIN: f64 = 0.6;
/// Écart type des kicks autour de la grille ajustée, en part de période.
const ECART_MAX: f64 = 0.04;
/// Deux périodes à moins de 3 % l'une de l'autre = le même tempo.
const MEME_TEMPO: f64 = 0.03;
/// Estimations concordantes pour annoncer (ou changer) un tempo.
const CONFIRMATIONS: u32 = 3;
/// Estimations irrégulières de suite qui font oublier le tempo.
const REFUS_MAX: u32 = 6;
/// Tempo gardé au plus ce temps sans kick.
const GARDE_S: f64 = 30.0;
/// Silence qui fait oublier le tempo.
const OUBLI_SILENCE_S: f64 = 3.0;
/// Silence au-delà duquel la musique est considérée comme arrêtée (relais coupé).
const MUET_S: f64 = 0.3;
/// Sans kick depuis ce temps, alors qu'un tempo est connu et que la musique joue : relais.
pub const RELAIS_S: f64 = 2.0;

/// Grille des temps : `origine + k × periode`.
#[derive(Clone, Copy, Debug)]
struct Grille {
    periode: f64,
    origine: f64,
}

impl Grille {
    fn proche(&self, autre: &Grille) -> bool {
        (self.periode / autre.periode - 1.0).abs() < MEME_TEMPO
    }
}

enum Estimation {
    /// Trop peu de kicks pour juger.
    PeuDeKicks,
    Irreguliers,
    Grille(Grille),
}

/// Suivi du tempo des kicks. Les instants sont en secondes, sur une horloge quelconque mais
/// croissante (temps du son analysé, par exemple).
#[derive(Debug, Default)]
pub struct Pulsation {
    kicks: VecDeque<f64>,
    verrou: Option<Grille>,
    candidat: Option<(Grille, u32)>,
    refus: u32,
    dernier_kick: Option<f64>,
    /// Début du silence en cours.
    muet_depuis: Option<f64>,
}

impl Pulsation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Un kick à l'instant `t`.
    pub fn kick(&mut self, t: f64) {
        self.dernier_kick = Some(t);
        self.kicks.push_back(t);
        while self.kicks.front().is_some_and(|&k| t - k > FENETRE_S) {
            self.kicks.pop_front();
        }
        match self.estimer() {
            Estimation::PeuDeKicks => {}
            Estimation::Irreguliers => {
                self.refus += 1;
                self.candidat = None;
                if self.refus >= REFUS_MAX {
                    self.verrou = None;
                }
            }
            Estimation::Grille(g) => {
                self.refus = 0;
                if self.verrou.is_some_and(|v| v.proche(&g)) {
                    self.verrou = Some(g);
                    self.candidat = None;
                    return;
                }
                let n = match self.candidat {
                    Some((c, n)) if c.proche(&g) => n + 1,
                    _ => 1,
                };
                if n >= CONFIRMATIONS {
                    self.verrou = Some(g);
                    self.candidat = None;
                } else {
                    self.candidat = Some((g, n));
                }
            }
        }
    }

    /// À appeler régulièrement : `audible` = la musique joue en ce moment (niveau au-dessus du
    /// silence). Oublie le tempo après un long silence ou un long passage sans kick.
    pub fn avancer(&mut self, t: f64, audible: bool) {
        if audible {
            self.muet_depuis = None;
        } else {
            let depuis = *self.muet_depuis.get_or_insert(t);
            if t - depuis > OUBLI_SILENCE_S {
                self.oublier();
            }
        }
        if self.dernier_kick.is_some_and(|k| t - k > GARDE_S) {
            self.oublier();
        }
    }

    fn oublier(&mut self) {
        self.kicks.clear();
        (self.verrou, self.candidat, self.refus, self.dernier_kick) = (None, None, 0, None);
    }

    /// Tempo stable, en BPM.
    pub fn bpm(&self) -> Option<f32> {
        self.verrou.map(|g| (60.0 / g.periode) as f32)
    }

    /// Position dans le temps en cours à l'instant `t` : 0 sur le temps, → 1 juste avant le
    /// suivant (les temps tombent là où tombaient les kicks).
    pub fn phase(&self, t: f64) -> Option<f32> {
        self.verrou
            .map(|g| ((t - g.origine) / g.periode).rem_euclid(1.0) as f32)
    }

    /// Relais : un tempo stable est connu, la musique joue, mais aucun kick depuis
    /// [`RELAIS_S`] (break, couplet sans grosse caisse).
    pub fn relais(&self, t: f64) -> bool {
        let muet = self.muet_depuis.is_some_and(|m| t - m > MUET_S);
        self.verrou.is_some() && !muet && self.dernier_kick.is_some_and(|k| t - k > RELAIS_S)
    }

    fn estimer(&self) -> Estimation {
        let n = self.kicks.len();
        if n < MIN_KICKS {
            return Estimation::PeuDeKicks;
        }
        let t0 = self.kicks[0];
        // meilleure grille brute
        let mut meilleure: Option<(f64, Grille)> = None;
        let pas = ((MAX_BPM - MIN_BPM) / PAS_BPM).round() as usize;
        for i in 0..=pas {
            let periode = 60.0 / (MIN_BPM + i as f64 * PAS_BPM);
            let (mut c, mut s) = (0.0, 0.0);
            for &t in &self.kicks {
                let a = std::f64::consts::TAU * (t - t0) / periode;
                c += a.cos();
                s += a.sin();
            }
            let alignement = (c * c + s * s).sqrt() / n as f64;
            if alignement < 0.5 {
                continue;
            }
            let g = Grille {
                periode,
                origine: t0 + periode * s.atan2(c) / std::f64::consts::TAU,
            };
            let score = alignement * self.couverture(&g);
            if meilleure.is_none_or(|(m, _)| score > m) {
                meilleure = Some((score, g));
            }
        }
        let Some((_, brute)) = meilleure else {
            return Estimation::Irreguliers;
        };
        // ajustement par moindres carrés sur les kicks de la grille : t = origine + k × période
        let sur_grille: Vec<(f64, f64)> = self
            .kicks
            .iter()
            .filter_map(|&t| {
                let x = (t - brute.origine) / brute.periode;
                ((x - x.round()).abs() < TOLERANCE).then_some((x.round(), t))
            })
            .collect();
        let m = sur_grille.len();
        if m < MIN_KICKS || (m as f64) < PART_SUR_GRILLE * n as f64 {
            return Estimation::Irreguliers;
        }
        let (mk, mt) = sur_grille
            .iter()
            .fold((0.0, 0.0), |(a, b), &(k, t)| (a + k, b + t));
        let (mk, mt) = (mk / m as f64, mt / m as f64);
        let (mut skk, mut skt) = (0.0, 0.0);
        for &(k, t) in &sur_grille {
            skk += (k - mk) * (k - mk);
            skt += (k - mk) * (t - mt);
        }
        if skk <= 0.0 {
            return Estimation::Irreguliers;
        }
        let periode = skt / skk;
        let g = Grille {
            periode,
            origine: mt - periode * mk,
        };
        let bpm = 60.0 / periode;
        if !(MIN_BPM - 2.0..=MAX_BPM + 2.0).contains(&bpm) {
            return Estimation::Irreguliers;
        }
        let ecart = (sur_grille
            .iter()
            .map(|&(k, t)| (t - g.origine - k * periode).powi(2))
            .sum::<f64>()
            / m as f64)
            .sqrt();
        if ecart > ECART_MAX * periode || self.couverture(&g) < COUVERTURE_MIN {
            return Estimation::Irreguliers;
        }
        Estimation::Grille(g)
    }

    /// Part des temps de la grille (du premier au dernier kick) qui ont un kick.
    fn couverture(&self, g: &Grille) -> f64 {
        let (Some(&premier), Some(&dernier)) = (self.kicks.front(), self.kicks.back()) else {
            return 0.0;
        };
        let indice = |t: f64| ((t - g.origine) / g.periode).round();
        let temps = indice(dernier) - indice(premier) + 1.0;
        let mut marques = 0usize;
        let mut precedent = None;
        for &t in &self.kicks {
            let x = (t - g.origine) / g.periode;
            let k = x.round();
            if (x - k).abs() < TOLERANCE && precedent != Some(k) {
                marques += 1;
                precedent = Some(k);
            }
        }
        marques as f64 / temps.max(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Générateur pseudo-aléatoire déterministe, dans [0, 1[.
    struct Alea(u64);
    impl Alea {
        fn f(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Kicks réguliers à `bpm` pendant `secs` secondes, décalés au hasard de ±`gigue` s (délai
    /// de détection variable), en avançant l'horloge toutes les 10 ms.
    fn reguliers(
        p: &mut Pulsation,
        debut: f64,
        bpm: f64,
        secs: f64,
        gigue: f64,
        alea: &mut Alea,
    ) -> f64 {
        let periode = 60.0 / bpm;
        let mut t = debut;
        let mut prochain = debut;
        while t < debut + secs {
            if t >= prochain {
                p.kick(prochain + gigue * (2.0 * alea.f() - 1.0) + 0.02);
                prochain += periode;
            }
            p.avancer(t, true);
            t += 0.01;
        }
        t
    }

    #[test]
    fn trouve_le_tempo_a_un_bpm_pres() {
        for bpm in [120.0, 128.0, 90.0, 74.0, 174.0] {
            let mut p = Pulsation::new();
            let mut alea = Alea(0x9e37_79b9_7f4a_7c15);
            reguliers(&mut p, 0.0, bpm, 10.0, 0.012, &mut alea);
            let trouve = p.bpm().unwrap_or_else(|| panic!("{bpm} : aucun tempo"));
            assert!((f64::from(trouve) - bpm).abs() <= 1.0, "{bpm} → {trouve}");
        }
    }

    #[test]
    fn tempo_garde_malgre_des_kicks_manques() {
        // un kick manqué de temps en temps : toujours le tempo
        let mut p = Pulsation::new();
        let periode = 60.0 / 128.0;
        for i in 0..24 {
            if i % 7 != 3 {
                p.kick(i as f64 * periode);
            }
        }
        let bpm = p.bpm().expect("tempo");
        assert!((bpm - 128.0).abs() <= 1.0, "{bpm}");
    }

    #[test]
    fn la_phase_suit_les_kicks() {
        let mut p = Pulsation::new();
        let mut alea = Alea(7);
        let fin = reguliers(&mut p, 0.0, 120.0, 10.0, 0.005, &mut alea);
        // kicks à 0,02 + k × 0,5 s : un quart de temps plus tard, phase 0,25
        let t = (fin / 0.5).ceil() * 0.5 + 0.02 + 0.125;
        let phase = p.phase(t).expect("phase");
        assert!((phase - 0.25).abs() < 0.04, "{phase}");
    }

    #[test]
    fn kicks_irreguliers_aucun_tempo() {
        for graine in 1..200u64 {
            let mut p = Pulsation::new();
            let mut alea = Alea(graine.wrapping_mul(0x2545_f491_4f6c_dd1d));
            let mut t = 0.0;
            while t < 20.0 {
                t += 0.25 + 0.6 * alea.f();
                p.kick(t);
                p.avancer(t, true);
                assert_eq!(
                    p.bpm(),
                    None,
                    "graine {graine} : tempo {:?} à {t:.2} s",
                    p.bpm()
                );
            }
        }
    }

    #[test]
    fn rien_en_silence() {
        let mut p = Pulsation::new();
        let mut t = 0.0;
        while t < 30.0 {
            p.avancer(t, false);
            assert!(!p.relais(t));
            t += 0.01;
        }
        assert_eq!((p.bpm(), p.phase(t)), (None, None));
    }

    #[test]
    fn relais_pendant_un_break_puis_oubli_au_silence() {
        let mut p = Pulsation::new();
        let mut alea = Alea(3);
        let mut t = reguliers(&mut p, 0.0, 128.0, 10.0, 0.01, &mut alea);
        assert!(!p.relais(t), "kicks en cours : pas de relais");
        // break : la musique continue sans kick
        let debut_break = t;
        while t < debut_break + 8.0 {
            p.avancer(t, true);
            // le dernier kick est tombé moins d'une période (0,47 s) avant le break
            if t - debut_break > RELAIS_S {
                assert!(
                    p.relais(t),
                    "pas de relais {:.2} s après le début du break",
                    t - debut_break
                );
            } else if t - debut_break < RELAIS_S - 0.5 {
                assert!(!p.relais(t), "relais trop tôt");
            }
            t += 0.01;
        }
        assert!(p.relais(t));
        assert!((p.bpm().unwrap() - 128.0).abs() <= 1.0);
        // la musique s'arrête : plus de relais tout de suite, tempo oublié ensuite
        let arret = t;
        while t < arret + 4.0 {
            p.avancer(t, false);
            if t - arret > 0.35 {
                assert!(!p.relais(t));
            }
            t += 0.01;
        }
        assert_eq!(p.bpm(), None);
    }

    #[test]
    fn un_nouveau_tempo_remplace_l_ancien() {
        let mut p = Pulsation::new();
        let mut alea = Alea(11);
        let t = reguliers(&mut p, 0.0, 120.0, 10.0, 0.008, &mut alea);
        assert!((p.bpm().unwrap() - 120.0).abs() <= 1.0);
        reguliers(&mut p, t, 95.0, 10.0, 0.008, &mut alea);
        assert!((p.bpm().unwrap() - 95.0).abs() <= 1.0, "{:?}", p.bpm());
    }
}
