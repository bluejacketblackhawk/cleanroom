<#
.SYNOPSIS
  Build the synthetic 3-speaker diarization fixture (audio + ground-truth RTTM).

.DESCRIPTION
  Cleanroom's diarization quality gate is DER <= 20% (handoff/06-QUALITY-EVAL.md §2). The AMI
  corpus is not redistributable and needs a licence click-through, so the offline gate runs
  against a *synthetic* conversation whose ground truth is exact by construction: each turn
  is rendered by a different Windows TTS voice, and the turn boundaries are known to the
  sample because this script lays the turns out itself.

  Voices come from the WinRT synthesizer (Windows.Media.SpeechSynthesis), which exposes the
  OneCore voices — Microsoft David (M), Microsoft Mark (M) and Microsoft Zira (F). Two of
  the three are male, so the fixture is not the trivially easy male-vs-female case.

  Output (into -OutDir):
    diarization-3spk.wav   16 kHz mono 16-bit PCM, ~2 minutes
    diarization-3spk.rttm  NIST RTTM ground truth, one line per turn

  Run it once, then point the gated test at the results:
    $env:CLEANROOM_DIAR_TEST_AUDIO = "<OutDir>\diarization-3spk.wav"
    $env:CLEANROOM_DIAR_TEST_RTTM  = "<OutDir>\diarization-3spk.rttm"
    cargo test -p anvil-asr -- --nocapture

.PARAMETER OutDir
  Directory the .wav and .rttm are written to. Created if missing.

.PARAMETER Ffmpeg
  Path to ffmpeg, used only to normalise each TTS render to 16 kHz mono 16-bit PCM.
  Defaults to $env:CLEANROOM_FFMPEG, then "ffmpeg" on PATH.

.PARAMETER GapSeconds
  Silence inserted between turns. 0.4 s is a natural conversational beat and is longer than
  the diarizer's default 0.3 s min-duration-on, so it does not create phantom turns.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Ffmpeg = $(if ($env:CLEANROOM_FFMPEG) { $env:CLEANROOM_FFMPEG } else { 'ffmpeg' }),
    [double]$GapSeconds = 0.4
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$SampleRate = 16000

# --- the conversation ---------------------------------------------------------------------
# A three-way podcast-ish exchange. Turns are long enough (> 2 s) that a real diarizer has
# something to embed, and speakers interleave rather than running in three solid blocks, so
# clustering has to actually track who is talking rather than just find two cut points.
$Turns = @(
    @{ Voice = 'Microsoft David'; Text = 'Welcome back to the show. Today we are talking about how audio mastering actually works, and why so much of it still happens in the cloud.' },
    @{ Voice = 'Microsoft Zira'; Text = 'Thanks for having me. I think the honest answer is that the cloud was simply easier for the people building these tools, not better for the people using them.' },
    @{ Voice = 'Microsoft Mark'; Text = 'I would push back on that a little. There were real compute constraints five years ago. A laptop could not run these models at any reasonable speed.' },
    @{ Voice = 'Microsoft David'; Text = 'That is fair. But laptops got a lot faster, and the models got a lot smaller. So what is the excuse now?' },
    @{ Voice = 'Microsoft Zira'; Text = 'Recurring revenue. Once you have a subscription business, moving the processing back onto the user machine is a threat to the business model, not a feature.' },
    @{ Voice = 'Microsoft Mark'; Text = 'That is cynical, although I will admit it is hard to argue with. The engineering case for local processing is stronger every year.' },
    @{ Voice = 'Microsoft David'; Text = 'Let us talk about the actual chain. What happens to a raw voice recording between the microphone and the finished episode?' },
    @{ Voice = 'Microsoft Zira'; Text = 'Roughly: clean it up, level it out, and make it loud enough. Noise reduction first, then compression, then loudness normalisation to a broadcast target.' },
    @{ Voice = 'Microsoft Mark'; Text = 'And crucially, in that order. If you compress before you denoise, you pull the noise floor up and you can never put it back down again.' },
    @{ Voice = 'Microsoft David'; Text = 'That is the mistake I hear most often in podcasts that were mastered by somebody in a hurry.' },
    @{ Voice = 'Microsoft Zira'; Text = 'The other one is chasing loudness. People push everything to the ceiling and wonder why an hour of it is exhausting to listen to.' },
    @{ Voice = 'Microsoft Mark'; Text = 'Dynamics are not a bug. A conversation has quiet moments, and squashing them flat takes the life out of the recording.' },
    @{ Voice = 'Microsoft David'; Text = 'Alright. Last question, and I want a real answer from both of you. Can a free tool actually beat the paid ones?' },
    @{ Voice = 'Microsoft Zira'; Text = 'On quality, yes, because the good models are open. On polish, that is where the paid tools still earn their money.' },
    @{ Voice = 'Microsoft Mark'; Text = 'Agreed. The algorithms are not the moat any more. The moat is whether the thing feels finished when you open it.' }
)

# --- WinRT TTS ----------------------------------------------------------------------------
# PowerShell 5.1 cannot await a WinRT IAsyncOperation directly; this is the standard
# AsTask reflection bridge.
Add-Type -AssemblyName System.Runtime.WindowsRuntime | Out-Null
[Windows.Media.SpeechSynthesis.SpeechSynthesizer, Windows.Media, ContentType = WindowsRuntime] | Out-Null
[Windows.Storage.Streams.DataReader, Windows.Storage.Streams, ContentType = WindowsRuntime] | Out-Null

$asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and
        $_.GetParameters().Count -eq 1 -and
        $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
    })[0]

function Await-WinRT($op, $resultType) {
    $task = $asTaskGeneric.MakeGenericMethod($resultType).Invoke($null, @($op))
    $task.Wait(-1) | Out-Null
    $task.Result
}

$synth = New-Object Windows.Media.SpeechSynthesis.SpeechSynthesizer
$allVoices = [Windows.Media.SpeechSynthesis.SpeechSynthesizer]::AllVoices
$wanted = $Turns | ForEach-Object { $_.Voice } | Select-Object -Unique
foreach ($name in $wanted) {
    if (-not ($allVoices | Where-Object { $_.DisplayName -eq $name })) {
        throw "TTS voice '$name' is not installed. Installed: $(($allVoices | ForEach-Object { $_.DisplayName }) -join ', ')"
    }
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("anvil-diar-fixture-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir | Out-Null }

# Read the `data` chunk of a RIFF/WAVE file, walking the chunk list rather than assuming a
# 44-byte header (WinRT's WAVs carry extra chunks).
function Get-WavPcm([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    if ([System.Text.Encoding]::ASCII.GetString($bytes, 0, 4) -ne 'RIFF') { throw "not a RIFF file: $path" }
    $pos = 12
    while ($pos + 8 -le $bytes.Length) {
        $id = [System.Text.Encoding]::ASCII.GetString($bytes, $pos, 4)
        $size = [BitConverter]::ToUInt32($bytes, $pos + 4)
        $body = $pos + 8
        if ($id -eq 'data') {
            $size = [Math]::Min([int64]$size, [int64]($bytes.Length - $body))
            $out = New-Object byte[] $size
            [Array]::Copy($bytes, $body, $out, 0, $size)
            return , $out                    # comma: keep the array whole, don't unroll it

        }
        $pos = $body + $size + ($size % 2)   # chunks are word-aligned
    }
    throw "no data chunk in $path"
}

$pcm = New-Object System.Collections.Generic.List[byte]
$gapBytes = [int]([Math]::Round($GapSeconds * $SampleRate)) * 2
$gap = New-Object byte[] $gapBytes
$rttm = New-Object System.Collections.Generic.List[string]
$fileId = 'diarization-3spk'

for ($i = 0; $i -lt $Turns.Count; $i++) {
    $turn = $Turns[$i]
    $voice = $allVoices | Where-Object { $_.DisplayName -eq $turn.Voice } | Select-Object -First 1
    $synth.Voice = $voice

    $raw = Join-Path $work "turn$i.raw.wav"
    $norm = Join-Path $work "turn$i.wav"

    $stream = Await-WinRT $synth.SynthesizeTextToStreamAsync($turn.Text) ([Windows.Media.SpeechSynthesis.SpeechSynthesisStream])
    $size = [uint32]$stream.Size
    $reader = New-Object Windows.Storage.Streams.DataReader($stream.GetInputStreamAt(0))
    Await-WinRT $reader.LoadAsync($size) ([uint32]) | Out-Null
    $buf = New-Object byte[] $size
    $reader.ReadBytes($buf)
    [System.IO.File]::WriteAllBytes($raw, $buf)
    $reader.Dispose()
    $stream.Dispose()

    # Normalise to exactly 16 kHz / mono / s16 so the concatenation below is a plain byte splice.
    & $Ffmpeg -hide_banner -loglevel error -y -i $raw -ar $SampleRate -ac 1 -c:a pcm_s16le $norm
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg failed on $raw (exit $LASTEXITCODE)" }

    $data = Get-WavPcm $norm

    $startSample = $pcm.Count / 2
    $pcm.AddRange($data)
    $endSample = $pcm.Count / 2
    if ($i -lt $Turns.Count - 1) { $pcm.AddRange($gap) }

    $start = [Math]::Round($startSample / $SampleRate, 3)
    $dur = [Math]::Round(($endSample - $startSample) / $SampleRate, 3)
    $spk = $turn.Voice -replace '\s+', '_'
    $rttm.Add(("SPEAKER {0} 1 {1:F3} {2:F3} <NA> <NA> {3} <NA> <NA>" -f $fileId, $start, $dur, $spk))

    Write-Host ("turn {0,2}  {1,-16}  {2,7:F3} .. {3,7:F3}" -f $i, $turn.Voice, $start, ($start + $dur))
}

# --- write the fixture --------------------------------------------------------------------
$wavPath = Join-Path $OutDir "$fileId.wav"
$rttmPath = Join-Path $OutDir "$fileId.rttm"

$dataBytes = $pcm.ToArray()
$fs = [System.IO.File]::Create($wavPath)
$bw = New-Object System.IO.BinaryWriter($fs)
$bw.Write([System.Text.Encoding]::ASCII.GetBytes('RIFF'))
$bw.Write([uint32](36 + $dataBytes.Length))
$bw.Write([System.Text.Encoding]::ASCII.GetBytes('WAVE'))
$bw.Write([System.Text.Encoding]::ASCII.GetBytes('fmt '))
$bw.Write([uint32]16)                       # PCM fmt chunk size
$bw.Write([uint16]1)                        # format = PCM
$bw.Write([uint16]1)                        # channels = mono
$bw.Write([uint32]$SampleRate)
$bw.Write([uint32]($SampleRate * 2))        # byte rate
$bw.Write([uint16]2)                        # block align
$bw.Write([uint16]16)                       # bits per sample
$bw.Write([System.Text.Encoding]::ASCII.GetBytes('data'))
$bw.Write([uint32]$dataBytes.Length)
$bw.Write($dataBytes)
$bw.Flush(); $bw.Close(); $fs.Close()

[System.IO.File]::WriteAllLines($rttmPath, $rttm)
Remove-Item -Recurse -Force $work

$total = [Math]::Round($dataBytes.Length / 2 / $SampleRate, 2)
Write-Host ""
Write-Host "wrote $wavPath  ($total s, 16 kHz mono)"
Write-Host "wrote $rttmPath ($($rttm.Count) turns, $($wanted.Count) speakers)"
