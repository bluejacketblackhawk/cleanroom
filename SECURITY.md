# Security Policy

## Reporting a vulnerability

Please report security issues **privately** via GitHub Security Advisories
("Report a vulnerability" on the repository's Security tab) rather than a public issue.
We aim to acknowledge within a few days.

Because ANVIL processes audio entirely locally and ships no server, the most relevant
classes of report are:

- A path by which the app makes a **network connection outside the updater/model-pack
  flows** (this would violate the core privacy guarantee — treat as high severity).
- Unsafe handling of untrusted media files (parser memory-safety, sidecar command
  injection, path traversal on export).
- Model-pack or updater integrity issues (hash/signature bypass).
- Tampering with the ffmpeg sidecar invocation.

## Our posture

- **No telemetry, no crash upload.** Crash dumps are saved locally only.
- Network access is confined to the `updater` and `models` modules and is CI-enforced.
- Sidecars run as separate processes with pipes only; model files are hash-verified
  before load; no dynamic code download, no `eval`.

## Scope

Pre-release software — expect rough edges. Once we tag a release, this policy gets a
supported-versions table.
