//! Russian word stems for the text search. FTS5's `porter` stems English
//! only, so «краш», «краша» and «крашит» were three different words: a record
//! that said one was not found by the others. This is the Snowball Russian
//! stemmer (snowballstem.org/algorithms/russian), applied by lore itself to
//! the Cyrillic words of what it indexes and of what it searches for. The
//! index keeps each word as written and adds its stem beside it; a query asks
//! for the word or its stem. A Latin word is left to `porter`.
use std::collections::HashSet;

/// What the index's rows were stemmed with; another value re-reads them.
pub const VERSION: &str = "ru-1";

/// A stem shorter than this is not added: «эт» of «это», «им» of «имя» would
/// match every pronoun.
const SHORTEST: usize = 3;

const VOWELS: &[char] = &['а', 'е', 'и', 'о', 'у', 'ы', 'э', 'ю', 'я'];

const GERUND_1: &[&str] = &["в", "вши", "вшись"];
const GERUND_2: &[&str] = &["ив", "ивши", "ившись", "ыв", "ывши", "ывшись"];
const ADJECTIVE: &[&str] = &[
    "ее", "ие", "ые", "ое", "ими", "ыми", "ей", "ий", "ый", "ой", "ем", "им", "ым", "ом", "его", "ого", "ему", "ому", "их", "ых", "ую", "юю", "ая", "яя", "ою", "ею",
];
const PARTICIPLE_1: &[&str] = &["ем", "нн", "вш", "ющ", "щ"];
const PARTICIPLE_2: &[&str] = &["ивш", "ывш", "ующ"];
const REFLEXIVE: &[&str] = &["ся", "сь"];
const VERB_1: &[&str] = &["ла", "на", "ете", "йте", "ли", "й", "л", "ем", "н", "ло", "но", "ет", "ют", "ны", "ть", "ешь", "нно"];
const VERB_2: &[&str] = &[
    "ила", "ыла", "ена", "ейте", "уйте", "ите", "или", "ыли", "ей", "уй", "ил", "ыл", "им", "ым", "ен", "ило", "ыло", "ено", "ят", "ует", "уют", "ит", "ыт", "ены", "ить", "ыть",
    "ишь", "ую", "ю",
];
const NOUN: &[&str] = &[
    "а", "ев", "ов", "ие", "ье", "е", "иями", "ями", "ами", "еи", "ии", "и", "ией", "ей", "ой", "ий", "й", "иям", "ям", "ием", "ем", "ам", "ом", "о", "у", "ах", "иях", "ях", "ы", "ь",
    "ию", "ью", "ю", "ия", "ья", "я",
];
const DERIVATIONAL: &[&str] = &["ост", "ость"];
const SUPERLATIVE: &[&str] = &["ейш", "ейше"];

fn is_vowel(ch: char) -> bool {
    VOWELS.contains(&ch)
}

/// Whether `word` (lower case) is a Russian word: Cyrillic letters only.
pub fn is_russian(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|ch| ('а'..='я').contains(&ch) || ch == 'ё')
}

/// Where the region after the first vowel starts, and R1, R2 (Snowball).
fn regions(word: &[char]) -> (usize, usize, usize) {
    let after = |from: usize| -> usize {
        let mut at = from;
        while at < word.len() && !is_vowel(word[at]) {
            at += 1;
        }
        while at < word.len() && is_vowel(word[at]) {
            at += 1;
        }
        // the first non-vowel after a vowel, and the region begins after it
        (at + 1).min(word.len())
    };
    let rv = word.iter().position(|&ch| is_vowel(ch)).map_or(word.len(), |at| at + 1);
    let r1 = after(0);
    let r2 = after(r1);
    (rv, r1, r2)
}

fn ends_with(word: &[char], from: usize, suffix: &str) -> bool {
    let suffix: Vec<char> = suffix.chars().collect();
    word.len() >= suffix.len() && word.len() - suffix.len() >= from && word[word.len() - suffix.len()..] == suffix[..]
}

/// The longest suffix of `lists` that `word` ends with inside the region from
/// `from`: its length and the list it is in.
fn longest(word: &[char], from: usize, lists: &[&[&str]]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (which, list) in lists.iter().enumerate() {
        for suffix in *list {
            let len = suffix.chars().count();
            if ends_with(word, from, suffix) && best.is_none_or(|(have, _)| len > have) {
                best = Some((len, which));
            }
        }
    }
    best
}

/// Removes the longest suffix of `first` or `second`; one of `first` only
/// after а or я, which stays.
fn remove(word: &mut Vec<char>, from: usize, first: &[&str], second: &[&str]) -> bool {
    let Some((len, which)) = longest(word, from, &[first, second]) else { return false };
    let start = word.len() - len;
    if which == 0 && !second.iter().any(|suffix| suffix.chars().count() == len && ends_with(word, from, suffix)) {
        if start == 0 || start - 1 < from || !matches!(word[start - 1], 'а' | 'я') {
            return false;
        }
    }
    word.truncate(start);
    true
}

fn adjectival(word: &mut Vec<char>, from: usize) -> bool {
    if !remove(word, from, &[], ADJECTIVE) {
        return false;
    }
    remove(word, from, PARTICIPLE_1, PARTICIPLE_2);
    true
}

/// The stem of a lower-case Russian word (ё read as е).
pub fn russian(word: &str) -> String {
    let mut w: Vec<char> = word.chars().map(|ch| if ch == 'ё' { 'е' } else { ch }).collect();
    let (rv, _r1, r2) = regions(&w);
    if rv >= w.len() {
        return w.into_iter().collect();
    }
    // Step 1
    if !remove(&mut w, rv, GERUND_1, GERUND_2) {
        remove(&mut w, rv, &[], REFLEXIVE);
        if !adjectival(&mut w, rv) && !remove(&mut w, rv, VERB_1, VERB_2) {
            remove(&mut w, rv, &[], NOUN);
        }
    }
    // Step 2
    if ends_with(&w, rv, "и") {
        w.pop();
    }
    // Step 3
    if let Some((len, _)) = longest(&w, r2.max(rv), &[DERIVATIONAL]) {
        w.truncate(w.len() - len);
    }
    // Step 4
    if ends_with(&w, rv, "нн") {
        w.pop();
    } else if remove(&mut w, rv, &[], SUPERLATIVE) {
        if ends_with(&w, rv, "нн") {
            w.pop();
        }
    } else if ends_with(&w, rv, "ь") {
        w.pop();
    }
    w.into_iter().collect()
}

/// The stem the search adds for `word` (lower case): a Russian word's, when
/// it differs from the word and is long enough to mean something.
pub fn extra_of(word: &str) -> Option<String> {
    if !is_russian(word) {
        return None;
    }
    let stem = russian(word);
    (stem != word && stem.chars().count() >= SHORTEST).then_some(stem)
}

/// The stems of `text`'s Russian words, each once, space-separated: what the
/// index adds to a column so that another form of a word finds it.
pub fn extra(text: &str) -> String {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for found in crate::index::word_re().find_iter(text) {
        if let Some(stem) = extra_of(&found.as_str().to_lowercase()) {
            if seen.insert(stem.clone()) {
                out.push(stem);
            }
        }
    }
    out.join(" ")
}

/// `text` with its Russian stems after it, for an FTS column.
pub fn with_stems(text: &str) -> String {
    let extra = extra(text);
    if extra.is_empty() {
        text.to_owned()
    } else {
        format!("{text}\n{extra}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_of_a_word_share_a_stem() {
        for (forms, stem) in [
            (&["краш", "краша", "крашит", "крашится", "крашиться"][..], "краш"),
            (&["вложенное", "вложенным", "вложенного", "вложенная"][..], "вложен"),
            (&["таблица", "таблицы", "таблицей", "таблицах"][..], "таблиц"),
            (&["импорт", "импорта", "импорте", "импортом"][..], "импорт"),
            (&["редактор", "редактора", "редакторе"][..], "редактор"),
        ] {
            for form in forms {
                assert_eq!(russian(form), stem, "{form}");
            }
        }
    }

    #[test]
    fn snowball_reference_words() {
        // from the Snowball Russian vocabulary and its output
        for (word, stem) in [
            ("вавиловка", "вавиловк"),
            ("вагнера", "вагнер"),
            ("важнейшие", "важн"),
            ("важности", "важност"),
            ("ёлка", "елк"),
            ("бегавшими", "бега"),
            ("прекрасного", "прекрасн"),
            ("рассказывала", "рассказыва"),
        ] {
            assert_eq!(russian(word), stem, "{word}");
        }
    }

    #[test]
    fn only_russian_words_get_a_stem() {
        assert_eq!(extra_of("crashes"), None);
        assert_eq!(extra_of("data_table"), None);
        assert_eq!(extra_of("это"), None, "a stem under three letters is not added");
        assert_eq!(extra("DataTable крашит редактор"), "краш");
        assert_eq!(with_stems("plain text"), "plain text");
    }
}
