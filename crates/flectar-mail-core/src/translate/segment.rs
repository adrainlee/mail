//! Structured segmentation of (sanitized) HTML or plain text into blocks.
//!
//! The skeleton keeps every tag and untranslatable region verbatim and holds
//! `{{B<n>}}` markers where translated blocks go. Inline markup inside a
//! block is replaced with placeholders (`⟦t1⟧…⟦/t1⟧`, `⟦c2⟧`) so the model sees
//! only text; [`super::reassemble`] restores them and rejects blocks whose
//! placeholders were damaged.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Para,
    Heading(u8),
    ListItem,
    Cell,
    Quote,
    Code,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub idx: u32,
    pub kind: BlockKind,
    /// Text sent to the model (placeholders instead of markup).
    pub source: String,
    pub translatable: bool,
    /// Placeholder → original markup.
    pub placeholders: HashMap<String, String>,
    /// Original inline HTML (fallback when a translation is rejected).
    pub original: String,
}

#[derive(Debug, Clone, Default)]
pub struct Segmented {
    pub blocks: Vec<Block>,
    pub skeleton: String,
    /// True when the skeleton is plain text (no HTML escaping on output).
    pub plain: bool,
}

#[derive(Debug, Clone)]
pub struct SegmentOpts {
    /// Translate `blockquote` content (quoted history). Off by default.
    pub translate_quotes: bool,
}

impl Default for SegmentOpts {
    fn default() -> Self {
        Self {
            translate_quotes: false,
        }
    }
}

const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "td",
    "th",
    "tr",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "ul",
    "ol",
    "blockquote",
    "section",
    "article",
    "header",
    "footer",
    "br",
    "hr",
    "body",
    "html",
    "head",
    "center",
    "dl",
    "dt",
    "dd",
    "caption",
];
/// Content copied verbatim, never translated.
const RAW_TAGS: &[&str] = &["pre", "script", "style", "code", "title", "textarea"];

fn tag_name(tag: &str) -> (String, bool) {
    let inner = tag.trim_start_matches('<');
    let closing = inner.starts_with('/');
    let name: String = inner
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    (name, closing)
}

fn kind_for(stack: &[String]) -> BlockKind {
    for name in stack.iter().rev() {
        match name.as_str() {
            "li" => return BlockKind::ListItem,
            "td" | "th" => return BlockKind::Cell,
            "blockquote" => return BlockKind::Quote,
            n if n.len() == 2 && n.starts_with('h') => {
                if let Some(d) = n[1..].parse::<u8>().ok().filter(|d| (1..=6).contains(d)) {
                    return BlockKind::Heading(d);
                }
            }
            _ => {}
        }
    }
    BlockKind::Para
}

struct Builder {
    seg: Segmented,
    buf: String,
    buf_has_text: bool,
    stack: Vec<String>,
    opts: SegmentOpts,
}

/// Protect URLs and e-mail addresses in plain text with `⟦cN⟧`.
fn protect_tokens(text: &str, next: &mut u32, map: &mut HashMap<String, String>) -> String {
    let mut out = String::new();
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let core = word.trim_end_matches(['.', ',', ';', ':', ')', '!', '?']);
        let tail = &word[core.len()..];
        let is_url =
            core.starts_with("http://") || core.starts_with("https://") || core.starts_with("www.");
        let is_mail = core.contains('@') && core.contains('.') && !core.contains('⟦');
        if !core.is_empty() && (is_url || is_mail) {
            *next += 1;
            let key = format!("⟦c{next}⟧");
            map.insert(key.clone(), core.to_string());
            out.push_str(&key);
            out.push_str(tail);
        } else {
            out.push_str(word);
        }
    }
    out
}

/// Convert inline HTML into placeholder text.
fn inline_to_source(html: &str) -> (String, HashMap<String, String>) {
    let mut map = HashMap::new();
    let mut out = String::new();
    let mut n = 0u32;
    let mut open: Vec<(String, u32)> = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(lt) = rest.find('<') {
            let text = &rest[..lt];
            out.push_str(&protect_tokens(&decode_entities(text), &mut n, &mut map));
            let Some(gt) = rest[lt..].find('>') else {
                out.push_str(&decode_entities(&rest[lt..]));
                break;
            };
            let tag = &rest[lt..lt + gt + 1];
            let (name, closing) = tag_name(tag);
            if !closing && name == "code" {
                let after = &rest[lt + gt + 1..];
                let end = after
                    .to_ascii_lowercase()
                    .find("</code")
                    .map(|e| {
                        e + after[e..]
                            .find('>')
                            .map(|g| g + 1)
                            .unwrap_or(after.len() - e)
                    })
                    .unwrap_or(after.len());
                n += 1;
                let key = format!("⟦c{n}⟧");
                map.insert(key.clone(), format!("{tag}{}", &after[..end]));
                out.push_str(&key);
                rest = &after[end..];
                continue;
            }
            if closing {
                if let Some(pos) = open.iter().rposition(|(o, _)| *o == name) {
                    let (_, id) = open.remove(pos);
                    let key = format!("⟦/t{id}⟧");
                    map.insert(key.clone(), tag.to_string());
                    out.push_str(&key);
                } else {
                    n += 1;
                    let key = format!("⟦c{n}⟧");
                    map.insert(key.clone(), tag.to_string());
                    out.push_str(&key);
                }
            } else if tag.ends_with("/>") || matches!(name.as_str(), "img" | "wbr") {
                n += 1;
                let key = format!("⟦c{n}⟧");
                map.insert(key.clone(), tag.to_string());
                out.push_str(&key);
            } else {
                n += 1;
                let key = format!("⟦t{n}⟧");
                map.insert(key.clone(), tag.to_string());
                out.push_str(&key);
                open.push((name, n));
            }
            rest = &rest[lt + gt + 1..];
        } else {
            out.push_str(&protect_tokens(&decode_entities(rest), &mut n, &mut map));
            break;
        }
    }
    (out, map)
}

pub(crate) fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

impl Builder {
    fn flush(&mut self) {
        let inline = std::mem::take(&mut self.buf);
        if !self.buf_has_text {
            self.seg.skeleton.push_str(&inline);
            return;
        }
        self.buf_has_text = false;
        let lead = inline.len() - inline.trim_start().len();
        let trail = inline.trim_end().len();
        let core = &inline[lead..trail];
        let kind = kind_for(&self.stack);
        let translatable = kind != BlockKind::Quote || self.opts.translate_quotes;
        let (source, placeholders) = inline_to_source(core);
        let idx = self.seg.blocks.len() as u32;
        self.seg.skeleton.push_str(&inline[..lead]);
        self.seg.skeleton.push_str(&format!("{{{{B{idx}}}}}"));
        self.seg.skeleton.push_str(&inline[trail..]);
        self.seg.blocks.push(Block {
            idx,
            kind,
            source,
            translatable,
            placeholders,
            original: core.to_string(),
        });
    }
}

pub fn segment_html(html: &str, opts: &SegmentOpts) -> Segmented {
    let mut b = Builder {
        seg: Segmented::default(),
        buf: String::new(),
        buf_has_text: false,
        stack: Vec::new(),
        opts: opts.clone(),
    };
    let mut rest = html;
    while !rest.is_empty() {
        // Comments are kept verbatim and never translated.
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            b.flush();
            b.seg.skeleton.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let Some(lt) = rest.find('<') else {
            if !rest.trim().is_empty() {
                b.buf_has_text = true;
            }
            b.buf.push_str(rest);
            break;
        };
        let text = &rest[..lt];
        if !decode_entities(text).trim().is_empty() {
            b.buf_has_text = true;
        }
        b.buf.push_str(text);
        let Some(gt) = rest[lt..].find('>') else {
            b.buf.push_str(&rest[lt..]);
            break;
        };
        let tag = &rest[lt..lt + gt + 1];
        let (name, closing) = tag_name(tag);
        rest = &rest[lt + gt + 1..];
        if !closing && RAW_TAGS.contains(&name.as_str()) {
            // Inline <code> inside a sentence stays in the block as a
            // protected placeholder; block-level raw regions go verbatim.
            let close = format!("</{name}");
            let lower = rest.to_ascii_lowercase();
            let end = lower
                .find(&close)
                .map(|e| e + rest[e..].find('>').map(|g| g + 1).unwrap_or(rest.len() - e))
                .unwrap_or(rest.len());
            let raw = format!("{tag}{}", &rest[..end]);
            rest = &rest[end..];
            if name == "code" && b.buf_has_text {
                // Inline code inside a sentence: stays in the block and is
                // protected as one placeholder by `inline_to_source`.
                b.buf.push_str(&raw);
                continue;
            }
            b.flush();
            let idx = b.seg.blocks.len() as u32;
            b.seg.skeleton.push_str(&format!("{{{{B{idx}}}}}"));
            b.seg.blocks.push(Block {
                idx,
                kind: if name == "pre" || name == "code" {
                    BlockKind::Code
                } else {
                    BlockKind::Raw
                },
                source: String::new(),
                translatable: false,
                placeholders: HashMap::new(),
                original: raw,
            });
            continue;
        }
        if BLOCK_TAGS.contains(&name.as_str()) {
            b.flush();
            b.seg.skeleton.push_str(tag);
            if closing {
                if let Some(pos) = b.stack.iter().rposition(|s| *s == name) {
                    b.stack.truncate(pos);
                }
            } else if !matches!(name.as_str(), "br" | "hr") && !tag.ends_with("/>") {
                b.stack.push(name);
            }
        } else {
            b.buf.push_str(tag);
        }
    }
    b.flush();
    b.seg
}

/// Plain text: paragraphs separated by blank lines; `>` quote lines and a
/// `-- ` signature tail are kept untranslated.
pub fn segment_text(text: &str) -> Segmented {
    let mut seg = Segmented {
        plain: true,
        ..Default::default()
    };
    let mut in_sig = false;
    let parts: Vec<&str> = text.split("\n\n").collect();
    for (i, para) in parts.iter().enumerate() {
        if i > 0 {
            seg.skeleton.push_str("\n\n");
        }
        if para.lines().any(|l| l == "-- ") {
            in_sig = true;
        }
        if para.trim().is_empty() {
            seg.skeleton.push_str(para);
            continue;
        }
        let quote = para.lines().all(|l| l.trim_start().starts_with('>'));
        let idx = seg.blocks.len() as u32;
        let mut map = HashMap::new();
        let mut n = 0;
        let source = protect_tokens(para, &mut n, &mut map);
        seg.skeleton.push_str(&format!("{{{{B{idx}}}}}"));
        seg.blocks.push(Block {
            idx,
            kind: if quote {
                BlockKind::Quote
            } else {
                BlockKind::Para
            },
            source,
            translatable: !quote && !in_sig,
            placeholders: map,
            original: para.to_string(),
        });
    }
    seg
}
