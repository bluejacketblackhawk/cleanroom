"""Objective metric implementations for the Cleanroom eval harness (dev-only).

Wired into the runner in M1 lane C. Gates and tools per handoff/06-QUALITY-EVAL.md §2:
loudness accuracy (ebur128 + ffmpeg cross-check), true peak (4x oversample), DNSMOS
P.835, PESQ/STOI (paired synthetic set), WER (jiwer), DER (pyannote.metrics), and
determinism double-render hashes.
"""
