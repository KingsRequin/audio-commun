"""Prépare les données du banc d'essai du kick (`examples/kick_bench.rs`).

Pour chaque morceau : `<nom>.f32` (mélange complet, mono, f32, 48 kHz) et `<nom>.kicks` (instants
des vrais kicks, en secondes).

    python kick_bench.py mdb <dossier MDB Drums> <sortie>        annotations humaines de MDB Drums
    python kick_bench.py ref <sortie> <fichier ou dossier>...    n'importe quel morceau (référence ADTOF)
    python kick_bench.py valide <dossier MDB Drums> [--demucs]   ADTOF comparé aux annotations de MDB

La référence d'un morceau quelconque vient d'ADTOF (transcription de batterie par réseau de neurones,
entraîné sur des centaines d'heures de vraie musique ; F1 ≈ 0,88 sur MDB Drums), hors temps réel et
avec tout le morceau sous les yeux. Avec `--demucs`, ADTOF écoute la batterie séparée par Demucs
(htdemucs) au lieu du mélange. Ce n'est pas la vérité, mais une bien meilleure approximation que tout
détecteur causal : `valide` mesure son accord avec les annotations humaines.

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


def adtof_kicks(mono, sr):
    """Instants des kicks (note 35) transcrits par ADTOF."""
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
    return sorted(peaks.get(35, []))


# --- Demucs ------------------------------------------------------------------------------------

_demucs = None


def separate_drums(stereo48):
    """Piste de batterie (mono, 44,1 kHz) séparée par htdemucs."""
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
    return src[_demucs.sources.index("drums")].mean(0).cpu().numpy(), sr


def reference(stereo, use_demucs):
    if use_demucs:
        drums, sr = separate_drums(stereo)
        return adtof_kicks(drums, sr)
    return adtof_kicks(stereo.mean(axis=0), SR)


def mdb_tracks(root):
    for mix in sorted((root / "audio" / "full_mix").glob("*_MIX.wav")):
        name = mix.stem.removesuffix("_MIX")
        ann = root / "annotations" / "class" / f"{name}_class.txt"
        kicks = sorted({float(l.split()[0]) for l in ann.read_text().splitlines() if "KD" in l})
        yield name, mix, kicks


def safe_name(p):
    keep = "".join(c if c.isalnum() or c in " -_()" else "_" for c in p.stem)
    return " ".join(keep.split())[:80]


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    args = [a for a in sys.argv[1:] if a != "--demucs"]
    use_demucs = "--demucs" in sys.argv
    if not args:
        sys.exit(__doc__)
    mode = args[0]
    if mode == "mdb":
        root, out = pathlib.Path(args[1]), pathlib.Path(args[2])
        out.mkdir(parents=True, exist_ok=True)
        for name, mix, kicks in mdb_tracks(root):
            load(mix).mean(axis=0).astype("<f4").tofile(out / f"{name}.f32")
            save_kicks(kicks, out / f"{name}.kicks")
            print(f"{name} : {len(kicks)} kicks", flush=True)
    elif mode == "valide":
        tot = [0, 0, 0]
        for name, mix, ref in mdb_tracks(pathlib.Path(args[1])):
            got = reference(load(mix), use_demucs)
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
                kicks = reference(stereo, use_demucs)
            except Exception as e:  # fichier illisible : on passe au suivant
                print(f"{name} : ÉCHEC {e}", flush=True)
                continue
            stereo.mean(axis=0).astype("<f4").tofile(out / f"{name}.f32")
            save_kicks(kicks, out / f"{name}.kicks")
            print(f"{name} : {len(kicks)} kicks", flush=True)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
