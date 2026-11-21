//! Banc d'essai du kick sur de vrais morceaux annotés :
//! `cargo run --release --example kick_bench -- <dossier> [sensibilité=5] [délai=0.1] [-v] [--detail <fichier.tsv>]`.
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
//!
//! Si `<nom>.kicks_doute` existe (instants où la référence est incertaine), une détection qui y
//! tombe ne compte ni juste ni fausse ; de même `<nom>.caisses_doute`. Un dossier sans aucun kick
//! de référence (voix, musique sans batterie) se lit en faux kicks par minute.
//!
//! Si `<nom>.caisses` existe (instants des vraies caisses claires), les caisses claires
//! détectées sont jugées de même ; leur total est affiché à la fin.
//!
//! `--detail` écrit une ligne par morceau et par détecteur (trouvés, annoncés, attendus, durée), dans le
//! format de `kick_modele.py valider --detail` : de quoi regrouper les résultats par style de musique.

use audio_commun::kick::Analyzer;
use std::io::Write;
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

/// Instants des kicks et des caisses claires détectés.
fn detect(samples: &[f32], sens: f32, gap: f64) -> (Vec<f64>, Vec<f64>) {
    let mut a = Analyzer::new(SR);
    a.sensitivity = sens;
    a.min_gap_s = gap;
    let (mut kicks, mut snares) = (Vec::new(), Vec::new());
    // Paquets de 10 ms, comme la capture : l'instant noté est la fin du paquet.
    for chunk in samples.chunks(480) {
        let before = (a.kicks, a.caisses);
        a.push(chunk);
        if a.kicks != before.0 {
            kicks.push(a.t);
        }
        if a.caisses != before.1 {
            snares.push(a.t);
        }
    }
    (kicks, snares)
}

fn read_times(path: &Path) -> std::io::Result<Vec<f64>> {
    let mut v: Vec<f64> = std::fs::read_to_string(path)?.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect();
    v.sort_by(f64::total_cmp);
    Ok(v)
}

fn score(refs: &[f64], doubt: &[f64], hits: &[f64]) -> Score {
    // une détection sur un instant douteux (référence incertaine) ne compte ni juste ni fausse
    let near = |h: f64, r: &[f64]| r.iter().any(|&r| h >= r - BEFORE && h <= r + AFTER);
    let hits: Vec<f64> = hits.iter().copied().filter(|&h| near(h, refs) || !near(h, doubt)).collect();
    let mut used = vec![false; refs.len()];
    let mut tp = 0;
    let mut delays = Vec::new();
    for &h in &hits {
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
    let detail_at = args.iter().position(|a| a == "--detail");
    let pos: Vec<&String> = args.iter().enumerate().filter(|&(i, a)| a != "-v" && detail_at.is_none_or(|d| i != d && i != d + 1)).map(|(_, a)| a).collect();
    let Some(dir) = pos.first() else {
        eprintln!("usage : kick_bench <dossier> [sensibilité=5] [délai=0.1] [-v] [--detail <fichier.tsv>]");
        std::process::exit(2);
    };
    let mut detail = match detail_at.and_then(|d| args.get(d + 1)) {
        Some(path) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
            writeln!(f, "détecteur\tdossier\tmorceau\ttrouvés\tannoncés\tattendus\tdurée")?;
            Some(f)
        }
        None => None,
    };
    let folder = Path::new(dir.as_str()).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
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
    let mut snare_total = Score { refs: 0, hits: 0, tp: 0, intro_fp: 0, delays: Vec::new() };
    let mut f1_sum = 0.0;
    let (mut seconds, mut compute) = (0.0, 0.0);
    for name in &names {
        let base = Path::new(dir.as_str()).join(name);
        let bytes = std::fs::read(base.with_extension("f32"))?;
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let refs = read_times(&base.with_extension("kicks"))?;
        let start = std::time::Instant::now();
        let (hits, snares) = detect(&samples, sens, gap);
        compute += start.elapsed().as_secs_f64();
        let duration = samples.len() as f64 / SR as f64;
        seconds += duration;
        let mut row = |kind: &str, s: &Score| match detail.as_mut() {
            Some(f) => writeln!(f, "{kind}\t{folder}\t{name}\t{}\t{}\t{}\t{duration:.1}", s.tp, s.hits, s.refs),
            None => Ok(()),
        };
        let doubt = |ext: &str| read_times(&base.with_extension(ext)).unwrap_or_default();
        let mut s = score(&refs, &doubt("kicks_doute"), &hits);
        row("kick", &s)?;
        if let Ok(snare_refs) = read_times(&base.with_extension("caisses")) {
            let mut c = score(&snare_refs, &doubt("caisses_doute"), &snares);
            row("caisse", &c)?;
            snare_total.refs += c.refs;
            snare_total.hits += c.hits;
            snare_total.tp += c.tp;
            snare_total.delays.append(&mut c.delays);
        }
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
    let minutes = seconds / 60.0;
    println!(
        "faux par minute : kicks {:.2}, caisses claires {:.2} ({:.0} min ; calcul {:.2} % du temps réel)",
        (total.hits - total.tp) as f64 / minutes,
        (snare_total.hits - snare_total.tp) as f64 / minutes,
        minutes,
        100.0 * compute / seconds
    );
    if snare_total.refs > 0 {
        println!(
            "CAISSES CLAIRES : réf {} dét {} préc {:.2} rappel {:.2} F1 {:.3} délai médian {:.0} ms",
            snare_total.refs,
            snare_total.hits,
            snare_total.precision(),
            snare_total.recall(),
            snare_total.f1(),
            median(&mut snare_total.delays) * 1000.0
        );
    }
    Ok(())
}
