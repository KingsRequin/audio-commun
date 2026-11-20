"""Prépare les données du banc d'essai du kick (`examples/kick_bench.rs`).

Pour chaque morceau : `<nom>.f32` (mélange complet, mono, f32, 48 kHz), `<nom>.kicks` et
`<nom>.caisses` (instants des vrais kicks et des vraies caisses claires, en secondes) et, avec
`--demucs`, `<nom>.kicks_doute` et `<nom>.caisses_doute` (instants où la référence est incertaine).

    python kick_bench.py mdb <dossier MDB Drums> <sortie>        annotations humaines de MDB Drums
    python kick_bench.py ref <sortie> <fichier ou dossier>...    n'importe quel morceau (référence ADTOF)
    python kick_bench.py valide <dossier MDB Drums> [--demucs]   ADTOF comparé aux annotations de MDB

La référence d'un morceau quelconque vient d'ADTOF (transcription de batterie par réseau de neurones,
entraîné sur des centaines d'heures de vraie musique ; F1 ≈ 0,88 sur MDB Drums), hors temps réel et
avec tout le morceau sous les yeux. Avec `--demucs`, ADTOF écoute la batterie séparée par Demucs
(htdemucs) et, à part, le mélange : un coup vu par les deux est sûr (`.kicks`, précision 0,92 face aux
annotations de MDB), un coup vu par une seule des deux écoutes est douteux (`.kicks_doute` : ni kick
ni non-kick pour l'apprentissage et l'évaluation). Ce n'est pas la vérité, mais une bien meilleure
approximation que tout détecteur causal : `valide` mesure son accord avec les annotations humaines.
`--pistes <dossier>` (avec `--demucs`) garde les 4 pistes séparées (`<nom>.npy`, int16, 44,1 kHz :
drums, bass, other, vocals) pour `kick_remix.py`.

Dépendances (environnement à part) : ffmpeg dans le PATH ; numpy, scipy, torch, adtof-pytorch,
demucs (seulement pour `--demucs`).
"""

import pathlib
import subprocess
import sys
import tempfile

import numpy as np
from scipy import signal

SR = 48000
TOL = 0.03  # tolérance de `valide` (s)
AUDIO = {".mp3", ".flac", ".wav", ".ogg", ".m4a", ".opus", ".webm"}


def load(path, sr=SR):
    """Décode n'importe quel fichier audio en stéréo float32 (2, n) via ffmpeg."""
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", "2", "-ar", str(sr), "-"],
        check=True,
        capture_output=True,
    ).stdout
    return np.frombuffer(raw, dtype="<f4").reshape(-1, 2).T.copy()


def save_kicks(times, out):
    out.write_text("".join(f"{t:.4f}\n" for t in times))


def write_wav(mono, sr, path):
    data = (np.clip(mono, -1, 1) * 32767).astype("<i2").tobytes()
    head = b"RIFF" + (36 + len(data)).to_bytes(4, "little") + b"WAVEfmt "
    head += (16).to_bytes(4, "little") + (1).to_bytes(2, "little") + (1).to_bytes(2, "little")
    head += sr.to_bytes(4, "little") + (sr * 2).to_bytes(4, "little") + (2).to_bytes(2, "little")
    head += (16).to_bytes(2, "little") + b"data" + len(data).to_bytes(4, "little")
    pathlib.Path(path).write_bytes(head + data)


# --- ADTOF -------------------------------------------------------------------------------------

_adtof = None


def adtof_notes(mono, sr):
    """Instants transcrits par ADTOF, par note MIDI (35 kick, 38 caisse claire, 42 charleston…)."""
    global _adtof
    import torch
    from adtof_pytorch import (
        FRAME_RNN_THRESHOLDS,
        LABELS_5,
        PeakPicker,
        calculate_n_bins,
        create_frame_rnn_model,
        get_default_weights_path,
        load_audio_for_model,
        load_pytorch_weights,
    )

    dev = "cuda" if torch.cuda.is_available() else "cpu"
    if _adtof is None:
        model = create_frame_rnn_model(calculate_n_bins())
        model = load_pytorch_weights(model, get_default_weights_path(), strict=False)
        _adtof = model.eval().to(dev)
    with tempfile.TemporaryDirectory() as tmp:
        wav = pathlib.Path(tmp) / "a.wav"
        write_wav(mono, sr, wav)
        x = load_audio_for_model(str(wav)).to(dev)
    with torch.no_grad():
        pred = _adtof(x).cpu().numpy()
    peaks = PeakPicker(thresholds=FRAME_RNN_THRESHOLDS, fps=100).pick(pred, labels=LABELS_5)[0]
    return {k: sorted(v) for k, v in peaks.items()}


# --- Demucs ------------------------------------------------------------------------------------

_demucs = None


def separate(stereo48):
    """Les 4 pistes (drums, bass, other, vocals ; mono, 44,1 kHz) séparées par htdemucs."""
    global _demucs
    import torch
    from demucs.apply import apply_model
    from demucs.pretrained import get_model

    dev = "cuda" if torch.cuda.is_available() else "cpu"
    if _demucs is None:
        _demucs = get_model("htdemucs").eval()
    sr = _demucs.samplerate
    wav = torch.from_numpy(signal.resample_poly(stereo48, sr, SR, axis=1).astype(np.float32))
    ref = wav.mean(0)
    mean, std = ref.mean(), ref.std() + 1e-8
    with torch.no_grad():
        src = apply_model(_demucs, ((wav - mean) / std)[None], device=dev, split=True, overlap=0.25, progress=False)[0]
    src = src * std + mean
    order = [_demucs.sources.index(n) for n in ("drums", "bass", "other", "vocals")]
    return src[order].mean(1).cpu().numpy(), sr


def consensus(a, b, tol=TOL):
    """(coups de `a` retrouvés dans `b` à ±tol, coups vus par une seule des deux listes)."""
    a, b = np.asarray(a, dtype=float), np.asarray(b, dtype=float)
    used, ok = np.zeros(len(b), bool), np.zeros(len(a), bool)
    for i, t in enumerate(a):
        c = np.where(~used & (np.abs(b - t) <= tol))[0]
        if len(c):
            used[c[np.argmin(np.abs(b[c] - t))]] = True
            ok[i] = True
    return a[ok], np.sort(np.concatenate([a[~ok], b[~used]]))


def reference(stereo, use_demucs, stems_out=None):
    """({note: instants sûrs}, {note: instants douteux}) ; sans Demucs, tout est sûr."""
    mix = adtof_notes(stereo.mean(axis=0), SR)
    if not use_demucs:
        return mix, {}
    stems, sr = separate(stereo)
    if stems_out is not None:
        np.save(stems_out, (np.clip(stems, -1, 1) * 32767).astype(np.int16))
    drums = adtof_notes(stems[0], sr)
    both = {n: consensus(drums.get(n, []), mix.get(n, [])) for n in set(drums) | set(mix)}
    return {n: v[0] for n, v in both.items()}, {n: v[1] for n, v in both.items()}


def mdb_tracks(root):
    for mix in sorted((root / "audio" / "full_mix").glob("*_MIX.wav")):
        name = mix.stem.removesuffix("_MIX")
        ann = root / "annotations" / "class" / f"{name}_class.txt"
        lines = ann.read_text().splitlines()
        notes = {c: sorted({float(l.split()[0]) for l in lines if l.split()[1:] == [c]}) for c in ("KD", "SD")}
        yield name, mix, notes["KD"], notes["SD"]


def safe_name(p):
    keep = "".join(c if c.isalnum() or c in " -_()" else "_" for c in p.stem)
    return " ".join(keep.split())[:80]


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    args = [a for a in sys.argv[1:] if a != "--demucs"]
    use_demucs = "--demucs" in sys.argv
    stems_dir = None
    if "--pistes" in args:
        i = args.index("--pistes")
        stems_dir = pathlib.Path(args[i + 1])
        stems_dir.mkdir(parents=True, exist_ok=True)
        del args[i : i + 2]
    if not args:
        sys.exit(__doc__)
    mode = args[0]
    if mode == "mdb":
        root, out = pathlib.Path(args[1]), pathlib.Path(args[2])
        out.mkdir(parents=True, exist_ok=True)
        for name, mix, kicks, snares in mdb_tracks(root):
            load(mix).mean(axis=0).astype("<f4").tofile(out / f"{name}.f32")
            save_kicks(kicks, out / f"{name}.kicks")
            save_kicks(snares, out / f"{name}.caisses")
            print(f"{name} : {len(kicks)} kicks", flush=True)
    elif mode == "valide":
        tot = [0, 0, 0]
        for name, mix, ref, _ in mdb_tracks(pathlib.Path(args[1])):
            got = reference(load(mix), use_demucs)[0].get(35, [])
            used, tp = set(), 0
            for g in got:
                m = [i for i, r in enumerate(ref) if i not in used and abs(g - r) <= TOL]
                if m:
                    used.add(m[0])
                    tp += 1
            p, r = tp / max(len(got), 1), tp / max(len(ref), 1)
            print(f"{name:<28} réf {len(ref):4} ADTOF {len(got):4}  préc {p:.2f} rappel {r:.2f}", flush=True)
            tot = [tot[0] + len(ref), tot[1] + len(got), tot[2] + tp]
        p, r = tot[2] / max(tot[1], 1), tot[2] / max(tot[0], 1)
        print(f"TOTAL préc {p:.3f} rappel {r:.3f} F1 {2 * p * r / max(p + r, 1e-9):.3f}")
    elif mode == "ref":
        out = pathlib.Path(args[1])
        out.mkdir(parents=True, exist_ok=True)
        files = []
        for a in map(pathlib.Path, args[2:]):
            files += sorted(p for p in a.iterdir() if p.suffix.lower() in AUDIO) if a.is_dir() else [a]
        for f in files:
            name = safe_name(f)
            if (out / f"{name}.kicks").exists():
                continue
            try:
                stereo = load(f)
                notes, doubt = reference(stereo, use_demucs, stems_dir / f"{name}.npy" if stems_dir else None)
            except Exception as e:  # fichier illisible : on passe au suivant
                print(f"{name} : ÉCHEC {e}", flush=True)
                continue
            stereo.mean(axis=0).astype("<f4").tofile(out / f"{name}.f32")
            for note, suffix in ((35, ".kicks"), (38, ".caisses")):
                save_kicks(notes.get(note, []), out / f"{name}{suffix}")
                if use_demucs:
                    save_kicks(doubt.get(note, []), out / f"{name}{suffix}_doute")
            print(f"{name} : {len(notes.get(35, []))} kicks, {len(notes.get(38, []))} caisses claires ; douteux : {len(doubt.get(35, []))}, {len(doubt.get(38, []))}", flush=True)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
