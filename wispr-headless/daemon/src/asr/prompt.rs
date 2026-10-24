//! What Granite Speech is asked: "transcribe the speech to text.", followed
//! by " Keywords: a, b, c" when the user keeps a list of terms it should
//! spell their way (IBM's keyword biasing). llama-server puts the audio after
//! this text, so for every piece of a dictation the prompt is the same
//! prefix and the server processes the keywords only once (its prompt
//! cache); see `Engine::prime` in mod.rs. Each piece still attends to them:
//! with 1500 terms (3.5k tokens) the pieces took 3x as long on a CPU.

pub const INSTRUCTION: &str = "transcribe the speech to text.";
/// Granite Speech 5.0's context, in tokens.
pub const MODEL_CONTEXT: u32 = 16_384;
/// Room each request needs besides the prompt: up to 30 s of audio (about
/// 10 tokens a second), the chat template, and the text (`max_tokens`).
pub const PIECE_TOKENS: u32 = 768;

/// The longest text the model may answer with for `samples` of audio:
/// generous for fast speech, short enough to stop a model that repeats itself.
pub fn max_tokens(samples: usize) -> u32 {
    (samples / 2_000) as u32 + 32
}

/// The terms in a keyword file: one per line, or several separated by
/// commas. Blank lines and lines starting with `#` are skipped, and a term
/// listed again is dropped.
pub fn parse_keywords(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split(','))
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect()
}

pub fn prompt(keywords: &[String]) -> String {
    if keywords.is_empty() {
        INSTRUCTION.to_string()
    } else {
        format!("{INSTRUCTION} Keywords: {}", keywords.join(", "))
    }
}

/// Tokens `text` probably takes, erring high: English words take 4 bytes a
/// token with Granite's tokenizer, names and numbers less. The server counts
/// exactly once it runs (see `fit` in mod.rs).
pub fn estimate_tokens(text: &str) -> u32 {
    (text.len() / 3) as u32 + 16
}

/// The context to start the server with for `prompt`, in steps of 256.
pub fn context_for(prompt: &str) -> u32 {
    (estimate_tokens(prompt) + PIECE_TOKENS)
        .div_ceil(256)
        .saturating_mul(256)
        .min(MODEL_CONTEXT)
}

/// How many of `keywords` to keep when the prompt with all of them takes
/// `tokens` and only `budget` fit: proportionally fewer, and some more for
/// the guess.
pub fn keep_for(keywords: usize, tokens: u32, budget: u32) -> usize {
    if tokens <= budget {
        return keywords;
    }
    (keywords as u64 * u64::from(budget) * 9 / 10 / u64::from(tokens.max(1))) as usize
}

/// The keywords that fit the model's context by the estimate (the rest is
/// dropped from the end).
pub fn fit_estimate(keywords: &[String]) -> &[String] {
    let budget = MODEL_CONTEXT - PIECE_TOKENS;
    let mut n = keywords.len();
    loop {
        let tokens = estimate_tokens(&prompt(&keywords[..n]));
        if tokens <= budget || n == 0 {
            return &keywords[..n];
        }
        n = keep_for(n, tokens, budget).min(n - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("term{i}")).collect()
    }

    #[test]
    fn without_keywords_the_prompt_is_the_instruction() {
        assert_eq!(prompt(&[]), "transcribe the speech to text.");
    }

    #[test]
    fn keywords_follow_the_instruction() {
        let k = vec!["PipeWire".to_string(), "nixpkgs".into(), "Granite".into()];
        assert_eq!(
            prompt(&k),
            "transcribe the speech to text. Keywords: PipeWire, nixpkgs, Granite"
        );
    }

    #[test]
    fn keyword_files_take_lines_and_commas() {
        let text = "# my terms\nPipeWire\n\n  nixpkgs ,Granite Speech\nPipeWire\n llama.cpp\t\n";
        assert_eq!(
            parse_keywords(text),
            ["PipeWire", "nixpkgs", "Granite Speech", "llama.cpp"]
        );
    }

    #[test]
    fn two_thousand_keywords_fit_with_room_for_a_piece() {
        let k = words(2_000);
        assert_eq!(fit_estimate(&k).len(), 2_000);
        let p = prompt(&k);
        let ctx = context_for(&p);
        assert!(ctx >= estimate_tokens(&p) + PIECE_TOKENS);
        assert!(ctx <= MODEL_CONTEXT);
        assert_eq!(ctx % 256, 0);
        assert_eq!(context_for(INSTRUCTION), 1_024);
    }

    #[test]
    fn too_many_keywords_are_cut_to_the_context() {
        let k = words(20_000);
        let kept = fit_estimate(&k);
        assert!(kept.len() > 1_000 && kept.len() < 20_000, "{}", kept.len());
        assert!(estimate_tokens(&prompt(kept)) <= MODEL_CONTEXT - PIECE_TOKENS);
        assert_eq!(kept[0], "term0", "the first ones stay");
    }

    #[test]
    fn keeping_fewer_keywords_when_counted_too_many() {
        assert_eq!(keep_for(100, 500, 1_000), 100);
        assert_eq!(keep_for(100, 2_000, 1_000), 45);
    }

    #[test]
    fn answers_are_capped_by_the_audio_length() {
        assert_eq!(max_tokens(16_000 * 30), 272);
        assert_eq!(max_tokens(0), 32);
    }
}
