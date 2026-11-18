//! Diagnostic de la capture et du kick sur le son réel :
//! `cargo run --release --example kick_probe -- [secondes=10]`.
//! Affiche chaque grosse caisse détectée (instant, intervalle), puis un résumé.

use audio_commun::capture::{self, Sink};
use audio_commun::kick::Analyzer;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

struct Probe {
    an: Option<Analyzer>,
    seen: u32,
    tx: Sender<Instant>,
}

impl Sink for Probe {
    fn open(&mut self, sample_rate: u32) {
        self.an = Some(Analyzer::new(sample_rate));
        self.seen = 0;
    }
    fn samples(&mut self, mono: &[f32]) {
        if let Some(a) = &mut self.an {
            a.push(mono);
        }
    }
    fn silence(&mut self, frames: usize) {
        if let Some(a) = &mut self.an {
            a.push_silence(frames);
        }
    }
    fn tick(&mut self, now: Instant) -> Duration {
        if let Some(a) = &self.an {
            for _ in self.seen..a.kicks {
                let _ = self.tx.send(now);
            }
            self.seen = a.kicks;
        }
        Duration::from_millis(10)
    }
    fn lost(&mut self) {}
}

fn main() -> anyhow::Result<()> {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let (tx, rx) = channel();
    let start = Instant::now();
    let _capture = capture::start("kick-probe", Probe { an: None, seen: 0, tx })?;
    let mut kicks: Vec<f64> = Vec::new();
    while start.elapsed() < Duration::from_secs(secs) {
        if let Ok(t) = rx.recv_timeout(Duration::from_millis(100)) {
            let at = t.duration_since(start).as_secs_f64();
            let gap = kicks.last().map(|p| format!(" (+{:.3} s)", at - p)).unwrap_or_default();
            println!("kick à {at:.3} s{gap}");
            kicks.push(at);
        }
    }
    let mut gaps: Vec<f64> = kicks.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_by(f64::total_cmp);
    match gaps.get(gaps.len() / 2) {
        Some(m) => println!("{} kicks, intervalle médian {m:.3} s (≈ {:.0} BPM)", kicks.len(), 60.0 / m),
        None => println!("{} kick(s)", kicks.len()),
    }
    Ok(())
}
