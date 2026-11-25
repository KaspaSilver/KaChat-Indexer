//! Word filter (docs/WORD_FILTER.md): public-chat messages and KaPosts that contain a listed word
//! are not stored. The list is `kachat_word_filter`, edited in the panel and seeded once from
//! `word_filter_default.txt`.
//!
//! kachat-admin compiles this file too (`#[path]`), to find stored messages a new word catches,
//! so it uses std and serde_json only.
//!
//! An entry is one word, a word ending in `*` (any ending), or a phrase of several words. Text
//! and entries go through the same normalizing: lowercase, accents dropped, common letter swaps
//! undone (`0`→o `1`→i `3`→e `4`→a `5`→s `7`→t `@`→a `$`→s). Words match whole, so "ass" does
//! not match "class"; stretched letters ("fuuuck") match too.

use std::collections::HashMap;

/// Longer words are ids, addresses or encoded data, never a listed word.
const MAX_WORD_CHARS: usize = 40;

/// An entry as stored: trimmed, lowercase, single spaces, at most this many characters.
pub const MAX_ENTRY_CHARS: usize = 64;

/// The list, by kind of entry, each with the entry as written.
#[derive(Default)]
pub struct WordFilter {
    /// Single words, matched whole.
    exact: HashMap<String, String>,
    /// Single words ending in `*`, matched as the start of a word.
    prefixes: Vec<(String, String)>,
    /// Several words in a row; the flag = the last one ends in `*`.
    phrases: Vec<(Vec<String>, bool, String)>,
}

impl WordFilter {
    pub fn new<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) -> WordFilter {
        let mut filter = WordFilter::default();
        for entry in entries {
            let entry = entry.as_ref().trim();
            let (body, prefix) = match entry.strip_suffix('*') {
                Some(body) => (body, true),
                None => (entry, false),
            };
            let mut words: Vec<String> = words(body).collect();
            match (words.len(), prefix) {
                (0, _) => {}
                (1, false) => {
                    filter.exact.insert(words.remove(0), entry.to_string());
                }
                (1, true) => filter.prefixes.push((words.remove(0), entry.to_string())),
                _ => filter.phrases.push((words, prefix, entry.to_string())),
            }
        }
        filter
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefixes.is_empty() && self.phrases.is_empty()
    }

    /// Whether any of `texts` contains a listed word.
    pub fn blocks<S: AsRef<str>>(&self, texts: &[S]) -> bool {
        self.first_match(texts).is_some()
    }

    /// The first entry any of `texts` contains, as written in the list.
    pub fn first_match<S: AsRef<str>>(&self, texts: &[S]) -> Option<&str> {
        if self.is_empty() {
            return None;
        }
        texts.iter().find_map(|t| self.match_in(t.as_ref()))
    }

    fn match_in(&self, text: &str) -> Option<&str> {
        let words: Vec<[String; 3]> = words(text).map(|w| forms(&w)).collect();
        for form in words.iter().flatten() {
            if let Some(entry) = self.exact.get(form) {
                return Some(entry);
            }
            if let Some((_, entry)) = self.prefixes.iter().find(|(p, _)| form.starts_with(p.as_str())) {
                return Some(entry);
            }
        }
        self.phrases.iter().find_map(|(phrase, prefix, entry)| {
            words
                .windows(phrase.len())
                .any(|window| {
                    window.iter().zip(phrase).enumerate().all(|(i, (forms, want))| {
                        let open = *prefix && i == phrase.len() - 1;
                        forms.iter().any(|f| if open { f.starts_with(want.as_str()) } else { f == want })
                    })
                })
                .then_some(entry.as_str())
        })
    }
}

/// An entry as stored, or None for one that can't be one (empty, or too long).
pub fn clean_entry(raw: &str) -> Option<String> {
    let entry = raw.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let body = entry.strip_suffix('*').unwrap_or(&entry);
    (words(body).next().is_some() && entry.chars().count() <= MAX_ENTRY_CHARS).then_some(entry)
}

/// The text a person reads in a public-chat message. Replies and edits are JSON envelopes whose
/// text is in `text` (and `replyToPreview`, the quoted message); reactions and media carry no
/// text to check. Anything else is the text itself.
pub fn room_texts(content: &str) -> Vec<String> {
    let trimmed = content.trim();
    let Ok(serde_json::Value::Object(v)) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return vec![content.to_string()];
    };
    ["text", "content", "replyToPreview", "caption", "fileName"]
        .iter()
        .filter_map(|key| v.get(*key).and_then(|s| s.as_str()))
        .filter(|s| !s.starts_with("data:"))
        .map(str::to_string)
        .collect()
}

/// The normalized words of `text`, skipping ids, addresses and encoded data.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !(c.is_alphanumeric() || c == '@' || c == '$'))
        .filter(|w| !w.is_empty() && w.chars().count() <= MAX_WORD_CHARS)
        // Hex ids; and numbers, which letter swaps would turn into words ("455").
        .filter(|w| w.chars().any(|c| c.is_alphabetic()) && !(w.len() >= 16 && w.bytes().all(|b| b.is_ascii_hexdigit())))
        .map(|w| w.chars().flat_map(char::to_lowercase).map(fold).collect::<String>())
        .filter(|w| !w.is_empty())
}

/// A word as written, with stretched letters cut to two, and to one: "asss" → "ass",
/// "fuuuck" → "fuck".
fn forms(word: &str) -> [String; 3] {
    let squeeze = |most: usize| {
        let mut out = String::with_capacity(word.len());
        let mut last = None;
        let mut run = 0;
        for c in word.chars() {
            run = if Some(c) == last { run + 1 } else { 1 };
            last = Some(c);
            if run <= most {
                out.push(c);
            }
        }
        out
    };
    [word.to_string(), squeeze(2), squeeze(1)]
}

/// One lowercase character with its accent dropped and letter swaps undone.
fn fold(c: char) -> char {
    match c {
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' => 't',
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'ď' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
        'ğ' => 'g',
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => 'i',
        'ł' => 'l',
        'ñ' | 'ń' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
        'ř' => 'r',
        'ś' | 'š' | 'ş' => 's',
        'ť' | 'ţ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_filter() -> WordFilter {
        WordFilter::new(
            include_str!("word_filter_default.txt")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#')),
        )
    }

    #[test]
    fn catches_words_spellings_and_phrases() {
        let f = default_filter();
        for text in [
            "what the FUCK",
            "fucking hell",
            "sh1t happens",
            "you @ss",
            "a$$hole",
            "fuuuuck this",
            "shiiiit",
            "Fück you",
            "son of a bitch",
            "you should kill   yourself",
            "go f*ck... no, FUCKER",
            "porn-site.com",
        ] {
            assert!(f.blocks(&[text]), "{text}");
        }
    }

    #[test]
    fn leaves_ordinary_words_alone() {
        let f = default_filter();
        for text in [
            "class assignment",
            "I assume the assistant passed",
            "Scunthorpe",
            "cocktail at the cockpit",
            "Damn, that's crap",
            "crackers and cheese",
            "Essex and Sussex",
            "pass the analysis",
            "455 KAS sent",
            "shiitake mushrooms",
            "Muy buenas!! Qué gran comunidad",
            "Takže zatim 4.",
            "VIva China!!!",
        ] {
            assert!(!f.blocks(&[text]), "{text}");
        }
    }

    #[test]
    fn skips_ids_addresses_and_media() {
        let f = WordFilter::new(["badass*", "fuck*"]);
        // A hex id that letter swaps would read as "badass…".
        assert!(!f.blocks(&["bada55bada55bada55"]));
        let reply = r#"{"text":"nice","replyToPreview":"hello","type":"reply","replyToId":"bada55ffbada55ff","replyToSender":"kaspa:qfuckqqqq"}"#;
        assert!(!f.blocks(&room_texts(reply)));
        let rude = r#"{"text":"fuck off","type":"reply","replyToId":"00"}"#;
        assert_eq!(f.first_match(&room_texts(rude)), Some("fuck*"));
        let quoted = r#"{"text":"ok","replyToPreview":"badass move","type":"reply"}"#;
        assert!(f.blocks(&room_texts(quoted)));
        let photo = r#"{"type":"file","mimeType":"image/jpeg","content":"data:image/jpeg;base64,fuckAAAA"}"#;
        assert!(room_texts(photo).is_empty());
        assert!(room_texts(r#"{"emoji":"👍","type":"reaction","targetTxId":"ab","action":"add"}"#).is_empty());
        assert_eq!(room_texts("plain {not json"), vec!["plain {not json".to_string()]);
    }

    #[test]
    fn entries_are_cleaned() {
        assert_eq!(clean_entry("  Son   OF a  BITCH "), Some("son of a bitch".into()));
        assert_eq!(clean_entry("Fuck*"), Some("fuck*".into()));
        assert_eq!(clean_entry("  "), None);
        assert_eq!(clean_entry("*"), None);
        assert_eq!(clean_entry("!!!"), None);
        assert_eq!(clean_entry(&"a".repeat(65)), None);
        // An empty list blocks nothing.
        assert!(!WordFilter::new(Vec::<String>::new()).blocks(&["fuck"]));
    }
}
