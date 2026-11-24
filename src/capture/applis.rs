//! Écoute de certaines applications seulement (capture « process loopback », Windows 10 2004+).
//!
//! Pour chaque nom d'exécutable, la capture vise la racine de l'arbre de processus de l'appli
//! (le processus de ce nom dont le parent n'a pas le même nom), arbre compris : un navigateur
//! joue le son depuis un processus enfant. Deux instances indépendantes d'une appli donnent deux
//! racines, donc deux clients. Le format est imposé (f32 stéréo, 48 000 Hz, converti par
//! Windows) : ce mode ne donne ni le format de mélange ni la période.
//!
//! Les clients livrent leurs paquets à des moments différents : chacun remplit sa file, et le
//! mélange (somme) n'avance que jusqu'à la plus courte des files des clients qui jouent. Un
//! client sans paquet depuis 30 ms compte comme du silence et ne retient plus les autres. La
//! liste des processus est revérifiée toutes les 2 s (appli lancée : ajoutée ; fermée :
//! retirée) ; sans aucun paquet, du silence est remis comme pour une sortie entière.

use super::{mono_et_niveaux, trames, Sink, DEVICE_CHECK, ERROR_LOG, MAX_WAIT_MS, NO_DATA};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

/// Poignées que `WaitForMultipleObjects` accepte au plus (`MAXIMUM_WAIT_OBJECTS`).
const MAXIMUM_WAIT_OBJECTS: usize = 64;
/// Fréquence imposée aux clients (Windows convertit).
const TAUX: u32 = 48_000;
/// Avance qu'une file peut prendre sur le mélange (client en retard) : au-delà, le mélange
/// avance sans attendre (100 ms).
const RETARD_MAX: usize = TAUX as usize / 10;

/// Seuil de niveau crête (0 à 1) d'une session au-dessus duquel son appli « joue » (−60 dBFS).
const JOUE: f32 = 0.001;

/// Application qui a une session audio : son exécutable, son nom affichable, et si elle joue en
/// ce moment (une session active dont le niveau crête dépasse −60 dBFS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliSonore {
    pub exe: String,
    pub nom: String,
    pub joue: bool,
}

/// Comme [`applis_sonores`], en disant pour chaque appli si elle joue en ce moment.
pub fn applis_sonores_detail() -> Vec<AppliSonore> {
    let init = wasapi::initialize_mta();
    let liste = lire_applis().unwrap_or_else(|e| {
        log::warn!("audio : liste des applis sonores illisible : {e:#}");
        Vec::new()
    });
    if init.is_ok() {
        wasapi::deinitialize();
    }
    liste
}

/// Applications qui ont en ce moment une session audio sur une sortie active, `(exécutable,
/// nom affichable)` : sans les sons système (pid 0, programmes du dossier système de Windows,
/// processus d'un autre compte) ni ce processus-ci, une seule fois par exécutable (casse
/// indifférente), triées par nom affichable. Le nom affichable est la description de
/// l'exécutable (« Google Chrome »), à défaut celui de la session, à défaut l'exécutable sans
/// `.exe`. Liste vide si Windows ne répond pas. Appelable depuis n'importe quel fil, comme
/// [`super::sorties`].
pub fn applis_sonores() -> Vec<(String, String)> {
    applis_sonores_detail()
        .into_iter()
        .map(|a| (a.exe, a.nom))
        .collect()
}

fn lire_applis() -> anyhow::Result<Vec<AppliSonore>> {
    let procs = processus()?;
    let moi = std::process::id();
    let racine = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let enumerator = DeviceEnumerator::new()?;
    let collection = enumerator.get_device_collection(&Direction::Render)?;
    let mut applis = Vec::new();
    for device in &collection {
        // Une sortie illisible n'empêche pas de lister les autres.
        let Ok(sessions) =
            device.and_then(|d| d.get_iaudiosessionmanager()?.get_audiosessionenumerator())
        else {
            continue;
        };
        for i in 0..sessions.get_count().unwrap_or(0) {
            let Ok(session) = sessions.get_session(i) else {
                continue;
            };
            if session
                .get_state()
                .is_ok_and(|s| s == wasapi::SessionState::Expired)
            {
                continue;
            }
            let pid = session.get_process_id().unwrap_or(0);
            if pid == 0 || pid == moi {
                continue;
            }
            let Some(p) = procs.iter().find(|p| p.pid == pid) else {
                continue;
            };
            // Chemin illisible : processus d'un autre compte (service de Windows, processus
            // protégé). Une appli que l'utilisateur lance en administrateur reste lisible.
            let Some(chemin) = chemin_image(pid) else {
                continue;
            };
            if de_windows(&String::from_utf16_lossy(&chemin), &racine) {
                continue;
            }
            let session_nom = session.get_display_name().ok();
            // joue : session active et son niveau crête au-dessus du seuil
            let joue = session
                .get_state()
                .is_ok_and(|s| s == wasapi::SessionState::Active)
                && session
                    .get_audiometerinformation()
                    .and_then(|m| m.get_peak_value())
                    .is_ok_and(|v| v > JOUE);
            applis.push((p.nom.clone(), chemin, session_nom, joue));
        }
    }
    let applis = applis
        .into_iter()
        .map(|(exe, chemin, session, joue)| AppliSonore {
            nom: nom_affichable(&exe, description(&chemin), session),
            exe,
            joue,
        })
        .collect();
    Ok(ranger(applis))
}

/// Programme du dossier système de Windows (`System32`, `SysWOW64`) : un composant de Windows
/// (« Écouter ce périphérique » joue un micro depuis `svchost.exe`, par exemple), pas une
/// application de l'utilisateur.
fn de_windows(chemin: &str, racine: &str) -> bool {
    let chemin = chemin.to_lowercase();
    let racine = racine.trim_end_matches('\\').to_lowercase();
    ["system32", "syswow64"]
        .iter()
        .any(|d| chemin.starts_with(&format!(r"{racine}\{d}\")))
}

/// Une seule entrée par exécutable (la première ; elle joue si l'une de ses sessions joue),
/// triée par nom affichable puis exécutable.
fn ranger(applis: Vec<AppliSonore>) -> Vec<AppliSonore> {
    let mut liste: Vec<AppliSonore> = Vec::new();
    for a in applis {
        match liste.iter_mut().find(|x| normaliser(&x.exe) == normaliser(&a.exe)) {
            Some(x) => x.joue |= a.joue,
            None => liste.push(a),
        }
    }
    liste.sort_by_cached_key(|a| (a.nom.to_lowercase(), a.exe.to_lowercase()));
    liste
}

/// Description de l'exécutable, sinon nom de la session (sauf référence de ressource
/// « @… »), sinon nom de l'exécutable sans `.exe`.
fn nom_affichable(exe: &str, description: Option<String>, session: Option<String>) -> String {
    let utile = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('@'))
            .map(str::to_owned)
    };
    utile(&description)
        .or_else(|| utile(&session))
        .unwrap_or_else(|| {
            let n = exe.len();
            if n > 4 && exe.is_char_boundary(n - 4) && exe[n - 4..].eq_ignore_ascii_case(".exe") {
                exe[..n - 4].to_owned()
            } else {
                exe.to_owned()
            }
        })
}

/// Nom d'exécutable comparable : sans espaces autour, en minuscules, `.exe` ajouté s'il manque.
fn normaliser(nom: &str) -> String {
    let mut n = nom.trim().to_lowercase();
    if !n.ends_with(".exe") {
        n.push_str(".exe");
    }
    n
}

/// Un processus de l'instantané de Windows.
#[derive(Clone, Debug)]
struct Processus {
    pid: u32,
    parent: u32,
    nom: String,
}

/// Racines des arbres de processus de l'exécutable `nom` (normalisé) : les processus de ce nom
/// dont le parent n'a pas le même nom (ou n'existe plus).
fn racines(procs: &[Processus], nom: &str) -> Vec<u32> {
    let de_ce_nom = |p: &Processus| normaliser(&p.nom) == nom;
    procs
        .iter()
        .filter(|p| de_ce_nom(p))
        .filter(|p| p.parent == p.pid || !procs.iter().any(|q| q.pid == p.parent && de_ce_nom(q)))
        .map(|p| p.pid)
        .collect()
}

/// Processus en cours (instantané « Toolhelp »).
fn processus() -> anyhow::Result<Vec<Processus>> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: instantané refermé avant de rendre ; `entree` porte sa taille comme demandé.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut entree = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut liste = Vec::new();
        let mut ok = Process32FirstW(snap, &mut entree).is_ok();
        while ok {
            let n = entree
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entree.szExeFile.len());
            liste.push(Processus {
                pid: entree.th32ProcessID,
                parent: entree.th32ParentProcessID,
                nom: String::from_utf16_lossy(&entree.szExeFile[..n]),
            });
            ok = Process32NextW(snap, &mut entree).is_ok();
        }
        let _ = CloseHandle(snap);
        Ok(liste)
    }
}

/// Chemin complet de l'exécutable du processus `pid` (UTF-16, sans zéro final).
fn chemin_image(pid: u32) -> Option<Vec<u16>> {
    use windows::core::PWSTR;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: poignée refermée aussitôt ; tampon dimensionné comme annoncé.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut chemin = [0u16; 1024];
        let mut n = chemin.len() as u32;
        let lu =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(chemin.as_mut_ptr()), &mut n);
        let _ = CloseHandle(h);
        lu.ok()?;
        Some(chemin[..n as usize].to_vec())
    }
}

/// Description (« FileDescription ») de l'exécutable `chemin` (UTF-16, sans zéro final).
fn description(chemin: &[u16]) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let chemin: Vec<u16> = chemin.iter().copied().chain([0]).collect();
    // SAFETY: tampons dimensionnés comme annoncé à chaque appel ; les pointeurs rendus par
    // VerQueryValueW désignent l'intérieur de `bloc`, encore vivant.
    unsafe {
        let chemin = PCWSTR(chemin.as_ptr());
        let taille = GetFileVersionInfoSizeW(chemin, None);
        if taille == 0 {
            return None;
        }
        let mut bloc = vec![0u8; taille as usize];
        GetFileVersionInfoW(chemin, None, taille, bloc.as_mut_ptr().cast()).ok()?;
        let lire = |cle: &str| -> Option<(*mut std::ffi::c_void, u32)> {
            let cle: Vec<u16> = cle.encode_utf16().chain([0]).collect();
            let mut p = std::ptr::null_mut();
            let mut l = 0u32;
            VerQueryValueW(bloc.as_ptr().cast(), PCWSTR(cle.as_ptr()), &mut p, &mut l)
                .as_bool()
                .then_some((p, l))
                .filter(|(p, l)| !p.is_null() && *l > 0)
        };
        // Langues déclarées, puis anglais (Unicode, puis Windows-1252) au cas où.
        let mut langues: Vec<(u16, u16)> = Vec::new();
        if let Some((p, l)) = lire("\\VarFileInfo\\Translation") {
            let paires = std::slice::from_raw_parts(p as *const u16, l as usize / 2);
            langues.extend(paires.chunks_exact(2).map(|c| (c[0], c[1])));
        }
        langues.extend([(0x0409, 0x04B0), (0x0409, 0x04E4)]);
        langues.into_iter().find_map(|(langue, page)| {
            let (p, l) = lire(&format!(
                "\\StringFileInfo\\{langue:04x}{page:04x}\\FileDescription"
            ))?;
            let texte = std::slice::from_raw_parts(p as *const u16, l as usize);
            let fin = texte.iter().position(|&c| c == 0).unwrap_or(texte.len());
            let texte = String::from_utf16_lossy(&texte[..fin]).trim().to_owned();
            (!texte.is_empty()).then_some(texte)
        })
    }
}

/// Files des clients et leur somme, alignées dans le temps (logique pure, sans Windows).
#[derive(Default)]
struct Melange {
    voies: Vec<Voie>,
}

struct Voie {
    id: u32,
    file: VecDeque<[f32; 2]>,
    /// Dernier paquet reçu (aucun : le client ne retient pas le mélange).
    dernier: Option<Instant>,
}

impl Melange {
    fn ajouter(&mut self, id: u32) {
        if !self.voies.iter().any(|v| v.id == id) {
            self.voies.push(Voie {
                id,
                file: VecDeque::new(),
                dernier: None,
            });
        }
    }

    fn retirer(&mut self, id: u32) {
        self.voies.retain(|v| v.id != id);
    }

    fn recevoir(&mut self, id: u32, trames: impl IntoIterator<Item = [f32; 2]>, now: Instant) {
        if let Some(v) = self.voies.iter_mut().find(|v| v.id == id) {
            let avant = v.file.len();
            v.file.extend(trames);
            // Un paquet vide ne dit pas que le client joue : il retiendrait le mélange.
            if v.file.len() > avant {
                v.dernier = Some(now);
            }
        }
    }

    fn silence(&mut self, id: u32, n: usize, now: Instant) {
        self.recevoir(id, std::iter::repeat_n([0.0; 2], n), now);
    }

    /// Plus rien en attente.
    fn vide(&self) -> bool {
        self.voies.iter().all(|v| v.file.is_empty())
    }

    /// Ajoute à `sortie` la somme des trames prêtes : jusqu'à la plus courte des files des
    /// clients qui jouent (paquet reçu depuis 30 ms au plus) ; les autres comptent comme du
    /// silence. Sans client qui joue, tout ce qui attend sort.
    fn sortir(&mut self, now: Instant, sortie: &mut Vec<[f32; 2]>) {
        let joue = |v: &Voie| {
            v.dernier
                .is_some_and(|t| now.saturating_duration_since(t) <= NO_DATA)
        };
        let plus_longue = self.voies.iter().map(|v| v.file.len()).max().unwrap_or(0);
        let n = self
            .voies
            .iter()
            .filter(|v| joue(v))
            .map(|v| v.file.len())
            .min()
            .unwrap_or(plus_longue)
            .max(plus_longue.saturating_sub(RETARD_MAX));
        if n == 0 {
            return;
        }
        let debut = sortie.len();
        sortie.resize(debut + n, [0.0; 2]);
        for v in &mut self.voies {
            let k = n.min(v.file.len());
            for (o, [l, r]) in sortie[debut..].iter_mut().zip(v.file.drain(..k)) {
                o[0] += l;
                o[1] += r;
            }
        }
    }
}

/// Un client « process loopback » ouvert et démarré ; arrêté quand il est lâché.
struct Client {
    pid: u32,
    nom: String,
    audio: AudioClient,
    capture: wasapi::AudioCaptureClient,
    event: wasapi::Handle,
}

impl Client {
    fn ouvrir(pid: u32, nom: &str) -> anyhow::Result<Self> {
        let mut audio = AudioClient::new_application_loopback_client(pid, true)?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, TAUX as usize, 2, None);
        audio.initialize_client(
            &format,
            &Direction::Capture,
            // Durée sans effet dans ce mode (Windows la choisit).
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 200_000,
            },
        )?;
        let event = audio.set_get_eventhandle()?;
        let capture = audio.get_audiocaptureclient()?;
        audio.start_stream()?;
        Ok(Self {
            pid,
            nom: nom.to_owned(),
            audio,
            capture,
            event,
        })
    }

    /// Vide les paquets disponibles dans la voie du client.
    fn vider(
        &self,
        octets: &mut Vec<u8>,
        melange: &mut Melange,
        now: Instant,
    ) -> anyhow::Result<()> {
        loop {
            let n = self.capture.get_next_packet_size()?.unwrap_or(0) as usize;
            if n == 0 {
                return Ok(());
            }
            // La taille du tampon de ce mode est fausse : chaque paquet dit la sienne.
            if octets.len() < n * 8 {
                octets.resize(n * 8, 0);
            }
            let (lues, info) = self.capture.read_from_device(octets)?;
            let lues = lues as usize;
            if info.flags.silent {
                melange.silence(self.pid, lues, now);
            } else {
                melange.recevoir(self.pid, trames(&octets[..lues * 8]), now);
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.audio.stop_stream();
    }
}

/// Écoute les applis `noms` jusqu'à l'arrêt (`Ok`). `Err` : Windows refuse la capture par
/// application (essai sur ce processus-ci), à remplacer par toute la sortie.
pub(super) fn ecouter(
    sink: &mut impl Sink,
    noms: &[String],
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    // Windows antérieur à 10 2004, ou capture refusée : on le sait avant d'attendre une appli.
    Client::ouvrir(std::process::id(), "essai")?;
    let mut voulus: Vec<String> = Vec::new();
    for n in noms.iter().map(|n| normaliser(n)) {
        if n != ".exe" && !voulus.contains(&n) {
            voulus.push(n);
        }
    }
    sink.open(TAUX);
    log::info!(
        "audio : écoute des applis {} ({TAUX} Hz)",
        voulus.join(", ")
    );
    let mut suivi = Suivi::default();
    let mut melange = Melange::default();
    let mut octets = Vec::new();
    let mut somme = Vec::new();
    let mut mono = Vec::new();
    let mut verifie: Option<Instant> = None;
    let mut derniere_sortie = Instant::now();
    let mut journal: Option<Instant> = None;
    let mut wait = Duration::from_millis(10);
    while !stop.load(Ordering::Relaxed) {
        if verifie.is_none_or(|t| t.elapsed() >= DEVICE_CHECK) {
            // Ouvertures inachevées (budget de temps épuisé) : reprises au tour suivant.
            let fini = suivi.actualiser(&mut melange, &voulus);
            verifie = fini.then(Instant::now);
        }
        let clients = &mut suivi.clients;
        let wait_ms = wait.as_millis().clamp(1, MAX_WAIT_MS) as u32;
        if clients.is_empty() {
            std::thread::sleep(Duration::from_millis(wait_ms.into()));
        } else {
            // Windows attend 64 poignées au plus ; les autres clients sont vidés au réveil.
            let events: Vec<HANDLE> = clients
                .iter()
                .take(MAXIMUM_WAIT_OBJECTS)
                .map(|c| c.event.as_handle())
                .collect();
            // SAFETY: poignées valides tant que les clients vivent (toute la durée de l'appel).
            // Délai dépassé = rien n'est joué : ce n'est pas une erreur.
            let _ = unsafe {
                windows::Win32::System::Threading::WaitForMultipleObjects(&events, false, wait_ms)
            };
        }
        let now = Instant::now();
        clients.retain(|c| match c.vider(&mut octets, &mut melange, now) {
            Ok(()) => true,
            Err(e) => {
                // Rouvert à la prochaine vérification s'il tourne encore.
                if journal.is_none_or(|t| now.duration_since(t) >= ERROR_LOG) {
                    log::warn!("audio : capture de « {} » interrompue : {e:#}", c.nom);
                    journal = Some(now);
                }
                melange.retirer(c.pid);
                false
            }
        });
        somme.clear();
        melange.sortir(now, &mut somme);
        if !somme.is_empty() {
            remettre(sink, &somme, &mut mono);
            derniere_sortie = now;
        } else if melange.vide() {
            let silent_for = now.duration_since(derniere_sortie);
            if silent_for > NO_DATA {
                let n = (silent_for.as_secs_f64() * f64::from(TAUX)) as usize;
                sink.silence(n.min(TAUX as usize / 10));
                derniere_sortie = now;
            }
        }
        wait = sink.tick(now);
    }
    Ok(())
}

/// Temps d'ouverture de clients par tour de boucle : au-delà, la suite attend le tour suivant
/// (le son des clients déjà ouverts continue de passer).
const BUDGET_OUVERTURE: Duration = Duration::from_millis(10);

/// Clients ouverts, et processus que Windows refuse de laisser capturer (pas réessayés tant
/// qu'ils tournent).
#[derive(Default)]
struct Suivi {
    clients: Vec<Client>,
    refuses: Vec<u32>,
}

impl Suivi {
    /// Met les clients en accord avec les processus en cours : ouvre les racines apparues,
    /// lâche celles disparues. `false` si le budget de temps n'a pas suffi à tout ouvrir.
    fn actualiser(&mut self, melange: &mut Melange, voulus: &[String]) -> bool {
        let debut = Instant::now();
        let procs = match processus() {
            Ok(p) => p,
            Err(e) => {
                log::debug!("audio : liste des processus illisible ({e})");
                return true;
            }
        };
        let cibles: Vec<(u32, &str)> = voulus
            .iter()
            .flat_map(|n| {
                racines(&procs, n)
                    .into_iter()
                    .map(move |pid| (pid, n.as_str()))
            })
            .collect();
        self.refuses
            .retain(|pid| cibles.iter().any(|&(p, _)| p == *pid));
        self.clients.retain(|c| {
            let garde = cibles.iter().any(|&(pid, _)| pid == c.pid);
            if !garde {
                log::info!("audio : « {} » ({}) fermée", c.nom, c.pid);
                melange.retirer(c.pid);
            }
            garde
        });
        for (pid, nom) in cibles {
            if self.clients.iter().any(|c| c.pid == pid) || self.refuses.contains(&pid) {
                continue;
            }
            if debut.elapsed() >= BUDGET_OUVERTURE {
                return false;
            }
            match Client::ouvrir(pid, nom) {
                Ok(c) => {
                    log::info!("audio : « {nom} » ({pid}) écoutée");
                    melange.ajouter(pid);
                    self.clients.push(c);
                }
                Err(e) => {
                    log::warn!("audio : « {nom} » ({pid}) ne peut pas être écoutée : {e:#}");
                    self.refuses.push(pid);
                }
            }
        }
        true
    }
}

/// Remet la somme au `Sink` : du silence si elle est nulle, sinon mono et niveaux stéréo.
fn remettre(sink: &mut impl Sink, somme: &[[f32; 2]], mono: &mut Vec<f32>) {
    if somme.iter().all(|&[l, r]| l == 0.0 && r == 0.0) {
        sink.silence(somme.len());
        return;
    }
    mono.resize(somme.len(), 0.0);
    let (gauche, droite) = mono_et_niveaux(somme.iter().copied(), mono);
    sink.samples(mono);
    sink.stereo(gauche, droite);
}

#[cfg(test)]
mod tests {
    use super::{
        de_windows, nom_affichable, AppliSonore, normaliser, racines, ranger, Melange, Processus, RETARD_MAX,
    };
    use std::time::{Duration, Instant};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn noms_compares_sans_la_casse_ni_l_extension() {
        assert_eq!(normaliser(" Spotify.EXE "), "spotify.exe");
        assert_eq!(normaliser("spotify"), "spotify.exe");
        assert_eq!(normaliser("Chrome.exe"), normaliser("CHROME"));
        assert_ne!(normaliser("chrome"), normaliser("chromium"));
        assert_eq!(normaliser("Éditeur"), "éditeur.exe");
    }

    fn p(pid: u32, parent: u32, nom: &str) -> Processus {
        Processus {
            pid,
            parent,
            nom: nom.into(),
        }
    }

    #[test]
    fn racine_de_l_arbre_de_l_appli() {
        let procs = [
            p(1, 0, "explorer.exe"),
            // un navigateur : racine lancée par l'Explorateur, enfants (dont celui du son)
            p(10, 1, "chrome.exe"),
            p(11, 10, "chrome.exe"),
            p(12, 11, "Chrome.exe"),
            // une seconde instance, lancée par une autre appli
            p(20, 30, "chrome.exe"),
            p(30, 1, "code.exe"),
            // parent disparu
            p(40, 999, "spotify.exe"),
            p(41, 40, "Spotify.exe"),
            // enfant d'un autre nom sous la racine : n'est pas une racine de chrome
            p(50, 10, "crashpad.exe"),
        ];
        assert_eq!(racines(&procs, &normaliser("CHROME.EXE")), vec![10, 20]);
        assert_eq!(racines(&procs, &normaliser("spotify")), vec![40]);
        assert!(racines(&procs, &normaliser("firefox.exe")).is_empty());
        // processus qui se dit son propre parent (pid réutilisé) : racine quand même
        assert_eq!(racines(&[p(7, 7, "a.exe")], "a.exe"), vec![7]);
    }

    #[test]
    fn applis_sans_doublon_et_triees() {
        let a = |exe: &str, nom: &str, joue: bool| AppliSonore {
            exe: exe.into(),
            nom: nom.into(),
            joue,
        };
        let liste = ranger(vec![
            a("Spotify.exe", "Spotify", false),
            a("chrome.exe", "Google Chrome", false),
            a("spotify.exe", "Spotify (2)", true),
            a("discord.exe", "discord", false),
        ]);
        // une seule entrée par exécutable, qui joue si l'une de ses sessions joue
        assert_eq!(
            liste,
            vec![
                a("discord.exe", "discord", false),
                a("chrome.exe", "Google Chrome", false),
                a("Spotify.exe", "Spotify", true),
            ]
        );
    }

    #[test]
    fn programmes_du_dossier_systeme_ecartes() {
        let r = r"C:\Windows";
        assert!(de_windows(r"C:\Windows\System32\svchost.exe", r));
        assert!(de_windows(r"c:\windows\SysWOW64\x.exe", r"C:\WINDOWS\"));
        assert!(!de_windows(r"C:\Windows\explorer.exe", r));
        assert!(!de_windows(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r
        ));
        assert!(!de_windows(r"D:\Windows\System32\a.exe", r));
    }

    #[test]
    fn nom_affichable_par_ordre_de_preference() {
        let s = |t: &str| Some(t.to_owned());
        assert_eq!(
            nom_affichable("chrome.exe", s("Google Chrome"), s("x")),
            "Google Chrome"
        );
        assert_eq!(nom_affichable("a.exe", s("  "), s("Lecteur")), "Lecteur");
        assert_eq!(
            nom_affichable("Spotify.EXE", None, s("@%SystemRoot%\\System32\\x.dll,-1")),
            "Spotify"
        );
        assert_eq!(nom_affichable("outil", None, None), "outil");
    }

    #[test]
    fn deux_flux_alignes_puis_additionnes() {
        let t0 = Instant::now();
        let mut m = Melange::default();
        m.ajouter(1);
        m.ajouter(2);
        let mut out = Vec::new();
        // les deux jouent ; A livre d'abord : rien ne sort tant que B n'a pas livré
        m.recevoir(1, [[0.25, 0.0]; 480], t0);
        m.recevoir(2, [[0.0, 0.5]; 240], t0);
        m.sortir(t0, &mut out);
        assert_eq!(out.len(), 240);
        m.recevoir(1, [[0.25, 0.0]; 480], t0 + ms(5));
        m.sortir(t0 + ms(5), &mut out);
        assert_eq!(out.len(), 240, "B n'a rien livré de plus : on l'attend");
        m.recevoir(2, [[0.0, 0.5]; 720], t0 + ms(8));
        m.sortir(t0 + ms(8), &mut out);
        assert_eq!(out.len(), 960);
        assert!(out.iter().all(|&t| t == [0.25, 0.5]), "somme trame à trame");
        assert!(m.vide());
    }

    #[test]
    fn un_client_muet_ne_bloque_pas_les_autres() {
        let t0 = Instant::now();
        let mut m = Melange::default();
        m.ajouter(1);
        m.ajouter(2);
        let mut out = Vec::new();
        m.recevoir(2, [[0.1, 0.1]; 10], t0);
        m.recevoir(1, [[0.5, 0.5]; 480], t0 + ms(20));
        m.sortir(t0 + ms(20), &mut out);
        // B a joué il y a 20 ms : attendu, seule sa part sort
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], [0.6, 0.6]);
        // 31 ms sans paquet de B : il compte comme du zéro
        m.sortir(t0 + ms(31), &mut out);
        assert_eq!(out.len(), 480);
        assert_eq!(out[479], [0.5, 0.5]);
    }

    #[test]
    fn un_client_nouveau_ou_retire_ne_bloque_pas() {
        let t0 = Instant::now();
        let mut m = Melange::default();
        m.ajouter(1);
        m.ajouter(2); // pas encore de paquet
        let mut out = Vec::new();
        m.recevoir(1, [[1.0, 0.0]; 100], t0);
        m.sortir(t0, &mut out);
        assert_eq!(out.len(), 100);
        // B joue puis est retiré (appli fermée) : A n'attend plus B
        m.recevoir(2, [[0.0; 2]; 5], t0 + ms(1));
        m.recevoir(1, [[1.0, 0.0]; 100], t0 + ms(1));
        m.retirer(2);
        m.sortir(t0 + ms(1), &mut out);
        assert_eq!(out.len(), 200);
        // une voie inconnue ne reçoit rien
        m.recevoir(3, [[1.0; 2]; 5], t0);
        assert!(m.vide());
        // un paquet vide ne fait pas attendre B
        m.ajouter(2);
        m.recevoir(2, [], t0 + ms(2));
        m.recevoir(1, [[1.0, 0.0]; 10], t0 + ms(2));
        m.sortir(t0 + ms(2), &mut out);
        assert_eq!(out.len(), 210);
    }

    #[test]
    fn un_client_en_retard_ne_retient_pas_plus_de_100_ms() {
        let t0 = Instant::now();
        let mut m = Melange::default();
        m.ajouter(1);
        m.ajouter(2);
        let mut out = Vec::new();
        m.recevoir(2, [[0.0; 2]; 1], t0);
        m.recevoir(1, vec![[1.0, 1.0]; RETARD_MAX + 300], t0);
        m.sortir(t0, &mut out);
        assert_eq!(out.len(), 300);
        assert_eq!(out[0], [1.0, 1.0]);
    }

    #[test]
    fn sans_client_qui_joue_tout_sort() {
        let t0 = Instant::now();
        let mut m = Melange::default();
        m.ajouter(1);
        m.ajouter(2);
        let mut out = Vec::new();
        m.silence(1, 50, t0);
        m.recevoir(2, [[0.2, 0.2]; 80], t0);
        m.sortir(t0 + ms(100), &mut out);
        assert_eq!(out.len(), 80);
        assert_eq!(out[0], [0.2, 0.2]);
        assert!(m.vide());
    }

    /// Essai sur les vraies applis de la machine :
    /// `cargo test --release applis_reelles -- --ignored --nocapture`. Passe aussi quand rien
    /// ne joue (une appli absente donne du silence).
    #[test]
    #[ignore]
    fn applis_reelles() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        #[derive(Default)]
        struct Comptes {
            ouvertures: AtomicU64,
            trames: AtomicU64,
            silence: AtomicU64,
            crete: AtomicU64,
        }
        struct Compte(Arc<Comptes>);
        impl crate::capture::Sink for Compte {
            fn open(&mut self, taux: u32) {
                // 48 kHz, ou la fréquence de la sortie par défaut après un repli
                println!("ouverte à {taux} Hz");
                self.0.ouvertures.fetch_add(1, Ordering::Relaxed);
            }
            fn samples(&mut self, mono: &[f32]) {
                self.0
                    .trames
                    .fetch_add(mono.len() as u64, Ordering::Relaxed);
                let c = mono.iter().fold(0.0f32, |c, s| c.max(s.abs()));
                self.0
                    .crete
                    .fetch_max(u64::from((c * 1000.0) as u32), Ordering::Relaxed);
            }
            fn silence(&mut self, n: usize) {
                self.0.silence.fetch_add(n as u64, Ordering::Relaxed);
            }
            fn tick(&mut self, _: Instant) -> Duration {
                ms(10)
            }
            fn lost(&mut self) {}
        }
        let applis = crate::capture::applis_sonores_detail();
        println!("applis sonores : {applis:#?}");
        let mut essais = vec![vec!["essai-inexistant.exe".to_owned()]];
        if let Some(a) = applis.first() {
            essais.push(vec![a.exe.clone()]);
        }
        if applis.len() >= 2 {
            essais.push(applis.iter().map(|a| a.exe.clone()).collect());
        }
        for noms in essais {
            let c = Arc::new(Comptes::default());
            let capture = crate::capture::start_ecoute(
                "essai",
                crate::capture::Ecoute::Applis(noms.clone()),
                Compte(c.clone()),
            )
            .unwrap();
            std::thread::sleep(ms(600));
            drop(capture);
            let (o, t, s) = (
                c.ouvertures.load(Ordering::Relaxed),
                c.trames.load(Ordering::Relaxed),
                c.silence.load(Ordering::Relaxed),
            );
            println!(
                "{noms:?} : {o} ouverture(s), {t} trames de son (crête {:.3}), {s} de silence",
                c.crete.load(Ordering::Relaxed) as f32 / 1000.0
            );
            assert!(o >= 1, "{noms:?}");
            // 600 ms à 48 kHz : au moins la moitié remise (son ou silence)
            assert!(t + s > 14_000, "{noms:?} : {t} + {s}");
        }
    }
}
