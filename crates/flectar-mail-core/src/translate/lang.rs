//! Very small language detector, only good enough to skip translating mail
//! that is already in the target language. Script ratios decide CJK, Kana,
//! Hangul, Cyrillic, Arabic, Greek, Hebrew and Thai; Latin-script text is told
//! apart by stop-word hits for a handful of common languages.

/// ISO 639-1 code of the dominant language, or `None` when unsure.
pub fn detect(text: &str) -> Option<&'static str> {
    let mut han = 0usize;
    let mut kana = 0usize;
    let mut hangul = 0usize;
    let mut cyr = 0usize;
    let mut arabic = 0usize;
    let mut greek = 0usize;
    let mut hebrew = 0usize;
    let mut thai = 0usize;
    let mut latin = 0usize;
    for c in text.chars() {
        match c as u32 {
            0x4E00..=0x9FFF | 0x3400..=0x4DBF => han += 1,
            0x3040..=0x30FF => kana += 1,
            0xAC00..=0xD7AF | 0x1100..=0x11FF => hangul += 1,
            0x0400..=0x04FF => cyr += 1,
            0x0600..=0x06FF => arabic += 1,
            0x0370..=0x03FF => greek += 1,
            0x0590..=0x05FF => hebrew += 1,
            0x0E00..=0x0E7F => thai += 1,
            _ if c.is_alphabetic() => latin += 1,
            _ => {}
        }
    }
    let total = han + kana + hangul + cyr + arabic + greek + hebrew + thai + latin;
    if total < 8 {
        return None;
    }
    let share = |n: usize| n as f64 / total as f64;
    // Japanese mixes kanji and kana; any real kana share means Japanese.
    if share(kana) > 0.1 && share(han + kana) > 0.4 {
        return Some("ja");
    }
    for (n, code) in [
        (han, "zh"),
        (hangul, "ko"),
        (cyr, "ru"),
        (arabic, "ar"),
        (greek, "el"),
        (hebrew, "he"),
        (thai, "th"),
    ] {
        if share(n) > 0.4 {
            return Some(code);
        }
    }
    if share(latin) < 0.6 {
        return None;
    }
    latin_language(text)
}

const STOPWORDS: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            "the", "and", "is", "to", "of", "you", "for", "that", "with", "this", "are", "your",
        ],
    ),
    (
        "fr",
        &[
            "le", "la", "les", "et", "est", "des", "une", "pour", "vous", "que", "dans", "pas",
        ],
    ),
    (
        "de",
        &[
            "der", "die", "das", "und", "ist", "nicht", "ich", "sie", "mit", "für", "ein", "zu",
        ],
    ),
    (
        "es",
        &[
            "el", "los", "las", "y", "es", "una", "para", "que", "por", "con", "del", "su",
        ],
    ),
    (
        "it",
        &[
            "il", "di", "che", "è", "per", "una", "non", "sono", "gli", "della", "con", "del",
        ],
    ),
    (
        "pt",
        &[
            "o", "os", "não", "uma", "para", "com", "que", "do", "da", "em", "você", "é",
        ],
    ),
    (
        "nl",
        &[
            "de", "het", "een", "en", "van", "niet", "ik", "je", "dat", "voor", "met", "zijn",
        ],
    ),
];

fn latin_language(text: &str) -> Option<&'static str> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .take(400)
        .map(|w| w.to_lowercase())
        .collect();
    if words.len() < 4 {
        return None;
    }
    let mut best: Option<(&'static str, usize)> = None;
    let mut second = 0usize;
    for (code, list) in STOPWORDS {
        let hits = words.iter().filter(|w| list.contains(&w.as_str())).count();
        match best {
            Some((_, b)) if hits <= b => second = second.max(hits),
            _ => {
                second = best.map_or(0, |(_, b)| b);
                best = Some((code, hits));
            }
        }
    }
    let (code, hits) = best?;
    // Need a clear winner covering a reasonable share of the words.
    (hits * 10 >= words.len() && hits > second * 3 / 2).then_some(code)
}

/// Map a target-language name or code ("Chinese (Simplified)", "zh-CN",
/// "日本語", "English") to the code [`detect`] returns.
pub fn target_code(target: &str) -> Option<&'static str> {
    let t = target.trim().to_lowercase();
    let table: &[(&str, &[&str])] = &[
        (
            "zh",
            &[
                "zh", "chinese", "中文", "简体", "繁體", "繁体", "汉语", "mandarin",
            ],
        ),
        ("ja", &["ja", "japanese", "日本"]),
        ("ko", &["ko", "korean", "한국"]),
        ("ru", &["ru", "russian", "рус"]),
        ("ar", &["ar", "arabic", "عرب"]),
        ("el", &["el", "greek"]),
        ("he", &["he", "hebrew"]),
        ("th", &["th", "thai"]),
        ("en", &["en", "english", "英"]),
        ("fr", &["fr", "french", "français", "法"]),
        ("de", &["de", "german", "deutsch", "德"]),
        ("es", &["es", "spanish", "español", "西班牙"]),
        ("it", &["it", "italian", "italiano"]),
        ("pt", &["pt", "portuguese", "português"]),
        ("nl", &["nl", "dutch", "nederlands"]),
    ];
    let head = t.split(['-', '_', ' ', '(']).next().unwrap_or("");
    for (code, names) in table {
        if names
            .iter()
            .any(|n| head == *n || ((n.chars().count() > 2 || !n.is_ascii()) && t.contains(n)))
        {
            return Some(code);
        }
    }
    None
}

/// Whether `text` is already written in `target`.
pub fn already_in(text: &str, target: &str) -> bool {
    matches!((detect(text), target_code(target)), (Some(a), Some(b)) if a == b)
}
