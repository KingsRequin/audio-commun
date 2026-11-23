//! Banc d'essai des temps de `spectre` (`beats`, `bpm`) sur les morceaux du banc du kick :
//! `cargo run --release --example tempo_bench -- <dossier> [-v]`.
//!
//! Le dossier est celui de `kick_bench` (`<nom>.f32` mono 48 kHz, `<nom>.kicks`). Les temps n'y sont pas
//! annotés : la référence est la grille que [`audio_commun::pulsation`] tire des kicks de référence, là
//! où ils sont réguliers (au moins 8 kicks sur une même grille). Seuls ces passages sont jugés ; un
//! morceau sans kick régulier (jazz, voix seule) ne compte que dans la dernière ligne.
//!
//! Affiche, sur les passages à grille de référence :
//! - la part du temps où `spectre` annonce un tempo, et s'il est le bon (à 4 % près), son double ou sa
//!   moitié, ou un autre ;
//! - la précision des temps (part des temps émis qui tombent sur la grille, de 50 ms avant à 90 ms après :
//!   l'analyse est causale) et leur rappel (part des temps de la grille qui ont le leur) ;
//! - le décalage médian des temps justes.
//!
//! Et sur tout le dossier : les temps émis par minute quand aucun tempo n'est annoncé (ce sont alors les
//! grosses caisses détectées qui font office de temps).

use audio_commun::pulsation::Pulsation;
use audio_commun::spectre::{Analyzer, AudioFeatures};
use std::path::Path;

const SR: u32 = 48000;
const RATE: u32 = 60;
const BEFORE: f64 = 0.05;
const AFTER: f64 = 0.09;

#[derive(Default)]
struct Bilan {
    /// Secondes : en tout, avec grille de référence, et dont tempo annoncé (bon, octave, autre).
    total_s: f64,
    ref_s: f64,
    same_s: f64,
    octave_s: f64,
    other_s: f64,
    /// Temps de la grille de référence, temps émis pendant qu'elle est connue, dont justes.
    expected: f64,
    emitted: usize,
    on_grid: usize,
    offsets: Vec<f64>,
    /// Sans tempo annoncé : secondes (hors silence) et temps émis.
    free_s: f64,
    free_beats: usize,
}

impl Bilan {
    fn add(&mut self, o: Bilan) {
        self.total_s += o.total_s;
        self.ref_s += o.ref_s;
        self.same_s += o.same_s;
        self.octave_s += o.octave_s;
        self.other_s += o.other_s;
        self.expected += o.expected;
        self.emitted += o.emitted;
        self.on_grid += o.on_grid;
        self.offsets.extend(o.offsets);
        self.free_s += o.free_s;
        self.free_beats += o.free_beats;
    }

    fn line(&mut self, label: &str) -> String {
        let pct = |a: f64, b: f64| if b > 0.0 { 100.0 * a / b } else { 0.0 };
        self.offsets.sort_by(f64::total_cmp);
        let median = self.offsets.get(self.offsets.len() / 2).map_or(f64::NAN, |o| o * 1000.0);
        format!(
            "{label:<44} grille {:>5.1} min sur {:>5.1} | tempo bon {:>3.0} % octave {:>3.0} % autre {:>3.0} % | temps préc {:.2} rappel {:.2} décalage {:>3.0} ms | sans tempo {:>5.1} temps/min sur {:>5.1} min",
            self.ref_s / 60.0,
            self.total_s / 60.0,
            pct(self.same_s, self.ref_s),
            pct(self.octave_s, self.ref_s),
            pct(self.other_s, self.ref_s),
            self.on_grid as f64 / self.emitted.max(1) as f64,
            self.on_grid as f64 / self.expected.max(1.0),
            median,
            self.free_beats as f64 / (self.free_s / 60.0).max(1e-9),
            self.free_s / 60.0,
        )
    }
}

fn measure(samples: &[f32], kicks: &[f64]) -> Bilan {
    let mut a = Analyzer::new(SR, 32, RATE as f32);
    let mut out = AudioFeatures::default();
    let mut grid = Pulsation::new();
    let mut b = Bilan::default();
    let dt = 1.0 / RATE as f64;
    let (mut n, mut next_kick, mut last_beats) = (0usize, 0usize, 0u32);
    for chunk in samples.chunks((SR / RATE) as usize) {
        a.push(chunk);
        a.analyze(&mut out);
        n += chunk.len();
        let t = n as f64 / SR as f64;
        while next_kick < kicks.len() && kicks[next_kick] <= t {
            grid.kick(kicks[next_kick]);
            next_kick += 1;
        }
        grid.avancer(t, true);
        let beats = out.beats.wrapping_sub(last_beats) as usize;
        last_beats = out.beats;
        b.total_s += dt;
        if out.bpm == 0.0 && !out.silent {
            b.free_s += dt;
            b.free_beats += beats;
        }
        let (Some(bpm), Some(phase)) = (grid.bpm(), grid.phase(t)) else { continue };
        let period = 60.0 / bpm as f64;
        b.ref_s += dt;
        b.expected += dt / period;
        if out.bpm > 0.0 {
            let ratio = out.bpm / bpm;
            if (ratio - 1.0).abs() < 0.04 {
                b.same_s += dt;
            } else if (ratio - 2.0).abs() < 0.08 || (ratio - 0.5).abs() < 0.02 {
                b.octave_s += dt;
            } else {
                b.other_s += dt;
            }
        }
        if beats > 0 {
            b.emitted += beats;
            let offset = ((phase as f64 + 0.5).rem_euclid(1.0) - 0.5) * period;
            if (-BEFORE..=AFTER).contains(&offset) {
                b.on_grid += 1;
                b.offsets.push(offset);
            }
        }
    }
    b
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v");
    let Some(dir) = args.iter().find(|a| *a != "-v").map(Path::new) else {
        eprintln!("usage : tempo_bench <dossier> [-v]");
        std::process::exit(2);
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".kicks").map(str::to_string))
        .filter(|n| dir.join(format!("{n}.f32")).exists())
        .collect();
    names.sort();
    let mut total = Bilan::default();
    for name in &names {
        let bytes = std::fs::read(dir.join(format!("{name}.f32")))?;
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let mut kicks: Vec<f64> = std::fs::read_to_string(dir.join(format!("{name}.kicks")))?
            .lines()
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect();
        kicks.sort_by(f64::total_cmp);
        let mut b = measure(&samples, &kicks);
        if verbose {
            println!("{}", b.line(&name.chars().take(44).collect::<String>()));
        }
        total.add(b);
    }
    println!("{}", total.line(&format!("TOTAL ({} morceaux)", names.len())));
    Ok(())
}
