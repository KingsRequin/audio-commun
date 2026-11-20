//! Vérifie que l'analyse du kick calcule comme `scripts/kick_modele.py` :
//! `cargo run --release --example kick_mesures -- <morceau.f32> [niveaux.bin] [cadence=48000]`.
//!
//! Affiche chaque coup reconnu (`kick 12.345` ou `caisse 12.345`, instant de la décision en
//! secondes), comme `python scripts/kick_modele.py detecter src/kick_modele.rs <morceau.f32>` :
//! les deux sorties doivent être les mêmes. Avec `niveaux.bin`, écrit aussi par pas de 5 ms les
//! 10 niveaux de bande (f32, dB) à comparer à `levels` du script.

use audio_commun::kick::Analyzer;
use std::io::Write;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(input) = args.first() else {
        eprintln!("usage : kick_mesures <morceau.f32> [niveaux.bin] [cadence]");
        std::process::exit(2);
    };
    let sr: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(48000);
    let bytes = std::fs::read(input)?;
    let mut a = Analyzer::new(sr);
    let mut levels = match args.get(1) {
        Some(path) => Some(std::io::BufWriter::new(std::fs::File::create(path)?)),
        None => None,
    };
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut seen = (0, 0, 0);
    for b in bytes.chunks_exact(4) {
        a.push(&[f32::from_le_bytes([b[0], b[1], b[2], b[3]])]);
        let (hops, lf) = a.niveaux();
        if hops == seen.0 {
            continue;
        }
        if a.kicks != seen.1 {
            writeln!(out, "kick {:.3}", a.t)?;
        }
        if a.caisses != seen.2 {
            writeln!(out, "caisse {:.3}", a.t)?;
        }
        seen = (hops, a.kicks, a.caisses);
        if let Some(w) = &mut levels {
            for v in lf {
                w.write_all(&(v as f32).to_le_bytes())?;
            }
        }
    }
    if let Some(mut w) = levels {
        w.flush()?;
    }
    out.flush()
}
