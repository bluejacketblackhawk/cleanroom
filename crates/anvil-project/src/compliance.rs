//! Compliance report generator (04 §S6 "Compliance report" acceptance criteria): renders a
//! mastered file's loudness/true-peak numbers, what each chain module did, and — for ACX —
//! pass/fail against ACX's submission checklist, as a self-contained HTML file and a PDF.
//!
//! [`ComplianceInput`] is a plain data struct built directly from real measurements: the
//! caller (the CLI's `master`/`analyze` commands) passes `anvil analyze`'s output on the
//! source file (the "before" columns) and the rendered output (the "after" columns) straight
//! through, so the report's numbers are guaranteed to equal what `anvil analyze` reports for
//! the same file (04 acceptance: "values match `anvil analyze` of the rendered file within
//! tolerance"). This crate deliberately does not depend on anvil-dsp's `AnalysisReport` type
//! — [`LoudnessMeasurement`]'s field names mirror it so the mapping is a straight copy, but
//! anvil-project stays dependency-light and agnostic of the DSP crate.
//!
//! PDF rendering uses `printpdf` (MIT, pure Rust) with `default-features = false`: only the
//! core page/text/graphics API, none of the heavy HTML-layout dependency chain (`azul-layout`,
//! `rust-fontconfig`) that crate's `html` feature pulls in. Text uses printpdf's built-in
//! Base-14 fonts (Helvetica/Courier), so no font files need to be embedded.

use std::fmt::Write as _;
use std::path::Path;

use printpdf::{
    BuiltinFont, Color, Mm, Op, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point, Pt,
    Rgb, TextItem,
};
use serde::{Deserialize, Serialize};

use anvil_core::Result;

use crate::preset::AUDIOBOOK_ACX_ID;
use crate::Preset;

/// How far the rendered output's integrated loudness may drift from the preset's target and
/// still count as compliant (03 §4.9: the two-pass normalize's own correction-iteration
/// threshold, reused here as the report's pass/fail line).
pub const LOUDNESS_TOLERANCE_LU: f64 = 0.5;

/// ACX's submission checklist (03 §4.9 "ACX audiobook (special...)"): RMS window, peak
/// ceiling, noise-floor ceiling.
pub const ACX_RMS_MIN_DBFS: f64 = -23.0;
pub const ACX_RMS_MAX_DBFS: f64 = -18.0;
pub const ACX_PEAK_MAX_DBFS: f64 = -3.0;
pub const ACX_NOISE_FLOOR_MAX_DBFS: f64 = -60.0;

/// One chain module's decision, as shown in the report's "what ANVIL did" table (03 §3 chain
/// order). Free-form name/detail strings so a new chain module never needs a schema bump
/// here — the DSP crate decides what's worth surfacing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleDecision {
    /// Module name, e.g. `"AI denoise"`, `"De-esser"`, `"Adaptive leveler"`.
    pub name: String,
    /// Whether the module actually ran (`false` = bypassed, e.g. no hum detected).
    pub applied: bool,
    /// One-line human-readable summary of what it did or why it was skipped, matching the
    /// Health Card's plain-language rationale (03 §2).
    pub detail: String,
}

impl ModuleDecision {
    /// A module that ran, with a summary of what it did.
    pub fn applied(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            applied: true,
            detail: detail.into(),
        }
    }

    /// A module that was bypassed, with a reason.
    pub fn bypassed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            applied: false,
            detail: detail.into(),
        }
    }
}

/// Loudness/true-peak measurements for one side of the master (before or after). Field names
/// intentionally mirror `anvil-dsp::AnalysisReport` so the CLI can copy values straight
/// across without renaming anything.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoudnessMeasurement {
    /// Integrated (program) loudness, LUFS (BS.1770-4).
    pub integrated_lufs: f64,
    /// Maximum true peak, dBTP (4× oversampled).
    pub true_peak_dbtp: f64,
    /// Loudness range, LU (EBU Tech 3342).
    pub loudness_range_lu: f64,
}

/// Everything the compliance report needs about one mastered file: the preset used, the
/// loudness/true-peak numbers before and after mastering, and what each chain module did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComplianceInput {
    pub source_file: String,
    pub output_file: String,
    pub preset: Preset,
    /// The shipped preset id used, if any (a [`Preset::by_id`] key, e.g.
    /// `preset::AUDIOBOOK_ACX_ID`). Drives whether the ACX pass/fail section is shown.
    /// `None` for a custom preset — never shows ACX rows even if its numbers happen to land
    /// in ACX's window, since ACX submission is a preset choice, not a measurement.
    pub preset_id: Option<String>,

    pub duration_secs: f64,
    pub sample_rate: u32,
    pub channels: u32,

    pub before: LoudnessMeasurement,
    pub after: LoudnessMeasurement,

    /// RMS level of the rendered output, dBFS. ACX grades on an RMS window, not integrated
    /// LUFS, so this is needed only for [`Self::acx_checks`]; `None` when not measured.
    pub rms_dbfs_out: Option<f64>,
    /// Noise floor of the rendered output, dBFS (ACX's floor check).
    pub noise_floor_dbfs_out: Option<f64>,

    pub modules: Vec<ModuleDecision>,
    /// The chain version the render was produced under (03: "Chain changes bump
    /// `chain_version`"), so an old report can be told apart from a re-render under a newer
    /// chain.
    pub chain_version: u32,
}

/// One ACX submission-checklist row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcxCheck {
    pub label: String,
    pub measured: Option<f64>,
    pub requirement: String,
    pub pass: bool,
}

impl ComplianceInput {
    /// Whether the rendered output's integrated loudness lands within
    /// [`LOUDNESS_TOLERANCE_LU`] of the preset's target.
    pub fn loudness_pass(&self) -> bool {
        (self.after.integrated_lufs - self.preset.target_lufs as f64).abs() <= LOUDNESS_TOLERANCE_LU
    }

    /// Whether the rendered output's true peak respects the preset's ceiling. The limiter
    /// guarantees this on the whole corpus (03 §4.10, CI-enforced zero tolerance); the report
    /// still checks and shows it rather than assuming.
    pub fn true_peak_pass(&self) -> bool {
        self.after.true_peak_dbtp <= self.preset.true_peak_ceiling_dbtp as f64
    }

    /// Whether this input used the shipped ACX preset (drives the report's ACX section).
    pub fn is_acx(&self) -> bool {
        self.preset_id.as_deref() == Some(AUDIOBOOK_ACX_ID)
    }

    /// ACX submission-checklist results (03 §4.9), or `None` if this wasn't an ACX render.
    pub fn acx_checks(&self) -> Option<Vec<AcxCheck>> {
        if !self.is_acx() {
            return None;
        }

        let rms_pass = self
            .rms_dbfs_out
            .map(|rms| (ACX_RMS_MIN_DBFS..=ACX_RMS_MAX_DBFS).contains(&rms))
            .unwrap_or(false);
        let peak_pass = self.after.true_peak_dbtp <= ACX_PEAK_MAX_DBFS;
        let floor_pass = self
            .noise_floor_dbfs_out
            .map(|floor| floor <= ACX_NOISE_FLOOR_MAX_DBFS)
            .unwrap_or(false);

        Some(vec![
            AcxCheck {
                label: "RMS level".into(),
                measured: self.rms_dbfs_out,
                requirement: format!("{ACX_RMS_MIN_DBFS:.0} to {ACX_RMS_MAX_DBFS:.0} dBFS"),
                pass: rms_pass,
            },
            AcxCheck {
                label: "Peak level".into(),
                measured: Some(self.after.true_peak_dbtp),
                requirement: format!("\u{2264} {ACX_PEAK_MAX_DBFS:.0} dBFS"),
                pass: peak_pass,
            },
            AcxCheck {
                label: "Noise floor".into(),
                measured: self.noise_floor_dbfs_out,
                requirement: format!("\u{2264} {ACX_NOISE_FLOOR_MAX_DBFS:.0} dBFS"),
                pass: floor_pass,
            },
        ])
    }

    /// Whether the render is compliant end to end: loudness target, true-peak ceiling, and
    /// (if applicable) every ACX checklist row.
    pub fn overall_pass(&self) -> bool {
        // ACX is graded on its RMS-window / peak / floor checklist, not on a LUFS target, so
        // an ACX render's loudness is judged by `acx_checks` rather than `loudness_pass`.
        let loudness_ok = self.is_acx() || self.loudness_pass();
        loudness_ok
            && self.true_peak_pass()
            && self
                .acx_checks()
                .map(|checks| checks.iter().all(|c| c.pass))
                .unwrap_or(true)
    }
}

/// The gain (and verdict) needed to bring a rendered file into ACX submission spec (03 §4.9):
/// RMS within [`ACX_RMS_MIN_DBFS`]..[`ACX_RMS_MAX_DBFS`] dBFS, true peak at most
/// [`ACX_PEAK_MAX_DBFS`], and a noise floor at most [`ACX_NOISE_FLOOR_MAX_DBFS`]. Produced by
/// [`AcxConform::compute`] from a rendered file's measured RMS/peak/noise floor; the caller
/// applies `gain_db` and re-measures (e.g. via [`ComplianceInput::acx_checks`]) to confirm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcxConform {
    /// Gain, dB, to apply to the render. Chosen to land RMS at the window's midpoint without
    /// pushing true peak past the ceiling; `0.0` when the file already passes every gain-fixable
    /// check, or when peak headroom is too tight to move RMS at all.
    pub gain_db: f32,
    /// Whether applying `gain_db` (and nothing else) is expected to satisfy every ACX check.
    pub will_pass: bool,
    /// Plain-language reasons this can't be fully fixed by gain alone. Empty when `will_pass`
    /// is `true`.
    pub blockers: Vec<String>,
}

impl AcxConform {
    /// Compute the corrective gain for a rendered file's measured RMS, true peak, and noise
    /// floor (all dBFS).
    ///
    /// Two things gain alone can never fix, and this says so rather than returning a
    /// plausible-looking but wrong number:
    /// - **Noise floor.** Raising level raises the noise floor by exactly the same amount — it
    ///   never gets further from the signal. A floor above [`ACX_NOISE_FLOOR_MAX_DBFS`] is a
    ///   room/recording problem, flagged as a blocker with `gain_db` still computed normally
    ///   for whatever it *can* fix.
    /// - **Crest factor.** `peak_dbfs - rms_dbfs` doesn't change with gain (both move together),
    ///   so if the source's peaks are too hot relative to its average level, no single gain
    ///   value lands RMS in the window *and* keeps true peak under the ceiling. The peak
    ///   ceiling is treated as the hard constraint (matches the limiter's own "zero tolerance"
    ///   guarantee, 03 §4.10) and the shortfall is reported rather than silently exceeding it.
    pub fn compute(rms_dbfs: f64, peak_dbfs: f64, noise_floor_dbfs: f64) -> Self {
        let mut blockers = Vec::new();

        let floor_unfixable = noise_floor_dbfs > ACX_NOISE_FLOOR_MAX_DBFS;
        if floor_unfixable {
            blockers.push(format!(
                "Noise floor measures {noise_floor_dbfs:.1} dBFS, above ACX's {ACX_NOISE_FLOOR_MAX_DBFS:.0} dBFS ceiling. \
                 That's a room-noise problem, not a level problem \u{2014} turning the file up raises the hiss by the same \
                 amount it raises the voice. Re-record in a quieter space or increase denoise strength before resubmitting."
            ));
        }

        let already_ok = (ACX_RMS_MIN_DBFS..=ACX_RMS_MAX_DBFS).contains(&rms_dbfs)
            && peak_dbfs <= ACX_PEAK_MAX_DBFS;

        // Largest gain the peak ceiling allows (can be negative if the file already exceeds
        // it). The limiter's ceiling is a hard guarantee elsewhere in the chain (03 §4.10), so
        // this never recommends a gain that would break it, even at the cost of leaving RMS
        // outside its window.
        let max_gain_for_peak = ACX_PEAK_MAX_DBFS - peak_dbfs;
        let rms_target = (ACX_RMS_MIN_DBFS + ACX_RMS_MAX_DBFS) / 2.0;
        let ideal_gain = rms_target - rms_dbfs;

        let gain_db = if already_ok {
            0.0
        } else {
            ideal_gain.min(max_gain_for_peak)
        };

        let resulting_rms = rms_dbfs + gain_db;
        let resulting_peak = peak_dbfs + gain_db;
        let rms_ok = (ACX_RMS_MIN_DBFS..=ACX_RMS_MAX_DBFS).contains(&resulting_rms);
        let peak_ok = resulting_peak <= ACX_PEAK_MAX_DBFS;

        if !rms_ok {
            let crest_factor = peak_dbfs - rms_dbfs;
            blockers.push(format!(
                "This file's peaks are too hot relative to its average level (crest factor {crest_factor:.1} dB) for gain \
                 alone to reach ACX's {ACX_RMS_MIN_DBFS:.0} to {ACX_RMS_MAX_DBFS:.0} dBFS RMS window without exceeding the \
                 {ACX_PEAK_MAX_DBFS:.0} dBFS peak ceiling. Needs compression/limiting on the render, not just a gain change."
            ));
        }

        AcxConform {
            gain_db: gain_db as f32,
            will_pass: rms_ok && peak_ok && !floor_unfixable,
            blockers,
        }
    }
}

fn tier_label(tier: crate::Tier) -> &'static str {
    match tier {
        crate::Tier::Fast => "Fast",
        crate::Tier::Standard => "Standard",
        crate::Tier::Studio => "Studio",
    }
}

fn format_duration(total_secs: f64) -> String {
    let total = total_secs.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn pass_word(pass: bool) -> &'static str {
    if pass {
        "PASS"
    } else {
        "FAIL"
    }
}

fn pass_class(pass: bool) -> &'static str {
    if pass {
        "pass"
    } else {
        "fail"
    }
}

// ---- HTML report ----------------------------------------------------------------------------

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The static document shell: doctype, head, inline CSS (light/dark via
/// `prefers-color-scheme`), and the opening body wrapper. Contains no format placeholders —
/// kept separate from the dynamic sections below so the CSS's literal `{`/`}` never needs
/// escaping.
const HTML_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>ANVIL compliance report</title>
<style>
  :root {
    --bg: #ffffff; --fg: #14171a; --muted: #5b6470; --border: #dfe3e8;
    --pass-bg: #e4f6ea; --pass-fg: #14732f; --fail-bg: #fbe7e7; --fail-fg: #a3231f;
    --card-bg: #f7f8fa;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0f1216; --fg: #eaedf0; --muted: #9aa4b0; --border: #2a2f36;
      --pass-bg: #123321; --pass-fg: #6fdc93; --fail-bg: #3a1616; --fail-fg: #ff8a80;
      --card-bg: #171b20;
    }
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 32px 24px 64px; background: var(--bg); color: var(--fg);
    font: 15px/1.5 -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  }
  .wrap { max-width: 780px; margin: 0 auto; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  h2 { font-size: 16px; margin: 28px 0 10px; border-bottom: 1px solid var(--border); padding-bottom: 6px; }
  .subtitle { color: var(--muted); margin: 0 0 24px; word-break: break-all; }
  .meta { display: grid; grid-template-columns: 1fr 1fr; gap: 10px 24px; background: var(--card-bg);
    border: 1px solid var(--border); border-radius: 8px; padding: 16px; font-size: 13px; }
  .meta div span { color: var(--muted); display: block; margin-bottom: 2px; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 8px 10px; border-bottom: 1px solid var(--border); }
  th { color: var(--muted); font-weight: 600; }
  .badge { display: inline-block; padding: 2px 10px; border-radius: 999px; font-size: 12px; font-weight: 600; }
  .badge.pass { background: var(--pass-bg); color: var(--pass-fg); }
  .badge.fail { background: var(--fail-bg); color: var(--fail-fg); }
  footer { margin-top: 32px; color: var(--muted); font-size: 12px; }
  code { font-family: ui-monospace, Consolas, monospace; }
</style>
</head>
<body>
<div class="wrap">
"#;

const HTML_FOOT: &str = "</div>\n</body>\n</html>\n";

/// Render a self-contained HTML compliance report (04 acceptance: "HTML+PDF open cleanly").
/// One file: inline CSS, no external assets or network calls, readable in both light and dark
/// viewers.
pub fn render_html(input: &ComplianceInput) -> String {
    let mut html = String::with_capacity(4096);
    html.push_str(HTML_HEAD);

    let _ = write!(
        html,
        "<h1>ANVIL compliance report</h1>\n<p class=\"subtitle\">{}</p>\n",
        escape_html(&input.output_file)
    );

    let _ = write!(
        html,
        concat!(
            "<div class=\"meta\">\n",
            "<div><span>Source</span>{}</div>\n",
            "<div><span>Output</span>{}</div>\n",
            "<div><span>Preset</span>{} ({})</div>\n",
            "<div><span>Duration</span>{}</div>\n",
            "<div><span>Sample rate</span>{} Hz, {} ch</div>\n",
            "<div><span>Chain version</span>{}</div>\n",
            "</div>\n",
        ),
        escape_html(&input.source_file),
        escape_html(&input.output_file),
        escape_html(&input.preset.name),
        tier_label(input.preset.tier),
        format_duration(input.duration_secs),
        input.sample_rate,
        input.channels,
        input.chain_version,
    );

    let loudness_pass = input.loudness_pass();
    let true_peak_pass = input.true_peak_pass();
    let _ = write!(
        html,
        concat!(
            "<h2>Loudness</h2>\n",
            "<table>\n<thead><tr><th>Metric</th><th>Before</th><th>After</th>",
            "<th>Target</th><th>Result</th></tr></thead>\n<tbody>\n",
            "<tr><td>Integrated loudness</td><td>{:.2} LUFS</td><td>{:.2} LUFS</td>",
            "<td>{:.1} LUFS (\u{00b1}{:.1})</td>",
            "<td><span class=\"badge {}\">{}</span></td></tr>\n",
            "<tr><td>True peak</td><td>{:.2} dBTP</td><td>{:.2} dBTP</td>",
            "<td>\u{2264} {:.1} dBTP</td>",
            "<td><span class=\"badge {}\">{}</span></td></tr>\n",
            "<tr><td>Loudness range</td><td>{:.2} LU</td><td>{:.2} LU</td><td>\u{2014}</td>",
            "<td>\u{2014}</td></tr>\n",
            "</tbody>\n</table>\n",
        ),
        input.before.integrated_lufs,
        input.after.integrated_lufs,
        input.preset.target_lufs,
        LOUDNESS_TOLERANCE_LU,
        pass_class(loudness_pass),
        pass_word(loudness_pass),
        input.before.true_peak_dbtp,
        input.after.true_peak_dbtp,
        input.preset.true_peak_ceiling_dbtp,
        pass_class(true_peak_pass),
        pass_word(true_peak_pass),
        input.before.loudness_range_lu,
        input.after.loudness_range_lu,
    );

    if let Some(checks) = input.acx_checks() {
        html.push_str("<h2>ACX submission checklist</h2>\n<table>\n<thead><tr><th>Check</th><th>Measured</th><th>Requirement</th><th>Result</th></tr></thead>\n<tbody>\n");
        for check in &checks {
            let measured = check
                .measured
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "n/a".to_string());
            let _ = writeln!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><span class=\"badge {}\">{}</span></td></tr>",
                escape_html(&check.label),
                measured,
                escape_html(&check.requirement),
                pass_class(check.pass),
                pass_word(check.pass),
            );
        }
        html.push_str("</tbody>\n</table>\n");
    }

    html.push_str("<h2>What ANVIL did</h2>\n<table>\n<thead><tr><th>Module</th><th>Status</th><th>Detail</th></tr></thead>\n<tbody>\n");
    for module in &input.modules {
        let _ = writeln!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&module.name),
            if module.applied {
                "Applied"
            } else {
                "Bypassed"
            },
            escape_html(&module.detail),
        );
    }
    html.push_str("</tbody>\n</table>\n");

    let _ = writeln!(
        html,
        "<footer>Chain version {} \u{b7} numbers match <code>anvil analyze</code> for {}.</footer>",
        input.chain_version,
        escape_html(&input.output_file),
    );

    html.push_str(HTML_FOOT);
    html
}

// ---- PDF report -------------------------------------------------------------------------------

const PAGE_WIDTH_MM: f32 = 210.0;
const PAGE_HEIGHT_MM: f32 = 297.0;
const MARGIN_MM: f32 = 20.0;
/// Conservative line budget per page: body leading is 14pt (~4.9mm), and the usable height
/// (297 − 2×20mm margins) is ~257mm, so 46 lines (~226mm) leaves headroom for the handful of
/// larger heading lines that only count as one line each toward this budget.
const MAX_LINES_PER_PAGE: u32 = 46;
const BODY_SIZE_PT: f32 = 10.0;
const TITLE_SIZE_PT: f32 = 18.0;
const SECTION_SIZE_PT: f32 = 12.0;
const BLACK: (f32, f32, f32) = (0.08, 0.09, 0.1);
const MUTED: (f32, f32, f32) = (0.35, 0.38, 0.42);
const PASS_COLOR: (f32, f32, f32) = (0.08, 0.45, 0.18);
const FAIL_COLOR: (f32, f32, f32) = (0.64, 0.14, 0.12);

/// Accumulates report lines into one or more A4 [`PdfPage`]s, starting a new page whenever
/// the current one fills up.
struct PdfWriter {
    pages: Vec<PdfPage>,
    ops: Vec<Op>,
    lines_on_page: u32,
}

impl PdfWriter {
    fn new() -> Self {
        let mut writer = Self {
            pages: Vec::new(),
            ops: Vec::new(),
            lines_on_page: 0,
        };
        writer.begin_text_section();
        writer
    }

    fn begin_text_section(&mut self) {
        self.ops.push(Op::StartTextSection);
        self.ops.push(Op::SetTextCursor {
            pos: Point::new(Mm(MARGIN_MM), Mm(PAGE_HEIGHT_MM - MARGIN_MM)),
        });
        self.lines_on_page = 0;
    }

    fn start_new_page(&mut self) {
        self.ops.push(Op::EndTextSection);
        let ops = std::mem::take(&mut self.ops);
        self.pages
            .push(PdfPage::new(Mm(PAGE_WIDTH_MM), Mm(PAGE_HEIGHT_MM), ops));
        self.begin_text_section();
    }

    fn ensure_room(&mut self) {
        if self.lines_on_page >= MAX_LINES_PER_PAGE {
            self.start_new_page();
        }
    }

    /// Emit one line of text and advance the cursor. `size_pt` also sets that line's
    /// leading (roughly 1.35× the size), so mixing heading and body sizes in the same text
    /// section doesn't overlap.
    fn line(&mut self, text: &str, size_pt: f32, bold: bool, color: (f32, f32, f32)) {
        self.ensure_room();
        let leading = size_pt * 1.35;
        let font = if bold {
            BuiltinFont::HelveticaBold
        } else {
            BuiltinFont::Helvetica
        };
        self.ops.push(Op::SetLineHeight { lh: Pt(leading) });
        self.ops.push(Op::SetFont {
            font: PdfFontHandle::Builtin(font),
            size: Pt(size_pt),
        });
        self.ops.push(Op::SetFillColor {
            col: Color::Rgb(Rgb {
                r: color.0,
                g: color.1,
                b: color.2,
                icc_profile: None,
            }),
        });
        self.ops.push(Op::ShowText {
            items: vec![TextItem::Text(text.to_string())],
        });
        self.ops.push(Op::AddLineBreak);
        self.lines_on_page += 1;
    }

    /// A monospace line (Courier), used for the numeric tables so columns line up under
    /// fixed-width padding.
    fn mono_line(&mut self, text: &str, color: (f32, f32, f32)) {
        self.ensure_room();
        let leading = BODY_SIZE_PT * 1.35;
        self.ops.push(Op::SetLineHeight { lh: Pt(leading) });
        self.ops.push(Op::SetFont {
            font: PdfFontHandle::Builtin(BuiltinFont::Courier),
            size: Pt(BODY_SIZE_PT),
        });
        self.ops.push(Op::SetFillColor {
            col: Color::Rgb(Rgb {
                r: color.0,
                g: color.1,
                b: color.2,
                icc_profile: None,
            }),
        });
        self.ops.push(Op::ShowText {
            items: vec![TextItem::Text(text.to_string())],
        });
        self.ops.push(Op::AddLineBreak);
        self.lines_on_page += 1;
    }

    fn blank(&mut self) {
        self.ensure_room();
        self.ops.push(Op::AddLineBreak);
        self.lines_on_page += 1;
    }

    fn finish(mut self) -> Vec<PdfPage> {
        self.ops.push(Op::EndTextSection);
        self.pages.push(PdfPage::new(
            Mm(PAGE_WIDTH_MM),
            Mm(PAGE_HEIGHT_MM),
            self.ops,
        ));
        self.pages
    }
}

/// Render the same report as a PDF using a pure-Rust PDF writer (`printpdf`, MIT) — no
/// external tool, browser, or network call required (03/07 offline posture). A clean tabular
/// layout via a monospace font, not a pixel-perfect design; if that stops being good enough,
/// the HTML report is print-optimized as a fallback (04 acceptance only requires both formats
/// "open cleanly" with matching numbers).
pub fn render_pdf(input: &ComplianceInput) -> Result<Vec<u8>> {
    let mut w = PdfWriter::new();

    w.line("ANVIL compliance report", TITLE_SIZE_PT, true, BLACK);
    w.line(&input.output_file, BODY_SIZE_PT, false, MUTED);
    w.blank();

    w.line("Overview", SECTION_SIZE_PT, true, BLACK);
    w.mono_line(&format!("Source          {}", input.source_file), BLACK);
    w.mono_line(&format!("Output          {}", input.output_file), BLACK);
    w.mono_line(
        &format!(
            "Preset          {} ({})",
            input.preset.name,
            tier_label(input.preset.tier)
        ),
        BLACK,
    );
    w.mono_line(
        &format!("Duration        {}", format_duration(input.duration_secs)),
        BLACK,
    );
    w.mono_line(
        &format!(
            "Format          {} Hz, {} ch",
            input.sample_rate, input.channels
        ),
        BLACK,
    );
    w.mono_line(&format!("Chain version   {}", input.chain_version), BLACK);
    w.blank();

    let loudness_pass = input.loudness_pass();
    let true_peak_pass = input.true_peak_pass();

    w.line("Loudness", SECTION_SIZE_PT, true, BLACK);
    w.mono_line(
        &format!(
            "{:<20}{:>12}{:>12}{:>16}",
            "Metric", "Before", "After", "Target"
        ),
        MUTED,
    );
    w.mono_line(
        &format!(
            "{:<20}{:>9.2} LU{:>9.2} LU{:>10.1} LU  [{}]",
            "Integrated loudness",
            input.before.integrated_lufs,
            input.after.integrated_lufs,
            input.preset.target_lufs,
            pass_word(loudness_pass),
        ),
        if loudness_pass {
            PASS_COLOR
        } else {
            FAIL_COLOR
        },
    );
    w.mono_line(
        &format!(
            "{:<20}{:>9.2} dB{:>9.2} dB{:>9.1} dB  [{}]",
            "True peak",
            input.before.true_peak_dbtp,
            input.after.true_peak_dbtp,
            input.preset.true_peak_ceiling_dbtp,
            pass_word(true_peak_pass),
        ),
        if true_peak_pass {
            PASS_COLOR
        } else {
            FAIL_COLOR
        },
    );
    w.mono_line(
        &format!(
            "{:<20}{:>9.2} LU{:>9.2} LU{:>13}",
            "Loudness range", input.before.loudness_range_lu, input.after.loudness_range_lu, "-",
        ),
        BLACK,
    );
    w.blank();

    if let Some(checks) = input.acx_checks() {
        w.line("ACX submission checklist", SECTION_SIZE_PT, true, BLACK);
        for check in &checks {
            let measured = check
                .measured
                .map(|v| format!("{v:.2} dBFS"))
                .unwrap_or_else(|| "n/a".to_string());
            w.mono_line(
                &format!(
                    "{:<14}{:>12}   requires {:<20}[{}]",
                    check.label,
                    measured,
                    check.requirement,
                    pass_word(check.pass),
                ),
                if check.pass { PASS_COLOR } else { FAIL_COLOR },
            );
        }
        w.blank();
    }

    w.line("What ANVIL did", SECTION_SIZE_PT, true, BLACK);
    if input.modules.is_empty() {
        w.mono_line("(no module decisions recorded)", MUTED);
    }
    for module in &input.modules {
        let marker = if module.applied { "x" } else { " " };
        w.mono_line(
            &format!("[{marker}] {:<24} {}", module.name, module.detail),
            BLACK,
        );
    }
    w.blank();

    w.line(
        &format!(
            "Chain version {} \u{b7} numbers match `anvil analyze` for {}.",
            input.chain_version, input.output_file
        ),
        8.0,
        false,
        MUTED,
    );

    let pages = w.finish();
    let mut doc = PdfDocument::new("ANVIL compliance report");
    let mut warnings = Vec::new();
    let bytes = doc
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut warnings);
    Ok(bytes)
}

/// Convenience: render both formats and write them next to `output_file` (or wherever the
/// caller points `html_path`/`pdf_path`), e.g. `episode.wav.report.html` /
/// `episode.wav.report.pdf`. Split out for callers (the CLI) who just want "give me both
/// files on disk" without re-deriving the write-temp-then-rename dance for a
/// non-schema-versioned, throwaway-if-re-rendered report.
pub fn write_reports(input: &ComplianceInput, html_path: &Path, pdf_path: &Path) -> Result<()> {
    let html = render_html(input);
    if let Some(parent) = html_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(html_path, html)?;

    let pdf = render_pdf(input)?;
    if let Some(parent) = pdf_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(pdf_path, pdf)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tier;

    fn sample_input(preset_id: Option<&str>) -> ComplianceInput {
        let preset = preset_id.and_then(Preset::by_id).unwrap_or_default();
        ComplianceInput {
            source_file: "raw/episode-42.wav".into(),
            output_file: "episode-42_mastered.wav".into(),
            preset,
            preset_id: preset_id.map(str::to_string),
            duration_secs: 3725.0, // 1:02:05
            sample_rate: 48_000,
            channels: 2,
            before: LoudnessMeasurement {
                integrated_lufs: -22.40,
                true_peak_dbtp: -6.10,
                loudness_range_lu: 9.8,
            },
            after: LoudnessMeasurement {
                integrated_lufs: -16.20,
                true_peak_dbtp: -1.50,
                loudness_range_lu: 7.2,
            },
            rms_dbfs_out: Some(-20.0),
            noise_floor_dbfs_out: Some(-65.0),
            modules: vec![
                ModuleDecision::applied("AI denoise", "SNR 28 dB -> strength 0.45"),
                ModuleDecision::bypassed("De-hum", "no stable 50/60 Hz peak detected"),
                ModuleDecision::applied("Adaptive leveler", "gain range -4.2..+3.1 dB"),
            ],
            chain_version: 3,
        }
    }

    #[test]
    fn non_acx_preset_has_no_acx_section() {
        let input = sample_input(None);
        assert!(input.acx_checks().is_none());
        assert!(!input.is_acx());
    }

    #[test]
    fn acx_checks_pass_for_compliant_render() {
        let mut input = sample_input(Some(AUDIOBOOK_ACX_ID));
        input.after.true_peak_dbtp = -3.4;
        input.rms_dbfs_out = Some(-20.0);
        input.noise_floor_dbfs_out = Some(-65.0);

        let checks = input.acx_checks().expect("acx preset must produce checks");
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.pass), "{checks:?}");
        assert!(input.overall_pass());
    }

    #[test]
    fn acx_checks_fail_when_rms_out_of_window() {
        let mut input = sample_input(Some(AUDIOBOOK_ACX_ID));
        input.rms_dbfs_out = Some(-10.0); // too loud for ACX's -23..-18 window
        input.after.true_peak_dbtp = -3.4;
        input.noise_floor_dbfs_out = Some(-65.0);

        let checks = input.acx_checks().unwrap();
        let rms = checks.iter().find(|c| c.label == "RMS level").unwrap();
        assert!(!rms.pass);
        assert!(!input.overall_pass());
    }

    #[test]
    fn acx_checks_fail_when_peak_too_hot() {
        let mut input = sample_input(Some(AUDIOBOOK_ACX_ID));
        input.after.true_peak_dbtp = -2.0; // ACX requires <= -3 dBFS
        input.rms_dbfs_out = Some(-20.0);
        input.noise_floor_dbfs_out = Some(-65.0);

        let checks = input.acx_checks().unwrap();
        let peak = checks.iter().find(|c| c.label == "Peak level").unwrap();
        assert!(!peak.pass);
    }

    #[test]
    fn acx_checks_fail_when_measurement_missing() {
        let mut input = sample_input(Some(AUDIOBOOK_ACX_ID));
        input.rms_dbfs_out = None;
        input.noise_floor_dbfs_out = None;

        let checks = input.acx_checks().unwrap();
        assert!(!checks.iter().find(|c| c.label == "RMS level").unwrap().pass);
        assert!(
            !checks
                .iter()
                .find(|c| c.label == "Noise floor")
                .unwrap()
                .pass
        );
    }

    #[test]
    fn loudness_pass_within_tolerance() {
        let mut input = sample_input(None);
        input.preset.target_lufs = -16.0;
        input.after.integrated_lufs = -16.4;
        assert!(input.loudness_pass());
        input.after.integrated_lufs = -17.0;
        assert!(!input.loudness_pass());
    }

    #[test]
    fn true_peak_pass_respects_ceiling() {
        let mut input = sample_input(None);
        input.preset.true_peak_ceiling_dbtp = -1.0;
        input.after.true_peak_dbtp = -1.5;
        assert!(input.true_peak_pass());
        input.after.true_peak_dbtp = -0.5;
        assert!(!input.true_peak_pass());
    }

    #[test]
    fn render_html_contains_key_values() {
        let input = sample_input(Some(AUDIOBOOK_ACX_ID));
        let html = render_html(&input);

        assert!(html.contains("episode-42_mastered.wav"));
        assert!(html.contains("Audiobook (ACX)"));
        assert!(html.contains("-16.20 LUFS"));
        assert!(html.contains("-1.50 dBTP"));
        assert!(html.contains("ACX submission checklist"));
        assert!(html.contains("AI denoise"));
        assert!(html.contains("De-hum"));
    }

    #[test]
    fn render_html_omits_acx_section_for_non_acx_preset() {
        let input = sample_input(None);
        let html = render_html(&input);
        assert!(!html.contains("ACX submission checklist"));
    }

    #[test]
    fn render_html_is_well_formed() {
        let input = sample_input(Some(AUDIOBOOK_ACX_ID));
        let html = render_html(&input);

        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        for (open, close) in [
            ("<html", "</html>"),
            ("<head>", "</head>"),
            ("<body>", "</body>"),
            ("<style>", "</style>"),
        ] {
            assert_eq!(
                html.matches(open).count(),
                html.matches(close).count(),
                "mismatched {open}/{close}"
            );
        }
        // Every opened <table> is closed, and every <tr> opened is closed.
        assert_eq!(
            html.matches("<table>").count(),
            html.matches("</table>").count()
        );
        assert_eq!(html.matches("<tr>").count(), html.matches("</tr>").count());
    }

    #[test]
    fn render_html_escapes_untrusted_strings() {
        let mut input = sample_input(None);
        input.source_file = "<script>alert(1)</script>.wav".into();
        let html = render_html(&input);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn render_pdf_opens_and_has_pages() {
        let input = sample_input(Some(AUDIOBOOK_ACX_ID));
        let bytes = render_pdf(&input).expect("pdf render");
        assert!(!bytes.is_empty());

        let mut warnings = Vec::new();
        let parsed =
            PdfDocument::parse(&bytes, &printpdf::PdfParseOptions::default(), &mut warnings)
                .expect("generated PDF must parse back cleanly");
        assert!(!parsed.pages.is_empty());
    }

    #[test]
    fn render_pdf_paginates_long_module_lists() {
        let mut input = sample_input(None);
        input.modules = (0..200)
            .map(|i| ModuleDecision::applied(format!("Module {i}"), "did a thing"))
            .collect();

        let bytes = render_pdf(&input).expect("pdf render");
        let mut warnings = Vec::new();
        let parsed =
            PdfDocument::parse(&bytes, &printpdf::PdfParseOptions::default(), &mut warnings)
                .expect("generated PDF must parse back cleanly");
        assert!(
            parsed.pages.len() > 1,
            "200 module rows should overflow onto a second page, got {} page(s)",
            parsed.pages.len()
        );
    }

    #[test]
    fn write_reports_writes_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        let html_path = tmp.path().join("report.html");
        let pdf_path = tmp.path().join("report.pdf");
        let input = sample_input(Some(AUDIOBOOK_ACX_ID));

        write_reports(&input, &html_path, &pdf_path).unwrap();

        assert!(html_path.exists());
        assert!(pdf_path.exists());
        assert!(std::fs::read_to_string(&html_path)
            .unwrap()
            .starts_with("<!doctype html>"));
    }

    #[test]
    fn tier_label_covers_all_variants() {
        assert_eq!(tier_label(Tier::Fast), "Fast");
        assert_eq!(tier_label(Tier::Standard), "Standard");
        assert_eq!(tier_label(Tier::Studio), "Studio");
    }

    // ---- AcxConform ----------------------------------------------------------------------

    #[test]
    fn acx_conform_already_compliant_needs_no_gain() {
        let conform = AcxConform::compute(-20.0, -4.0, -65.0);
        assert_eq!(conform.gain_db, 0.0);
        assert!(conform.will_pass);
        assert!(conform.blockers.is_empty());
    }

    #[test]
    fn acx_conform_lifts_a_too_quiet_file_into_the_window() {
        let conform = AcxConform::compute(-30.0, -20.0, -65.0);
        assert!(conform.will_pass, "{conform:?}");
        assert!(conform.blockers.is_empty());

        let resulting_rms = -30.0 + conform.gain_db as f64;
        assert!(
            (ACX_RMS_MIN_DBFS..=ACX_RMS_MAX_DBFS).contains(&resulting_rms),
            "resulting RMS {resulting_rms} not inside the ACX window"
        );
        let resulting_peak = -20.0 + conform.gain_db as f64;
        assert!(resulting_peak <= ACX_PEAK_MAX_DBFS);
        assert!(
            conform.gain_db > 0.0,
            "a too-quiet file needs positive gain"
        );
    }

    #[test]
    fn acx_conform_pulls_down_a_too_loud_file_into_the_window() {
        let conform = AcxConform::compute(-10.0, -4.0, -65.0);
        assert!(conform.will_pass, "{conform:?}");
        assert!(conform.blockers.is_empty());

        let resulting_rms = -10.0 + conform.gain_db as f64;
        assert!(
            (ACX_RMS_MIN_DBFS..=ACX_RMS_MAX_DBFS).contains(&resulting_rms),
            "resulting RMS {resulting_rms} not inside the ACX window"
        );
        let resulting_peak = -4.0 + conform.gain_db as f64;
        assert!(resulting_peak <= ACX_PEAK_MAX_DBFS);
        assert!(conform.gain_db < 0.0, "a too-loud file needs negative gain");
    }

    #[test]
    fn acx_conform_reports_high_noise_floor_as_unfixable() {
        // RMS and peak are already inside spec; only the noise floor is a problem.
        let conform = AcxConform::compute(-20.0, -4.0, -40.0);

        assert!(!conform.will_pass);
        assert!(
            !conform.blockers.is_empty(),
            "a high noise floor must produce a clear blocker string"
        );
        assert!(
            conform
                .blockers
                .iter()
                .any(|b| b.to_lowercase().contains("noise floor")),
            "blocker should name the noise floor: {:?}",
            conform.blockers
        );
        // Not a bogus/huge correction: RMS and peak were already fine, so no gain is needed
        // (and none would help the actual problem).
        assert_eq!(conform.gain_db, 0.0);
    }

    #[test]
    fn acx_conform_still_computes_sane_gain_when_floor_is_also_unfixable() {
        // Too quiet AND noisy: gain should still chase the RMS window sensibly rather than
        // being zeroed out or nonsensical just because the file can never fully pass.
        let conform = AcxConform::compute(-30.0, -20.0, -40.0);

        assert!(!conform.will_pass);
        assert!(conform
            .blockers
            .iter()
            .any(|b| b.to_lowercase().contains("noise floor")));
        assert!(
            (conform.gain_db - 9.5).abs() < 0.01,
            "expected the same sensible RMS-targeting gain as the clean too-quiet case, got {}",
            conform.gain_db
        );
    }

    #[test]
    fn acx_conform_flags_crest_factor_it_cannot_fix_with_gain_alone() {
        // Very quiet on average but already close to the peak ceiling: no single gain value
        // can lift RMS into the window without breaking the peak ceiling.
        let conform = AcxConform::compute(-30.0, -3.5, -65.0);

        assert!(!conform.will_pass);
        assert!(!conform.blockers.is_empty());
        assert!(conform
            .blockers
            .iter()
            .any(|b| b.to_lowercase().contains("crest factor")));

        // The peak ceiling must never be violated, even though RMS can't be fully fixed.
        let resulting_peak = -3.5 + conform.gain_db as f64;
        assert!(resulting_peak <= ACX_PEAK_MAX_DBFS + 1e-6);
    }
}
