//! Banc d'essai du kick sur de vrais morceaux annotés :
//! `cargo run --release --example kick_bench -- <dossier> [sensibilité=5] [délai=0.1] [-v]`.
//!
//! Le dossier contient, pour chaque morceau, `<nom>.f32` (mono, f32 petit-boutiste, 48 kHz) et
//! `<nom>.kicks` (un instant de kick en secondes par ligne, la référence). `scripts/kick_bench.py`
//! les prépare (annotations MDB Drums, ou, pour un morceau quelconque, transcription ADTOF de la
//! batterie séparée par Demucs).
//!
//! Un kick détecté compte s'il tombe entre 20 ms avant et 70 ms après un kick de référence
//! (le détecteur est causal : il répond après l'attaque). Affiche par morceau la précision
//! (part des kicks détectés qui sont vrais), le rappel (part des vrais kicks trouvés), le score
//! F1, le délai médian et les faux kicks dans l'intro (avant le premier vrai kick).

use audio_commun::kick::Analyzer;
use std::path::Path;

const SR: u32 = 48000;
const BEFORE: f64 = 0.02;
const AFTER: f64 = 0.07;

struct Score {
    refs: usize,
    hits: usize,
    tp: usize,
    intro_fp: usize,
    delays: Vec<f64>,
}

impl Score {
    fn precision(&self) -> f64 {
        self.tp as f64 / self.hits.max(1) as f64
    }
    fn recall(&self) -> f64 {
        self.tp as f64 / self.refs.max(1) as f64
    }
    fn f1(&self) -> f64 {
        let (p, r) = (self.precision(), self.recall());
        if p + r > 0.0 {
            2.0 * p * r / (p + r)
        } else {
            0.0
        }
    }
}

fn detect(samples: &[f32], sens: f32, gap: f64) -> Vec<f64> {
    let mut a = Analyzer::new(SR);
    a.sensitivity = sens;
    a.min_gap_s = gap;
    let mut out = Vec::new();
    // Paquets de 10 ms, comme la capture : l'instant noté est la fin du paquet.
    for chunk in samples.chunks(480) {
        let before = a.kicks;
        a.push(chunk);
        if a.kicks != before {
            out.push(a.t);
        }
    }
    out
}

fn score(refs: &[f64], hits: &[f64]) -> Score {
    let mut used = vec![false; refs.len()];
    let mut tp = 0;
    let mut delays = Vec::new();
    for &h in hits {
        // vrai kick libre le plus proche dans la fenêtre
        let best = refs
            .iter()
            .enumerate()
            .filter(|&(i, &r)| !used[i] && h >= r - BEFORE && h <= r + AFTER)
            .min_by(|a, b| (h - a.1).abs().total_cmp(&(h - b.1).abs()));
        if let Some((i, &r)) = best {
            used[i] = true;
            tp += 1;
            delays.push(h - r);
        }
    }
    let first = refs.first().copied().unwrap_or(f64::MAX);
    let intro_fp = hits.iter().filter(|&&h| h < first - BEFORE).count();
    Score { refs: refs.len(), hits: hits.len(), tp, intro_fp, delays }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v");
    let pos: Vec<&String> = args.iter().filter(|a| *a != "-v").collect();
    let Some(dir) = pos.first() else {
        eprintln!("usage : kick_bench <dossier> [sensibilité=5] [délai=0.1] [-v]");
        std::process::exit(2);
    };
    let sens: f32 = pos.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let gap: f64 = pos.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.1);

    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".kicks").map(str::to_string))
        .filter(|n| Path::new(dir.as_str()).join(format!("{n}.f32")).exists())
        .collect();
    names.sort();

    println!("{:<44} {:>5} {:>5} {:>6} {:>6} {:>6} {:>7} {:>5}", "morceau", "réf", "dét", "préc", "rappel", "F1", "délai", "intro");
    let mut total = Score { refs: 0, hits: 0, tp: 0, intro_fp: 0, delays: Vec::new() };
    let mut f1_sum = 0.0;
    for name in &names {
        let base = Path::new(dir.as_str()).join(name);
        let bytes = std::fs::read(base.with_extension("f32"))?;
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let mut refs: Vec<f64> = std::fs::read_to_string(base.with_extension("kicks"))?
            .lines()
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect();
        refs.sort_by(f64::total_cmp);
        let hits = detect(&samples, sens, gap);
        let mut s = score(&refs, &hits);
        let label: String = name.chars().take(44).collect();
        println!(
            "{:<44} {:>5} {:>5} {:>6.2} {:>6.2} {:>6.2} {:>5.0}ms {:>5}",
            label,
            s.refs,
            s.hits,
            s.precision(),
            s.recall(),
            s.f1(),
            median(&mut s.delays) * 1000.0,
            s.intro_fp
        );
        if verbose {
            let fp: Vec<String> = hits
                .iter()
                .filter(|&&h| !refs.iter().any(|&r| h >= r - BEFORE && h <= r + AFTER))
                .map(|h| format!("{h:.2}"))
                .collect();
            println!("    faux : {}", fp.join(" "));
        }
        f1_sum += s.f1();
        total.refs += s.refs;
        total.hits += s.hits;
        total.tp += s.tp;
        total.intro_fp += s.intro_fp;
        total.delays.append(&mut s.delays);
    }
    println!(
        "{:<44} {:>5} {:>5} {:>6.2} {:>6.2} {:>6.2} {:>5.0}ms {:>5}",
        "TOTAL",
        total.refs,
        total.hits,
        total.precision(),
        total.recall(),
        total.f1(),
        median(&mut total.delays) * 1000.0,
        total.intro_fp
    );
    println!("F1 moyen par morceau : {:.3}", f1_sum / names.len().max(1) as f64);
    Ok(())
}
