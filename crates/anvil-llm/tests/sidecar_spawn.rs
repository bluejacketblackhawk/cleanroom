//! The sidecar really spawns a process — proved without a 4.7 GB model.
//!
//! The unit tests cover the pipeline against a scripted [`Completer`]; the gated test in
//! `real_generation.rs` covers a real Qwen2.5. Between them sits the part neither exercises on
//! a machine with no llama.cpp: `Command` → temp prompt file → argv → stdout → JSON. This test
//! covers exactly that, by pointing `CLEANROOM_LLAMA` at **this test binary**, which re-executes
//! itself as a stub `llama-cli`.
//!
//! The stub is not a toy: it asserts the argv is a valid llama-cli invocation, reads the
//! prompt out of the `-f` file, checks it is the ChatML we think we are sending, and answers
//! with JSON wrapped in the sort of chatter a real model produces.
//!
//! `harness = false` (see Cargo.toml): the stub needs a plain `fn main`, not libtest.

use std::path::{Path, PathBuf};

use anvil_llm::rubric::{self, RubricContext};
use anvil_llm::{generate, GenerateOptions, LlamaSidecar, TranscriptInput, TranscriptSegment};

/// Set on the child so it knows to be a model instead of a test runner.
const STUB_ENV: &str = "CLEANROOM_LLM_STUB";

fn main() {
    if std::env::var_os(STUB_ENV).is_some() {
        stub_llama_cli();
        return;
    }
    run_test();
    println!("test sidecar_spawn ... ok");
}

// --- the test ---------------------------------------------------------------------------

fn run_test() {
    let exe = std::env::current_exe().expect("current exe");
    let model = fake_model();

    // The child inherits this env, and re-enters main() as the stub.
    std::env::set_var(STUB_ENV, "1");
    std::env::set_var("CLEANROOM_LLAMA", &exe);

    // The locator resolves the binary from CLEANROOM_LLAMA (search order step 1).
    let sidecar = LlamaSidecar::locate().expect("locate via CLEANROOM_LLAMA");
    assert_eq!(sidecar.binary(), exe.as_path());

    let input = episode();
    let opts = GenerateOptions {
        model: Some(model.clone()),
        // Small window on purpose: forces the map stage, so the child is spawned repeatedly
        // and the multi-call path is exercised for real.
        ctx_tokens: 4_096,
        max_output_tokens: 512,
        ..Default::default()
    };

    let notes = generate(&input, &opts).expect("generate through the sidecar");
    let _ = std::fs::remove_file(&model);

    // The chapters the stub emitted were mangled on purpose; they come back clean.
    let starts: Vec<f64> = notes.chapters.iter().map(|c| c.start).collect();
    assert_eq!(
        starts,
        [0.0, 600.0, 1200.0],
        "chapters must be snapped, sorted and clamped, got {starts:?}"
    );
    assert!(notes.chapters.iter().all(|c| !c.title.trim().is_empty()));
    assert!(notes.summary.contains("microphones"));
    assert_eq!(notes.bullets.len(), 3);
    assert_eq!(notes.keywords.len(), 5);

    let report = rubric::score(&notes, &RubricContext::from_input(&input));
    assert!(report.usable, "structurally unusable: {report:#?}");

    // And the prompt file the child was given is gone.
    assert!(
        !leftover_prompt_files(),
        "the sidecar leaked its temp prompt file"
    );
}

/// A 40-minute, 200-segment episode — long enough to need several chunks at a 4k context.
fn episode() -> TranscriptInput {
    TranscriptInput {
        language: "en".into(),
        segments: (0..200)
            .map(|i| TranscriptSegment {
                text: format!(
                    "part {i}: the hosts keep talking about microphones, room acoustics and \
                     editing software for another dozen seconds or so"
                ),
                start: i as f64 * 12.0,
                end: i as f64 * 12.0 + 11.0,
            })
            .collect(),
    }
}

/// An empty file standing in for a gguf: nothing on our side of the boundary reads it (that
/// is llama.cpp's job), we only require that it exists.
fn fake_model() -> PathBuf {
    let path = std::env::temp_dir().join(format!("anvil-llm-stub-{}.gguf", std::process::id()));
    std::fs::write(&path, b"not really a gguf").expect("write fake model");
    path
}

fn leftover_prompt_files() -> bool {
    let prefix = format!("anvil-llm-prompt-{}-", std::process::id());
    std::fs::read_dir(std::env::temp_dir())
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(prefix.as_str()))
        })
        .unwrap_or(false)
}

// --- the stub model ---------------------------------------------------------------------

/// Behaves like `llama-cli`: reads the prompt from `-f`, writes a completion to stdout.
fn stub_llama_cli() {
    let args: Vec<String> = std::env::args().collect();

    let model = flag(&args, "-m").expect("llama-cli must be given -m <model>");
    assert!(
        Path::new(&model).is_file(),
        "the sidecar passed a model path that does not exist: {model}"
    );
    let prompt_file = flag(&args, "-f").expect("llama-cli must be given -f <prompt file>");
    for expected in ["-c", "-n", "--temp", "--top-p", "-s"] {
        assert!(
            args.iter().any(|a| a == expected),
            "missing {expected} in {args:?}"
        );
    }
    // We render ChatML ourselves, so llama.cpp must not apply its own chat template, and it
    // must not echo the prompt back at us.
    for expected in ["-no-cnv", "--no-display-prompt"] {
        assert!(
            args.iter().any(|a| a == expected),
            "missing {expected} in {args:?}"
        );
    }

    let prompt = std::fs::read_to_string(&prompt_file).expect("prompt file is readable");
    assert!(
        prompt.starts_with("<|im_start|>system\n") && prompt.ends_with("<|im_start|>assistant\n"),
        "the prompt is not ChatML: {:?}…",
        prompt.chars().take(60).collect::<String>()
    );

    let json = if prompt.contains("CHAPTER CANDIDATES") {
        // Deliberately awful timestamps: mid-segment, a clock string, out of order, one past
        // the end of the episode, and a duplicate.
        r#"{
          "summary": "The hosts spend the episode on microphones, then on the room, then on how they edit it all together afterwards, with a long detour into why the cheap microphone in the drawer is still fine for a guest who only visits once.",
          "bullets": ["A dynamic microphone forgives an untreated room.",
                      "Treat the first reflection point before buying more gear.",
                      "Cut filler words from the transcript, not the waveform."],
          "chapters": [
            {"title": "Editing", "start": 1203.0},
            {"title": "Cold open", "start": 4.0},
            {"title": "Chapter 2: The room", "start": "10:01"},
            {"title": "Room again", "start": 605.0},
            {"title": "Sponsor read", "start": 99999.0}
          ],
          "titles": ["The room always wins", "Microphones, rooms and regrets"],
          "keywords": ["microphones", "acoustics", "editing", "podcasting", "reverb"]
        }"#
    } else {
        r#"{"summary": "This part is about microphones and the room.",
            "bullets": ["They compare a dynamic and a condenser."],
            "topics": ["microphones", "acoustics"]}"#
    };
    // Real models wrap JSON in prose and fences; the parser must cope, so the stub does too.
    println!("Sure! Here are the show notes:\n\n```json\n{json}\n```\n\n[end of text]");
    eprintln!("llama_perf_context_print: stub");
}

/// The value following `flag` in argv.
fn flag(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}
