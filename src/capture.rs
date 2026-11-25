//! Capture d'une sortie son (WASAPI loopback, mode événement).
//!
//! Un fil dédié ouvre la sortie voulue en loopback (f32 stéréo convertie par Windows, puis
//! mélangée en mono), et remet chaque paquet au [`Sink`] dès son arrivée, avec le niveau de
//! chaque canal ([`Sink::stereo`]). Sans son, Windows
//! n'envoie aucun paquet : du silence est injecté pour la durée écoulée, afin que les niveaux
//! retombent. Le client est interrogé au moins toutes les 250 ms même sans paquet : une sortie
//! invalidée (format changé, son spatial, mode exclusif, pilote relancé) se voit ainsi aussitôt.
//! Un changement de sortie par défaut (signalé par Windows, revérifié toutes les 2 s) rouvre le
//! flux tout de suite, sans pause ; après une erreur, il est rouvert 1 s plus tard. Le fil est
//! inscrit auprès de MMCSS (« Pro Audio ») pour être réveillé à l'heure même quand la machine
//! est chargée.
//!
//! Sortie écoutée ([`Sortie`]) : celle par défaut de Windows (ses changements sont suivis), ou
//! une sortie précise choisie dans [`sorties`]. Si la sortie choisie disparaît (casque
//! débranché, carte désactivée), la capture se replie sur la sortie par défaut et revient sur
//! la choisie dès qu'elle est de nouveau active (vérifié toutes les 2 s).
//!
//! Ou seulement le son de certaines applications ([`Ecoute::Applis`], [`start_ecoute`]) :
//! capture « process loopback » de Windows 10 2004+, détaillée dans le module `applis` ;
//! [`applis_sonores`] liste les applications qui ont une session audio.
//!
//! Une panique dans le fil est rattrapée : [`Sink::lost`], puis le fil se termine
//! ([`Capture::actif`] le dit, à l'application de relancer la capture).

mod applis;

pub use applis::{applis_sonores, applis_sonores_detail, AppliSonore};

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
    /// Le flux est perdu (erreur) ; un nouvel essai suit. Pas appelé quand la sortie écoutée
    /// change (sortie par défaut, sortie choisie partie ou revenue) : [`Sink::open`] suit
    /// aussitôt.
    fn lost(&mut self);
    /// Niveaux efficaces (RMS) des canaux gauche et droit du paquet qui vient d'être remis à
    /// [`Sink::samples`] : de quel côté joue le son. Rien par défaut.
    fn stereo(&mut self, _gauche: f32, _droite: f32) {}
    /// Écoute par application ([`Ecoute::Applis`]) : `true` quand Windows la refuse et que toute
    /// la sortie par défaut est écoutée à la place (repli), `false` dès que la capture par
    /// application fonctionne (au premier essai réussi, repli ou non : l'état part toujours d'une
    /// valeur sûre). Rien par défaut.
    fn repli(&mut self, _actif: bool) {}
}

/// Capture en cours ; l'arrêter = la `drop` : au plus ~300 ms d'attente, au-delà le fil est
/// laissé finir seul (il s'arrête à son prochain réveil).
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Le fil de capture tourne-t-il encore ? `false` s'il s'est arrêté de lui-même (panique :
    /// [`Sink::lost`] a été appelé, plus rien ne sera remis) : à relancer.
    pub fn actif(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

/// Attente au plus de la fin du fil de capture quand elle est lâchée.
const ARRET_MAX: Duration = Duration::from_millis(300);

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(t) = self.thread.take() else { return };
        let debut = Instant::now();
        while !t.is_finished() {
            if debut.elapsed() >= ARRET_MAX {
                // Appel Windows bloqué (activation, pilote) : ne pas figer l'appelant.
                log::warn!(
                    "audio : fil de capture « {} » toujours actif après {} ms, laissé finir seul",
                    t.thread().name().unwrap_or("?"),
                    ARRET_MAX.as_millis()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = t.join();
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

/// Ce que la capture écoute : tout le son d'une sortie, ou seulement celui de certaines
/// applications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ecoute {
    /// Tout le mélange de Windows sur cette sortie.
    Sortie(Sortie),
    /// Le son de ces applications seulement, par nom d'exécutable (`spotify.exe`, ou
    /// `spotify` : l'extension `.exe` est sous-entendue ; casse indifférente), quelle que soit la
    /// sortie sur laquelle elles jouent. Une appli absente est attendue (ajoutée dès qu'elle est
    /// lancée, vérifié toutes les 2 s) ; tant qu'aucune ne tourne, du silence est remis. Liste
    /// vide = silence (voir [`Ecoute::depuis_reglages`] pour « aucune appli = tout »).
    Applis(Vec<String>),
}

impl Default for Ecoute {
    fn default() -> Self {
        Self::Sortie(Sortie::ParDefaut)
    }
}

impl Ecoute {
    /// D'après des réglages : les applis données (noms vides ignorés), ou à défaut toute la
    /// sortie `sortie_id` (vide = sortie par défaut, comme [`Sortie::depuis_id`]).
    pub fn depuis_reglages<S: AsRef<str>>(sortie_id: &str, applis: &[S]) -> Self {
        let applis: Vec<String> = applis
            .iter()
            .map(|a| a.as_ref().trim())
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect();
        if applis.is_empty() {
            Self::Sortie(Sortie::depuis_id(sortie_id))
        } else {
            Self::Applis(applis)
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
    start_ecoute(name, Ecoute::Sortie(sortie), sink)
}

/// Lance l'écoute de `ecoute` dans un fil nommé `name`. Pour [`Ecoute::Applis`] : fréquence
/// fixe de 48 000 Hz (Windows convertit), son des applis additionné ; si Windows refuse la
/// capture par application, une ligne de journal et toute la sortie par défaut est écoutée à la
/// place ([`Sink::repli`]) : jusqu'à l'arrêt si elle n'est pas prise en charge (version
/// antérieure à Windows 10 2004), sinon (refus passager) la capture par application est
/// réessayée toutes les 30 s dans un fil à part, sans couper le flux de la sortie par défaut, qui
/// n'est quitté qu'une fois l'essai réussi.
///
/// Une panique dans le fil (dans le `Sink` ou la capture) est rattrapée : [`Sink::lost`] est
/// appelé et le fil se termine ([`Capture::actif`] passe à `false`).
pub fn start_ecoute(name: &str, ecoute: Ecoute, sink: impl Sink) -> std::io::Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || run(sink, &ecoute, &s))?;
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
/// Sans paquet, le client est interrogé au moins à ce rythme (sortie invalidée).
const SONDE: Duration = Duration::from_millis(250);
/// Repli passager sur la sortie par défaut : intervalle entre deux essais de la capture par
/// application (faits sans quitter la sortie par défaut).
const REPLI_ESSAI: Duration = Duration::from_secs(30);

fn run(mut sink: impl Sink, ecoute: &Ecoute, stop: &AtomicBool) {
    if wasapi::initialize_mta().is_err() {
        log::warn!("audio : initialisation COM impossible");
    }
    let mmcss = Mmcss::register();
    sans_panique(&mut sink, |sink| match ecoute {
        Ecoute::Sortie(sortie) => ecouter_sortie(sink, sortie, stop, None),
        Ecoute::Applis(noms) => ecouter_applis(
            sink,
            stop,
            applis::essai(),
            |sink, retour| ecouter_sortie(sink, &Sortie::ParDefaut, stop, retour),
            |sink| applis::ecouter(sink, noms, stop),
        ),
    });
    drop(mmcss);
    wasapi::deinitialize();
}

/// Lance `f` ; s'il panique, le `Sink` apprend que le flux est perdu ([`Sink::lost`]) et le fil
/// pourra se terminer (le message est déjà passé par le crochet de panique). Renvoie `false`
/// après une panique.
fn sans_panique<S: Sink>(sink: &mut S, f: impl FnOnce(&mut S)) -> bool {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    if catch_unwind(AssertUnwindSafe(|| f(&mut *sink))).is_ok() {
        return true;
    }
    log::error!("audio : le fil de capture a planté, capture arrêtée");
    // Le `Sink` peut être dans un état incohérent : une seconde panique ne doit pas sortir.
    if catch_unwind(AssertUnwindSafe(|| sink.lost())).is_err() {
        log::error!("audio : `lost` a planté à son tour");
    }
    false
}

/// Écoute par application. `essai` : réponse de Windows à un premier essai de capture par
/// application ; `sortie` écoute toute la sortie par défaut (repli) jusqu'à l'arrêt, ou jusqu'à ce
/// que l'essai périodique qu'on lui donne réussisse ; `applis` écoute les applis jusqu'à l'arrêt.
/// Si Windows refuse : toute la sortie par défaut, pour de bon si la capture par application
/// n'est pas prise en charge, sinon jusqu'à ce qu'un essai (toutes les 30 s, dans un fil à part)
/// réussisse.
fn ecouter_applis<S: Sink>(
    sink: &mut S,
    stop: &AtomicBool,
    essai: anyhow::Result<()>,
    sortie: impl FnOnce(&mut S, Option<&mut EssaiApplis>),
    applis: impl FnOnce(&mut S),
) {
    if let Err(e) = essai {
        let definitif = applis::non_prise_en_charge(&e);
        let suite = if definitif {
            "non prise en charge par cette version de Windows"
        } else {
            "nouvel essai toutes les 30 s"
        };
        log::warn!(
            "audio : capture par application impossible ({e:#}), toute la sortie par défaut est \
             écoutée ; {suite}"
        );
        sink.lost();
        sink.repli(true);
        let mut retour = (!definitif).then(|| EssaiApplis::new(applis::essai, REPLI_ESSAI));
        sortie(sink, retour.as_mut());
        if stop.load(Ordering::Relaxed) || !retour.is_some_and(|r| r.reussi) {
            return;
        }
        log::info!("audio : capture par application rétablie");
    }
    // Toujours, même sans repli dans ce fil : l'état annoncé part d'une valeur sûre.
    sink.repli(false);
    applis(sink);
}

/// Essai périodique de la capture par application pendant un repli, dans un fil à part :
/// l'activation peut bloquer, et le son de la sortie par défaut doit continuer de passer.
struct EssaiApplis {
    essai: fn() -> anyhow::Result<()>,
    intervalle: Duration,
    /// Prochain essai ; `None` : abandonné (Windows ne connaît pas cette capture).
    prochain: Option<Instant>,
    fil: Option<JoinHandle<anyhow::Result<()>>>,
    /// Un essai a réussi : le repli peut prendre fin (reste vrai).
    reussi: bool,
}

impl EssaiApplis {
    fn new(essai: fn() -> anyhow::Result<()>, intervalle: Duration) -> Self {
        Self {
            essai,
            intervalle,
            prochain: Some(Instant::now() + intervalle),
            fil: None,
            reussi: false,
        }
    }

    /// À appeler souvent : relève l'essai fini, lance le suivant quand il est temps. Vrai dès
    /// qu'un essai a réussi.
    fn avancer(&mut self, now: Instant) -> bool {
        if self.reussi {
            return true;
        }
        if let Some(fil) = self.fil.take_if(|f| f.is_finished()) {
            match fil.join() {
                Ok(Ok(())) => {
                    self.reussi = true;
                    return true;
                }
                Ok(Err(e)) if applis::non_prise_en_charge(&e) => {
                    log::warn!("audio : capture par application non prise en charge ({e:#})");
                    self.prochain = None;
                }
                Ok(Err(e)) => {
                    log::debug!("audio : capture par application toujours impossible ({e:#})");
                    self.prochain = Some(now + self.intervalle);
                }
                Err(_) => self.prochain = Some(now + self.intervalle),
            }
        }
        if self.fil.is_none() && self.prochain.is_some_and(|t| now >= t) {
            let essai = self.essai;
            let lance = std::thread::Builder::new()
                .name("audio-essai-applis".into())
                .spawn(move || {
                    let init = wasapi::initialize_mta();
                    let r = essai();
                    if init.is_ok() {
                        wasapi::deinitialize();
                    }
                    r
                });
            match lance {
                Ok(f) => self.fil = Some(f),
                Err(e) => {
                    log::debug!("audio : essai de capture par application non lancé ({e})");
                    self.prochain = Some(now + self.intervalle);
                }
            }
        }
        false
    }
}

/// Écoute toute une sortie jusqu'à l'arrêt, en relançant le flux après chaque erreur ou
/// changement de sortie. Avec `retour` (repli des applis) : aussi jusqu'à ce que l'essai de la
/// capture par application réussisse, sans couper le flux en attendant.
fn ecouter_sortie(
    sink: &mut impl Sink,
    sortie: &Sortie,
    stop: &AtomicBool,
    mut retour: Option<&mut EssaiApplis>,
) {
    let mut fini = || {
        stop.load(Ordering::Relaxed)
            || retour
                .as_deref_mut()
                .is_some_and(|r| r.avancer(Instant::now()))
    };
    let mut last_error_log: Option<Instant> = None;
    let mut hidden = 0u32;
    while !fini() {
        match stream(sink, sortie, &mut fini) {
            Ok(Fin::Arret) => {}
            Ok(Fin::Relancer(raison)) => {
                // Pas une panne : rouvert aussitôt, sans perte annoncée.
                log::info!("audio : {raison}, flux rouvert");
            }
            Err(e) => {
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
                    if fini() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

/// Fin d'une session de capture sans erreur.
#[derive(Debug, PartialEq, Eq)]
enum Fin {
    /// Arrêt demandé (ou fin du repli des applis).
    Arret,
    /// La sortie à écouter a changé : rouvrir aussitôt, ce n'est pas une panne.
    Relancer(&'static str),
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

/// Une session de capture. `Ok` : arrêt demandé, ou sortie changée (à rouvrir aussitôt) ;
/// `Err` : panne, à rouvrir après une pause.
fn stream(
    sink: &mut impl Sink,
    sortie: &Sortie,
    fini: &mut impl FnMut() -> bool,
) -> anyhow::Result<Fin> {
    let enumerator = DeviceEnumerator::new()?;
    let suivi = suivi(sortie, |id| active(&enumerator, id));
    let device = match suivi {
        Suivi::Choisie(id) => enumerator.get_device(id)?,
        Suivi::Defaut { .. } => enumerator.get_default_device(&Direction::Render)?,
    };
    let device_id = device.get_id()?;
    // Sortie par défaut suivie : Windows signale son changement aussitôt (la vérification
    // toutes les 2 s reste en secours si l'inscription échoue).
    let defaut_change = Arc::new(AtomicBool::new(false));
    let _avis = match suivi {
        Suivi::Defaut { .. } => avis_sortie_par_defaut(&enumerator, &defaut_change),
        Suivi::Choisie(_) => None,
    };
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
    let mut sonde = Instant::now();
    let mut device_checked = Instant::now();
    let mut wait = Duration::from_millis(10);
    let result = loop {
        if fini() {
            break Ok(Fin::Arret);
        }
        if defaut_change.load(Ordering::Relaxed) {
            break Ok(Fin::Relancer("sortie son par défaut changée"));
        }
        let wait_ms = wait.as_millis().clamp(1, MAX_WAIT_MS) as u32;
        // Délai dépassé = pas de paquet (rien n'est joué) : ce n'est pas une erreur.
        if event.wait_for_event(wait_ms).is_ok() {
            if let Err(e) = drain(&capture, &mut bytes, &mut mono, sink) {
                break Err(e);
            }
            last_data = Instant::now();
            sonde = last_data;
        } else {
            // Une sortie invalidée (format changé, son spatial, mode exclusif, pilote relancé)
            // n'envoie plus rien : seul le client le dit.
            if sonde.elapsed() >= SONDE {
                sonde = Instant::now();
                if let Err(e) = capture.get_next_packet_size() {
                    break Err(anyhow::Error::from(e).context("sortie invalidée"));
                }
            }
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
                        break Ok(Fin::Relancer("sortie choisie disparue"));
                    }
                }
                Suivi::Defaut { choisie_absente } => {
                    if choisie_absente.is_some_and(|id| active(&enumerator, id)) {
                        break Ok(Fin::Relancer("sortie choisie de nouveau active"));
                    }
                    match enumerator
                        .get_default_device(&Direction::Render)
                        .and_then(|d| d.get_id())
                    {
                        Ok(id) if id != device_id => {
                            break Ok(Fin::Relancer("sortie son par défaut changée"));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            break Err(anyhow::Error::from(e).context("plus de sortie par défaut"))
                        }
                    }
                }
            }
        }
    };
    let _ = client.stop_stream();
    result
}

/// Inscription auprès de Windows : `change` passe à `true` quand la sortie son par défaut
/// (rôle « console », celle qu'ouvre [`stream`]) change. `None` si Windows la refuse.
fn avis_sortie_par_defaut(
    enumerator: &DeviceEnumerator,
    change: &Arc<AtomicBool>,
) -> Option<wasapi::DeviceEventRegistration> {
    let mut rappels = wasapi::DeviceEventCallbacks::new();
    let c = change.clone();
    // Appelé dans un fil de Windows : ne fait que lever le drapeau.
    rappels.set_default_device_callback(move |direction, role, _id| {
        if direction == Direction::Render && role == wasapi::Role::Console {
            c.store(true, Ordering::Relaxed);
        }
    });
    enumerator
        .register_notification_callback(rappels)
        .inspect_err(|e| log::debug!("audio : changements de sortie non signalés ({e})"))
        .ok()
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
            let (gauche, droite) = mix_mono(&bytes[..frames * 8], &mut mono[..frames]);
            sink.samples(&mono[..frames]);
            sink.stereo(gauche, droite);
        }
    }
    Ok(())
}

/// Stéréo f32 entrelacée (octets little-endian) → mono ; renvoie le niveau efficace (RMS) de
/// chaque canal (gauche, droite). Les valeurs non finies ne comptent pas dans les niveaux.
fn mix_mono(stereo: &[u8], mono: &mut [f32]) -> (f32, f32) {
    mono_et_niveaux(trames(stereo), mono)
}

/// Stéréo f32 entrelacée (octets little-endian) → trames `[gauche, droite]`.
fn trames(stereo: &[u8]) -> impl Iterator<Item = [f32; 2]> + '_ {
    stereo.chunks_exact(8).map(|fr| {
        [
            f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]]),
            f32::from_le_bytes([fr[4], fr[5], fr[6], fr[7]]),
        ]
    })
}

/// Trames stéréo → mono (moyenne des deux canaux) ; renvoie le niveau efficace de chaque canal.
fn mono_et_niveaux(trames: impl Iterator<Item = [f32; 2]>, mono: &mut [f32]) -> (f32, f32) {
    let (mut sl, mut sr, mut n) = (0.0f32, 0.0f32, 0usize);
    for (m, [l, r]) in mono.iter_mut().zip(trames) {
        *m = 0.5 * (l + r);
        if l.is_finite() && r.is_finite() {
            (sl, sr) = (sl + l * l, sr + r * r);
        }
        n += 1;
    }
    let n = n.max(1) as f32;
    ((sl / n).sqrt(), (sr / n).sqrt())
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
    use super::{
        ecouter_applis, mix_mono, sans_panique, suivi, Capture, Ecoute, EssaiApplis, Sink, Sortie,
        Suivi, ARRET_MAX,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// `Sink` qui note les appels qui comptent pour l'état (ouverture, perte, repli).
    #[derive(Default)]
    struct Journal(Vec<String>);

    impl Sink for Journal {
        fn open(&mut self, fs: u32) {
            self.0.push(format!("open {fs}"));
        }
        fn samples(&mut self, _: &[f32]) {}
        fn silence(&mut self, _: usize) {}
        fn tick(&mut self, _: Instant) -> Duration {
            Duration::from_millis(10)
        }
        fn lost(&mut self) {
            self.0.push("lost".into());
        }
        fn repli(&mut self, actif: bool) {
            self.0.push(format!("repli {actif}"));
        }
    }

    fn refus(code: u32) -> anyhow::Error {
        let w = windows::core::Error::from(windows::core::HRESULT(code as i32));
        anyhow::Error::from(wasapi::WasapiError::Windows(w))
    }
    /// Accès refusé (passager) et classe non enregistrée (Windows trop ancien).
    const PASSAGER: u32 = 0x8007_0005;
    const DEFINITIF: u32 = 0x8004_0154;

    #[test]
    fn repli_leve_et_baisse_au_premier_essai_reussi() {
        let stop = AtomicBool::new(false);
        // essai réussi d'emblée : `repli(false)` quand même, puis les applis
        let mut j = Journal::default();
        let mut applis = false;
        ecouter_applis(
            &mut j,
            &stop,
            Ok(()),
            |_, _| panic!("pas de repli"),
            |_| applis = true,
        );
        assert_eq!(j.0, ["repli false"]);
        assert!(applis);

        // refus passager : repli, sortie par défaut avec essai périodique ; essai réussi
        let mut j = Journal::default();
        let mut applis = false;
        ecouter_applis(
            &mut j,
            &stop,
            Err(refus(PASSAGER)),
            |s: &mut Journal, retour| {
                s.0.push("sortie".into());
                // le flux de la sortie par défaut reste ouvert jusqu'à l'essai réussi
                retour.expect("essai périodique").reussi = true;
            },
            |_| applis = true,
        );
        assert_eq!(j.0, ["lost", "repli true", "sortie", "repli false"]);
        assert!(applis);

        // refus définitif : sortie par défaut jusqu'à l'arrêt, sans essai ni retour
        let mut j = Journal::default();
        ecouter_applis(
            &mut j,
            &stop,
            Err(refus(DEFINITIF)),
            |s: &mut Journal, retour| {
                assert!(retour.is_none());
                s.0.push("sortie".into());
                stop.store(true, Ordering::Relaxed);
            },
            |_| panic!("pas d'applis"),
        );
        assert_eq!(j.0, ["lost", "repli true", "sortie"]);
    }

    /// Attend jusqu'à 2 s que l'essai réussisse ; renvoie s'il a réussi.
    fn essayer(e: &mut EssaiApplis, pendant: Duration) -> bool {
        let debut = Instant::now();
        while debut.elapsed() < pendant {
            if e.avancer(Instant::now()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn essai_periodique_des_applis() {
        static PASSAGERS: AtomicU32 = AtomicU32::new(0);
        static DEFINITIFS: AtomicU32 = AtomicU32::new(0);
        fn reussit_au_troisieme() -> anyhow::Result<()> {
            if PASSAGERS.fetch_add(1, Ordering::Relaxed) < 2 {
                Err(refus(PASSAGER))
            } else {
                Ok(())
            }
        }
        fn jamais() -> anyhow::Result<()> {
            DEFINITIFS.fetch_add(1, Ordering::Relaxed);
            Err(refus(DEFINITIF))
        }
        let mut e = EssaiApplis::new(reussit_au_troisieme, Duration::from_millis(5));
        assert!(essayer(&mut e, Duration::from_secs(2)));
        assert_eq!(PASSAGERS.load(Ordering::Relaxed), 3);
        assert!(e.avancer(Instant::now()), "la réussite reste acquise");
        // pas d'essai avant l'intervalle
        let mut e = EssaiApplis::new(reussit_au_troisieme, Duration::from_secs(60));
        assert!(!essayer(&mut e, Duration::from_millis(30)));
        assert!(e.fil.is_none());
        // Windows ne connaît pas cette capture : un seul essai, puis abandon
        let mut e = EssaiApplis::new(jamais, Duration::from_millis(5));
        assert!(!essayer(&mut e, Duration::from_millis(200)));
        assert_eq!(DEFINITIFS.load(Ordering::Relaxed), 1);
        assert!(e.prochain.is_none());
    }

    #[test]
    fn panique_du_fil_annoncee_comme_perte() {
        let mut j = Journal::default();
        assert!(sans_panique(&mut j, |s| s.open(48_000)));
        assert_eq!(j.0, ["open 48000"]);
        let mut j = Journal::default();
        assert!(!sans_panique(&mut j, |s| {
            s.open(48_000);
            panic!("essai : panique voulue");
        }));
        assert_eq!(j.0, ["open 48000", "lost"]);
        // une capture dont le fil s'est terminé n'est plus active
        let c = Capture {
            stop: Arc::new(AtomicBool::new(false)),
            thread: Some(std::thread::spawn(|| {})),
        };
        let debut = Instant::now();
        while c.actif() {
            assert!(debut.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(1));
        }
        let c = capture_lente(Duration::ZERO);
        assert!(c.actif());
    }

    /// Capture factice dont le fil met `duree` à voir l'arrêt.
    fn capture_lente(duree: Duration) -> Capture {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let debut = Instant::now();
            while !s.load(Ordering::Relaxed) || debut.elapsed() < duree {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Capture {
            stop,
            thread: Some(thread),
        }
    }

    #[test]
    fn arret_borne_meme_si_le_fil_est_bloque() {
        // fil qui s'arrête vite : attendu jusqu'au bout
        let debut = Instant::now();
        drop(capture_lente(Duration::ZERO));
        assert!(debut.elapsed() < ARRET_MAX);
        // fil bloqué 3 s : lâché après ~300 ms
        let debut = Instant::now();
        drop(capture_lente(Duration::from_secs(3)));
        let attente = debut.elapsed();
        assert!(attente >= ARRET_MAX, "{attente:?}");
        assert!(
            attente < ARRET_MAX + Duration::from_millis(200),
            "{attente:?}"
        );
    }

    /// Essai sur les vraies sorties de la machine :
    /// `cargo test --release sorties_reelles -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sorties_reelles() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        struct Compte(Arc<AtomicU32>);
        impl super::Sink for Compte {
            fn open(&mut self, _: u32) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn samples(&mut self, _: &[f32]) {}
            fn silence(&mut self, _: usize) {}
            fn tick(&mut self, _: std::time::Instant) -> std::time::Duration {
                std::time::Duration::from_millis(10)
            }
            fn lost(&mut self) {}
        }
        let liste = super::sorties();
        println!("{liste:#?}");
        assert!(!liste.is_empty(), "aucune sortie active");
        // la dernière sortie listée, puis une sortie inconnue (repli sur la sortie par défaut)
        for sortie in [
            Sortie::Id(liste[liste.len() - 1].0.clone()),
            Sortie::Id("{inconnue}".into()),
        ] {
            let ouvertures = Arc::new(AtomicU32::new(0));
            let c = super::start_sur("essai", sortie.clone(), Compte(ouvertures.clone())).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(800));
            drop(c);
            assert_eq!(ouvertures.load(Ordering::Relaxed), 1, "{sortie:?}");
        }
    }

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
    fn reglages_sans_appli_veulent_dire_toute_la_sortie() {
        assert_eq!(
            Ecoute::depuis_reglages("", &[] as &[&str]),
            Ecoute::Sortie(Sortie::ParDefaut)
        );
        assert_eq!(
            Ecoute::depuis_reglages("casque", &[" ", ""]),
            Ecoute::Sortie(Sortie::Id("casque".into()))
        );
        assert_eq!(
            Ecoute::depuis_reglages("casque", &[" Spotify.exe ", "", "chrome"]),
            Ecoute::Applis(vec!["Spotify.exe".into(), "chrome".into()])
        );
        assert_eq!(Ecoute::default(), Ecoute::Sortie(Sortie::ParDefaut));
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
        let (g, d) = mix_mono(&stereo, &mut mono);
        assert_eq!(mono, [0.5, 0.0, -1.0]);
        // niveaux efficaces : √((1 + 0,25 + 1) / 3) à gauche, √((0 + 0,25 + 1) / 3) à droite
        assert!((g - (2.25f32 / 3.0).sqrt()).abs() < 1e-6, "{g}");
        assert!((d - (1.25f32 / 3.0).sqrt()).abs() < 1e-6, "{d}");
    }
}
