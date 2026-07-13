use std::collections::HashSet;

use bibtex_parser::parse_names;
use deunicode::deunicode;

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "by", "for", "from", "in", "into", "of", "on", "or", "the", "to",
    "with",
];

pub fn generate_citation_key<'a>(
    author: Option<&str>,
    date: Option<&str>,
    title: Option<&str>,
    existing: impl IntoIterator<Item = &'a str>,
) -> String {
    let author = author_component(author).unwrap_or_else(|| "anon".to_owned());
    let year = year_component(date).unwrap_or_else(|| "nd".to_owned());
    let title = title_component(title).unwrap_or_else(|| "item".to_owned());
    let base = format!("{author}{year}{title}");
    let existing = existing.into_iter().collect::<HashSet<_>>();

    if !existing.contains(base.as_str()) {
        return base;
    }

    for index in 0.. {
        let candidate = format!("{base}{}", alphabetic_suffix(index));
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("an unbounded suffix sequence always contains an unused key")
}

fn author_component(author: Option<&str>) -> Option<String> {
    let author = author?;
    let first = parse_names(author).into_iter().next()?;
    let family = if first.last.is_empty() {
        first.literal.as_deref().unwrap_or("")
    } else {
        &first.last
    };
    normalized_words(family).into_iter().next()
}

fn year_component(date: Option<&str>) -> Option<String> {
    let bytes = date?.as_bytes();
    bytes
        .windows(4)
        .find(|window| window.iter().all(u8::is_ascii_digit))
        .map(|window| String::from_utf8_lossy(window).into_owned())
}

fn title_component(title: Option<&str>) -> Option<String> {
    normalized_words(title?)
        .into_iter()
        .find(|word| !STOP_WORDS.contains(&word.as_str()))
}

fn normalized_words(value: &str) -> Vec<String> {
    deunicode(value)
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn alphabetic_suffix(mut index: usize) -> String {
    let mut suffix = Vec::new();
    loop {
        suffix.push((b'a' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    suffix.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_author_year_title() {
        assert_eq!(
            generate_citation_key(
                Some("Lovelace, Ada"),
                Some("1843-08"),
                Some("A Sketch of the Analytical Engine"),
                [],
            ),
            "lovelace1843sketch"
        );
    }

    #[test]
    fn transliterates_and_uses_fallbacks() {
        assert_eq!(
            generate_citation_key(Some("张, 伟"), None, Some("The 机器"), []),
            "zhangndji"
        );
        assert_eq!(generate_citation_key(None, None, None, []), "anonnditem");
    }

    #[test]
    fn appends_deterministic_collision_suffixes() {
        assert_eq!(
            generate_citation_key(
                Some("Doe, Jane"),
                Some("2026"),
                Some("Example"),
                ["doe2026example", "doe2026examplea", "doe2026exampleb"],
            ),
            "doe2026examplec"
        );
    }

    #[test]
    fn suffixes_continue_after_z() {
        assert_eq!(alphabetic_suffix(25), "z");
        assert_eq!(alphabetic_suffix(26), "aa");
        assert_eq!(alphabetic_suffix(27), "ab");
    }
}
