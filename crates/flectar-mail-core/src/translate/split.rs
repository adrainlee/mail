//! Split a block that exceeds the chunk budget into sentence-sized pieces.
//! Cuts happen only between sentences and never inside a paired inline-tag
//! placeholder (`⟦tN⟧…⟦/tN⟧`), so each piece can be translated on its own and
//! the pieces joined back before placeholder validation.

use super::est_tokens;

/// Pieces whose concatenation is exactly `source`. One piece when it fits.
pub fn split_source(source: &str, max_tokens: usize) -> Vec<String> {
    let max_tokens = max_tokens.max(50);
    if est_tokens(source) <= max_tokens {
        return vec![source.to_string()];
    }
    // Candidate cut points: after sentence punctuation (+ following spaces)
    // at placeholder depth zero.
    let mut cuts = Vec::new();
    let mut depth = 0i32;
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        if c == '⟦' {
            let rest = &source[pos..];
            if rest[('⟦'.len_utf8())..].starts_with("/t") {
                depth -= 1;
            } else if rest[('⟦'.len_utf8())..].starts_with('t') {
                depth += 1;
            }
        }
        let ends_sentence = matches!(c, '.' | '!' | '?' | '。' | '！' | '？' | '；' | ';' | '\n');
        if ends_sentence && depth <= 0 {
            let mut j = i + 1;
            while j < chars.len() && chars[j].1.is_whitespace() {
                j += 1;
            }
            let at = chars.get(j).map_or(source.len(), |(p, _)| *p);
            // A cut only counts before more text (not trailing punctuation).
            if at < source.len() && (c != '.' || j > i + 1) {
                cuts.push(at);
            }
        }
        i += 1;
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut last_ok = None;
    for &cut in &cuts {
        if est_tokens(&source[start..cut]) > max_tokens
            && let Some(ok) = last_ok.take()
        {
            pieces.push(source[start..ok].to_string());
            start = ok;
        }
        last_ok = Some(cut);
    }
    if est_tokens(&source[start..]) > max_tokens
        && let Some(ok) = last_ok
        && ok > start
    {
        pieces.push(source[start..ok].to_string());
        start = ok;
    }
    pieces.push(source[start..].to_string());
    pieces
}
