//! Capture de la sortie son par défaut (WASAPI loopback, mode événement).
//!
//! Un fil dédié ouvre la sortie par défaut en loopback (f32 stéréo convertie par Windows, puis
//! mélangée en mono), et remet chaque paquet au [`Sink`] dès son arrivée. Sans son, Windows
//! n'envoie aucun paquet : du silence est injecté pour la durée écoulée, afin que les niveaux
//! retombent. La sortie par défaut est revérifiée toutes les 2 s ; si elle change ou si une
//! erreur survient, le flux est rouvert 1 s plus tard. Le fil est inscrit auprès de MMCSS
//! (« Pro Audio ») pour être réveillé à l'heure même quand la machine est chargée.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

/// Ce que l'application fait du son. Toutes les méthodes sont appelées dans le fil de capture.
pub trait Sink: Send + 'static {
    /// Un flux vient d'être ouvert à cette fréquence d'échantillonnage.
    fn open(&mut self, sample_rate: u32);
    /// Échantillons mono reçus.
    fn samples(&mut self, mono: &[f32]);
    /// `frames` échantillons de silence (paquet marqué silencieux, ou rien n'est joué).
    fn silence(&mut self, frames: usize);
    /// Après chaque réveil du fil (paquets lus ou délai écoulé). Renvoie le délai voulu avant le
    /// prochain réveil, s'il ne vient aucun paquet (borné entre 1 et 20 ms).
    fn tick(&mut self, now: Instant) -> Duration;
    /// Le flux est perdu (erreur, sortie changée) ; un nouvel essai suit.
    fn lost(&mut self);
}

/// Capture en cours ; l'arrêter = la `drop` (au plus ~20 ms d'attente).
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Lance la capture dans un fil nommé `name`.
pub fn start(name: &str, sink: impl Sink) -> std::io::Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || run(sink, &s))?;
    Ok(Capture {
        stop,
        thread: Some(thread),
    })
}

/// Au-delà, l'absence de paquet veut dire que rien n'est joué.
const NO_DATA: Duration = Duration::from_millis(30);
/// Vérification de la sortie par défaut (changement de casque, d'enceintes…).
const DEVICE_CHECK: Duration = Duration::from_secs(2);
/// Plus longue attente d'un paquet : borne aussi le délai d'arrêt.
const MAX_WAIT_MS: u128 = 20;
/// Erreurs répétées : une ligne de journal toutes les 30 s au plus.
const ERROR_LOG: Duration = Duration::from_secs(30);

fn run(mut sink: impl Sink, stop: &AtomicBool) {
    if wasapi::initialize_mta().is_err() {
        log::warn!("audio : initialisation COM impossible");
    }
    let _mmcss = Mmcss::register();
    let mut last_error_log: Option<Instant> = None;
    let mut hidden = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = stream(&mut sink, stop) {
            let now = Instant::now();
            if last_error_log.is_none_or(|t| now.duration_since(t) >= ERROR_LOG) {
                let masked = if hidden > 0 { format!(" ({hidden} masquées)") } else { String::new() };
                log::warn!("audio : capture interrompue : {e:#}{masked}, nouvel essai");
                last_error_log = Some(now);
                hidden = 0;
            } else {
                hidden += 1;
            }
            sink.lost();
            // Attente découpée pour rester réactif à l'arrêt.
            for _ in 0..50 {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    wasapi::deinitialize();
}

/// Une session de capture sur la sortie par défaut. `Ok` = arrêt demandé ; `Err` = à relancer.
fn stream(sink: &mut impl Sink, stop: &AtomicBool) -> anyhow::Result<()> {
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(&Direction::Render)?;
    let device_id = device.get_id()?;
    let mut client = device.get_iaudioclient()?;
    let sample_rate = client.get_mixformat()?.get_samplespersec();
    let format = WaveFormat::new(32, 32, &SampleType::Float, sample_rate as usize, 2, None);
    let (default_period, _) = client.get_device_period()?;
    client.initialize_client(
        &format,
        &Direction::Capture,
        &StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period * 4,
        },
    )?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    let frames_max = client.get_buffer_size()? as usize;
    let mut bytes = vec![0u8; frames_max * 8];
    let mut mono = vec![0.0f32; frames_max];
    sink.open(sample_rate);
    client.start_stream()?;
    log::info!(
        "audio : loopback ouvert sur « {} » ({sample_rate} Hz)",
        device.get_friendlyname().unwrap_or_default()
    );

    let mut last_data = Instant::now();
    let mut device_checked = Instant::now();
    let mut wait = Duration::from_millis(10);
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let wait_ms = wait.as_millis().clamp(1, MAX_WAIT_MS) as u32;
        // Délai dépassé = pas de paquet (rien n'est joué) : ce n'est pas une erreur.
        if event.wait_for_event(wait_ms).is_ok() {
            if let Err(e) = drain(&capture, &mut bytes, &mut mono, sink) {
                break Err(e);
            }
            last_data = Instant::now();
        } else {
            let silent_for = last_data.elapsed();
            if silent_for > NO_DATA {
                let n = (silent_for.as_secs_f64() * f64::from(sample_rate)) as usize;
                sink.silence(n.min(sample_rate as usize / 10));
                last_data = Instant::now();
            }
        }
        let now = Instant::now();
        wait = sink.tick(now);
        if now.duration_since(device_checked) >= DEVICE_CHECK {
            device_checked = now;
            let current = enumerator
                .get_default_device(&Direction::Render)
                .and_then(|d| d.get_id());
            if current.as_deref().ok() != Some(device_id.as_str()) {
                break Err(anyhow::anyhow!("sortie son par défaut changée"));
            }
        }
    };
    let _ = client.stop_stream();
    result
}

/// Vide les paquets disponibles (mélange mono L+R).
fn drain(
    capture: &wasapi::AudioCaptureClient,
    bytes: &mut [u8],
    mono: &mut [f32],
    sink: &mut impl Sink,
) -> anyhow::Result<()> {
    while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
        let (frames, info) = capture.read_from_device(bytes)?;
        let frames = frames as usize;
        if info.flags.silent {
            sink.silence(frames);
        } else {
            mix_mono(&bytes[..frames * 8], &mut mono[..frames]);
            sink.samples(&mono[..frames]);
        }
    }
    Ok(())
}

/// Stéréo f32 entrelacée (octets little-endian) → mono.
fn mix_mono(stereo: &[u8], mono: &mut [f32]) {
    for (m, fr) in mono.iter_mut().zip(stereo.chunks_exact(8)) {
        let l = f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]]);
        let r = f32::from_le_bytes([fr[4], fr[5], fr[6], fr[7]]);
        *m = 0.5 * (l + r);
    }
}

/// Inscription du fil auprès du planificateur multimédia (MMCSS, tâche « Pro Audio »), retirée
/// à la fin. Échec sans conséquence : le fil reste en priorité normale.
struct Mmcss(Option<windows::Win32::Foundation::HANDLE>);

impl Mmcss {
    fn register() -> Self {
        use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let mut task = 0u32;
        // SAFETY: chaîne statique terminée par zéro, index de tâche valide pour la durée de l'appel.
        match unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task) } {
            Ok(h) => Self(Some(h)),
            Err(e) => {
                log::debug!("audio : MMCSS indisponible ({e})");
                Self(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: poignée rendue par AvSetMmThreadCharacteristicsW sur ce même fil.
            let _ = unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mix_mono;

    #[test]
    fn mono_est_la_moyenne_des_deux_canaux() {
        let mut stereo = Vec::new();
        for (l, r) in [(1.0f32, 0.0f32), (0.5, -0.5), (-1.0, -1.0)] {
            stereo.extend_from_slice(&l.to_le_bytes());
            stereo.extend_from_slice(&r.to_le_bytes());
        }
        let mut mono = [9.0f32; 3];
        mix_mono(&stereo, &mut mono);
        assert_eq!(mono, [0.5, 0.0, -1.0]);
    }
}
