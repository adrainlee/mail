//! Split a block that exceeds the chunk budget into pieces that fit.
//! Cuts prefer sentence ends; a single sentence that is still too long is cut
//! at clause punctuation, then at whitespace, and only as a last resort
//! between characters. No cut ever falls inside a placeholder token (`⟦…⟧`)
//! or between a paired inline-tag placeholder (`⟦tN⟧…⟦/tN⟧`), so each piece
//! can be translated on its own and the pieces joined back before
//! placeholder validation. Concatenating the pieces gives back `source`.

use super::est_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Sentence,
    Clause,
    Space,
    Char,
}

impl Tier {
    fn next(self) -> Option<Tier> {
        match self {
            Tier::Sentence => Some(Tier::Clause),
            Tier::Clause => Some(Tier::Space),
            Tier::Space => Some(Tier::Char),
            Tier::Char => None,
        }
    }
}

/// Pieces whose concatenation is exactly `source`. One piece when it fits.
pub fn split_source(source: &str, max_tokens: usize) -> Vec<String> {
    let max_tokens = max_tokens.max(50);
    let mut out = Vec::new();
    split_tier(source, max_tokens, Some(Tier::Sentence), &mut out);
    out
}

fn split_tier(s: &str, max: usize, tier: Option<Tier>, out: &mut Vec<String>) {
    let Some(t) = tier.filter(|_| est_tokens(s) > max) else {
        out.push(s.to_string());
        return;
    };
    let cuts = cut_points(s, t);
    if cuts.is_empty() {
        return split_tier(s, max, t.next(), out);
    }
    for piece in pack(s, &cuts, max) {
        split_tier(piece, max, t.next(), out);
    }
}

/// Greedy packing: take as many segments as fit, cut at the last that did.
fn pack<'a>(s: &'a str, cuts: &[usize], max: usize) -> Vec<&'a str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut last_ok: Option<usize> = None;
    for &cut in cuts {
        if est_tokens(&s[start..cut]) > max
            && let Some(ok) = last_ok.take()
        {
            pieces.push(&s[start..ok]);
            start = ok;
        }
        if cut > start {
            last_ok = Some(cut);
        }
    }
    if est_tokens(&s[start..]) > max
        && let Some(ok) = last_ok
        && ok > start
    {
        pieces.push(&s[start..ok]);
        start = ok;
    }
    pieces.push(&s[start..]);
    pieces
}

/// Byte offsets where a new piece may start, for one tier.
fn cut_points(source: &str, tier: Tier) -> Vec<usize> {
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut cuts = Vec::new();
    let mut depth = 0i32;
    let mut in_token = false;
    for i in 0..chars.len() {
        let (pos, c) = chars[i];
        if c == '⟦' {
            in_token = true;
            let rest = &source[pos + '⟦'.len_utf8()..];
            if rest.starts_with("/t") {
                depth -= 1;
            } else if rest.starts_with('t') {
                depth += 1;
            }
            continue;
        }
        if c == '⟧' {
            in_token = false;
            continue;
        }
        if in_token || depth > 0 {
            continue;
        }
        match tier {
            Tier::Sentence | Tier::Clause => {
                let ends = if tier == Tier::Sentence {
                    matches!(c, '.' | '!' | '?' | '。' | '！' | '？' | '；' | ';' | '\n')
                } else {
                    matches!(c, ',' | '，' | '、' | ':' | '：' | '—' | '–' | ')' | '）')
                };
                if !ends {
                    continue;
                }
                let mut j = i + 1;
                while j < chars.len() && chars[j].1.is_whitespace() {
                    j += 1;
                }
                // ASCII punctuation needs following space ("3.14", "1,000").
                let ascii = c.is_ascii() && c != '\n';
                if j < chars.len() && (!ascii || j > i + 1) {
                    cuts.push(chars[j].0);
                }
            }
            Tier::Space => {
                if c.is_whitespace() && i + 1 < chars.len() && !chars[i + 1].1.is_whitespace() {
                    cuts.push(chars[i + 1].0);
                }
            }
            Tier::Char => {
                if i + 1 < chars.len() {
                    cuts.push(chars[i + 1].0);
                }
            }
        }
    }
    cuts.dedup();
    cuts
}
