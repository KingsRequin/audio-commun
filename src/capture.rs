//! Capture d'une sortie son (WASAPI loopback, mode événement).
//!
//! Un fil dédié ouvre la sortie voulue en loopback (f32 stéréo convertie par Windows, puis
//! mélangée en mono), et remet chaque paquet au [`Sink`] dès son arrivée. Sans son, Windows
//! n'envoie aucun paquet : du silence est injecté pour la durée écoulée, afin que les niveaux
//! retombent. La sortie est revérifiée toutes les 2 s ; si elle change ou si une erreur
//! survient, le flux est rouvert 1 s plus tard. Le fil est inscrit auprès de MMCSS
//! (« Pro Audio ») pour être réveillé à l'heure même quand la machine est chargée.
//!
//! Sortie écoutée ([`Sortie`]) : celle par défaut de Windows (ses changements sont suivis), ou
//! une sortie précise choisie dans [`sorties`]. Si la sortie choisie disparaît (casque
//! débranché, carte désactivée), la capture se replie sur la sortie par défaut et revient sur
//! la choisie dès qu'elle est de nouveau active (vérifié toutes les 2 s).

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

/// Sortie son écoutée.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Sortie {
    /// La sortie par défaut de Windows, suivie quand elle change.
    #[default]
    ParDefaut,
    /// Une sortie précise, par son identifiant Windows (voir [`sorties`]).
    Id(String),
}

impl Sortie {
    /// Identifiant tel que rangé dans des réglages : vide = sortie par défaut.
    pub fn depuis_id(id: &str) -> Self {
        let id = id.trim();
        if id.is_empty() {
            Self::ParDefaut
        } else {
            Self::Id(id.to_owned())
        }
    }
}

/// Sorties son actives de Windows, `(identifiant, nom affiché)`, dans l'ordre de Windows ;
/// liste vide si Windows ne répond pas. Appelable depuis n'importe quel fil : COM y est
/// initialisé au besoin (et laissé tel quel s'il l'était déjà, en STA par exemple).
pub fn sorties() -> Vec<(String, String)> {
    let init = wasapi::initialize_mta();
    let liste = lire_sorties().unwrap_or_else(|e| {
        log::warn!("audio : liste des sorties illisible : {e:#}");
        Vec::new()
    });
    // S_OK ou S_FALSE : ce fil a pris une référence COM, à rendre. Échec (fil déjà dans un
    // autre appartement) : rien à rendre.
    if init.is_ok() {
        wasapi::deinitialize();
    }
    liste
}

fn lire_sorties() -> anyhow::Result<Vec<(String, String)>> {
    let enumerator = DeviceEnumerator::new()?;
    let collection = enumerator.get_device_collection(&Direction::Render)?;
    let mut liste = Vec::new();
    for device in &collection {
        let device = device?;
        let id = device.get_id()?;
        let nom = device.get_friendlyname().unwrap_or_else(|_| id.clone());
        liste.push((id, nom));
    }
    Ok(liste)
}

/// Lance la capture de la sortie par défaut dans un fil nommé `name`.
pub fn start(name: &str, sink: impl Sink) -> std::io::Result<Capture> {
    start_sur(name, Sortie::ParDefaut, sink)
}

/// Lance la capture de `sortie` dans un fil nommé `name` (sur la sortie par défaut tant que
/// la sortie choisie est absente).
pub fn start_sur(name: &str, sortie: Sortie, sink: impl Sink) -> std::io::Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || run(sink, &sortie, &s))?;
    Ok(Capture {
        stop,
        thread: Some(thread),
    })
}

/// Au-delà, l'absence de paquet veut dire que rien n'est joué.
const NO_DATA: Duration = Duration::from_millis(30);
/// Vérification de la sortie (changement de casque, d'enceintes, retour de la sortie choisie).
const DEVICE_CHECK: Duration = Duration::from_secs(2);
/// Plus longue attente d'un paquet : borne aussi le délai d'arrêt.
const MAX_WAIT_MS: u128 = 20;
/// Erreurs répétées : une ligne de journal toutes les 30 s au plus.
const ERROR_LOG: Duration = Duration::from_secs(30);

fn run(mut sink: impl Sink, sortie: &Sortie, stop: &AtomicBool) {
    if wasapi::initialize_mta().is_err() {
        log::warn!("audio : initialisation COM impossible");
    }
    let _mmcss = Mmcss::register();
    let mut last_error_log: Option<Instant> = None;
    let mut hidden = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = stream(&mut sink, sortie, stop) {
            let now = Instant::now();
            if last_error_log.is_none_or(|t| now.duration_since(t) >= ERROR_LOG) {
                let masked = if hidden > 0 {
                    format!(" ({hidden} masquées)")
                } else {
                    String::new()
                };
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

/// La sortie `id` existe-t-elle et est-elle active ?
fn active(enumerator: &DeviceEnumerator, id: &str) -> bool {
    enumerator
        .get_device(id)
        .and_then(|d| d.get_state())
        .is_ok_and(|s| matches!(s, wasapi::DeviceState::Active))
}

/// Ce que surveille une session ouverte : la sortie par défaut (suivie, ou repli faute de la
/// sortie choisie), ou la sortie choisie elle-même.
#[derive(Debug, PartialEq, Eq)]
enum Suivi<'a> {
    Defaut { choisie_absente: Option<&'a str> },
    Choisie(&'a str),
}

/// Sortie à ouvrir : la choisie si elle est active, sinon celle par défaut.
fn suivi<'a>(sortie: &'a Sortie, choisie_active: impl FnOnce(&str) -> bool) -> Suivi<'a> {
    match sortie {
        Sortie::ParDefaut => Suivi::Defaut {
            choisie_absente: None,
        },
        Sortie::Id(id) if choisie_active(id) => Suivi::Choisie(id),
        Sortie::Id(id) => Suivi::Defaut {
            choisie_absente: Some(id),
        },
    }
}

/// Une session de capture. `Ok` = arrêt demandé ; `Err` = à relancer.
fn stream(sink: &mut impl Sink, sortie: &Sortie, stop: &AtomicBool) -> anyhow::Result<()> {
    let enumerator = DeviceEnumerator::new()?;
    let suivi = suivi(sortie, |id| active(&enumerator, id));
    let device = match suivi {
        Suivi::Choisie(id) => enumerator.get_device(id)?,
        Suivi::Defaut { .. } => enumerator.get_default_device(&Direction::Render)?,
    };
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
    let repli = if matches!(
        suivi,
        Suivi::Defaut {
            choisie_absente: Some(_)
        }
    ) {
        " ; sortie choisie absente, sortie par défaut en attendant"
    } else {
        ""
    };
    log::info!(
        "audio : loopback ouvert sur « {} » ({sample_rate} Hz){repli}",
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
            match suivi {
                Suivi::Choisie(id) => {
                    if !active(&enumerator, id) {
                        break Err(anyhow::anyhow!("sortie choisie disparue"));
                    }
                }
                Suivi::Defaut { choisie_absente } => {
                    if choisie_absente.is_some_and(|id| active(&enumerator, id)) {
                        break Err(anyhow::anyhow!("sortie choisie de nouveau active"));
                    }
                    let current = enumerator
                        .get_default_device(&Direction::Render)
                        .and_then(|d| d.get_id());
                    if current.as_deref().ok() != Some(device_id.as_str()) {
                        break Err(anyhow::anyhow!("sortie son par défaut changée"));
                    }
                }
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
            let _ =
                unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mix_mono, suivi, Sortie, Suivi};

    #[test]
    fn identifiant_vide_veut_dire_sortie_par_defaut() {
        assert_eq!(Sortie::depuis_id(""), Sortie::ParDefaut);
        assert_eq!(Sortie::depuis_id("  "), Sortie::ParDefaut);
        assert_eq!(
            Sortie::depuis_id("{0.0.0}.{abc}"),
            Sortie::Id("{0.0.0}.{abc}".into())
        );
    }

    #[test]
    fn sortie_choisie_absente_repli_sur_la_sortie_par_defaut() {
        let choisie = Sortie::Id("casque".into());
        assert_eq!(suivi(&choisie, |_| true), Suivi::Choisie("casque"));
        assert_eq!(
            suivi(&choisie, |_| false),
            Suivi::Defaut {
                choisie_absente: Some("casque")
            }
        );
        // sortie par défaut : aucune sortie choisie à vérifier
        let mut demande = false;
        let s = suivi(&Sortie::ParDefaut, |_| {
            demande = true;
            true
        });
        assert_eq!(
            s,
            Suivi::Defaut {
                choisie_absente: None
            }
        );
        assert!(!demande);
    }

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
