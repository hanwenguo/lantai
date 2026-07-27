use std::collections::HashSet;

use bibtex_parser::{PersonName, parse_names};
use deunicode::deunicode;

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "by", "for", "from", "in", "into", "of", "on", "or", "the", "to",
    "with",
];

/// A longer name list collapses to `TRUNCATED_NAMES` initials and `ET_AL`.
const MAX_NAMES: usize = 4;
const TRUNCATED_NAMES: usize = 3;
/// Letters taken from the family name of a lone author, or from the title.
const PREFIX_LETTERS: usize = 3;
/// The alphabetic styles print `\etalchar{+}` for the names they drop. A key is
/// read back by the BibTeX parser, which allows no `+`, so a hyphen stands in.
const ET_AL: char = '-';

/// Build a key in the Bib(La)TeX alphabetic style, such as `Lov43`, `GJ79`, or
/// `ABC-95`.
///
/// The name part follows `alpha.bst`: a lone author contributes the first three
/// letters of the family name, two to four authors contribute one initial each,
/// and a longer list or a trailing `and others` keeps three initials followed by
/// the et-al marker. Particles stay lowercase, so `van der Berg` alone becomes
/// `vdB`. Without an author the first significant title word takes the name
/// part's place, and an item with neither becomes `Anon`. The year is its final
/// two digits and is omitted when no date is known. Collisions receive `a`, `b`,
/// and later suffixes, as the alphabetic styles disambiguate identical labels.
pub fn generate_citation_key<'a>(
    author: Option<&str>,
    date: Option<&str>,
    title: Option<&str>,
    existing: impl IntoIterator<Item = &'a str>,
) -> String {
    let names = author_component(author)
        .or_else(|| title_component(title))
        .unwrap_or_else(|| "Anon".to_owned());
    let year = year_component(date).unwrap_or_default();
    let base = format!("{names}{year}");
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
    let mut names = parse_names(author?);
    let mut truncated = names.last().is_some_and(|name| {
        family_words(name)
            .first()
            .is_some_and(|word| word == "others")
    });
    if truncated {
        names.pop();
    }
    if names.len() > MAX_NAMES {
        truncated = true;
    }
    if truncated {
        names.truncate(TRUNCATED_NAMES);
    }

    let component = match names.as_slice() {
        [] => return None,
        [single] if !truncated => name_initials(single)
            .filter(|initials| initials.len() > 1)
            .or_else(|| family_prefix(single))?,
        names => {
            let initials = names.iter().filter_map(name_initials).collect::<String>();
            if initials.is_empty() {
                return None;
            }
            initials
        }
    };

    Some(if truncated {
        format!("{component}{ET_AL}")
    } else {
        component
    })
}

/// The initials `alpha.bst` writes as `{v{}}{l{}}`: one lowercase letter per
/// particle followed by an uppercase letter per family-name token.
fn name_initials(name: &PersonName) -> Option<String> {
    let particles = particle_words(name)
        .into_iter()
        .filter_map(|word| word.chars().next())
        .map(|letter| letter.to_ascii_lowercase());
    let family = family_words(name)
        .into_iter()
        .filter_map(|word| word.chars().next())
        .map(|letter| letter.to_ascii_uppercase());
    let initials = particles.chain(family).collect::<String>();
    (!initials.is_empty()).then_some(initials)
}

/// The opening letters of a lone author's family name, as in `Lovelace` to
/// `Lov`.
fn family_prefix(name: &PersonName) -> Option<String> {
    let family = family_words(name);
    Some(capitalized_prefix(family.first()?))
}

/// The family-name tokens of a person. A braced organization name such as
/// `{The Unicode Consortium}` is one token, as it is to BibTeX.
fn family_words(name: &PersonName) -> Vec<String> {
    if let Some(literal) = &name.literal {
        ascii_tokens([literal.as_str()])
    } else if name.family.is_empty() {
        ascii_tokens(name.last.split_whitespace())
    } else {
        ascii_tokens(name.family.iter().map(String::as_str))
    }
}

/// The particle tokens of a person, such as `van` and `der`.
fn particle_words(name: &PersonName) -> Vec<String> {
    if name.prefix.is_empty() {
        ascii_tokens(name.von.split_whitespace())
    } else {
        ascii_tokens(name.prefix.iter().map(String::as_str))
    }
}

fn year_component(date: Option<&str>) -> Option<String> {
    let bytes = date?.as_bytes();
    let year = bytes
        .windows(4)
        .find(|window| window.iter().all(u8::is_ascii_digit))?;
    Some(String::from_utf8_lossy(&year[2..]).into_owned())
}

fn title_component(title: Option<&str>) -> Option<String> {
    let words = ascii_words(title?);
    let word = words
        .iter()
        .find(|word| !STOP_WORDS.contains(&word.to_ascii_lowercase().as_str()))
        .or_else(|| words.first())?;
    Some(capitalized_prefix(word))
}

/// One transliterated ASCII word per source token. A token stays one word even
/// when transliteration spells it as several, so `张伟` counts once, the way
/// BibTeX counts the tokens the source wrote.
fn ascii_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    tokens
        .into_iter()
        .map(|token| ascii_words(token).concat())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Transliterated ASCII words, dropping punctuation and other characters.
fn ascii_words(value: &str) -> Vec<String> {
    deunicode(value)
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The opening letters of a word, capitalized so that labels look alike
/// regardless of how the source spelled the name. Later letters keep their case,
/// which preserves acronyms such as `ISO`.
fn capitalized_prefix(word: &str) -> String {
    word.chars()
        .take(PREFIX_LETTERS)
        .enumerate()
        .map(|(index, letter)| {
            if index == 0 {
                letter.to_ascii_uppercase()
            } else {
                letter
            }
        })
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
    use proptest::prelude::*;

    use super::*;

    fn key(author: Option<&str>, date: Option<&str>, title: Option<&str>) -> String {
        generate_citation_key(author, date, title, [])
    }

    #[test]
    fn generates_an_alphabetic_label() {
        assert_eq!(
            key(
                Some("Lovelace, Ada"),
                Some("1843-08"),
                Some("A Sketch of the Analytical Engine"),
            ),
            "Lov43"
        );
    }

    #[test]
    fn abbreviates_several_authors_to_initials() {
        assert_eq!(
            key(Some("Garey, M. and Johnson, D."), Some("1979"), None),
            "GJ79"
        );
        assert_eq!(
            key(
                Some("A, One and B, Two and C, Three and D, Four"),
                Some("1995"),
                None
            ),
            "ABCD95"
        );
        assert_eq!(
            key(
                Some("A, One and B, Two and C, Three and D, Four and E, Five"),
                Some("1995"),
                None,
            ),
            "ABC-95"
        );
        assert_eq!(
            key(Some("A, One and B, Two and others"), Some("1995"), None),
            "AB-95"
        );
    }

    #[test]
    fn keeps_particles_lowercase() {
        assert_eq!(key(Some("van der Berg, Jan"), Some("2019"), None), "vdB19");
        assert_eq!(
            key(Some("van der Berg, Jan and Jones, Ann"), Some("2019"), None),
            "vdBJ19"
        );
    }

    #[test]
    fn treats_an_organization_as_one_name() {
        assert_eq!(
            key(Some("{The Unicode Consortium}"), Some("2024"), None),
            "The24"
        );
        assert_eq!(key(Some("{ISO}"), Some("2016"), None), "ISO16");
        assert_eq!(
            key(Some("{ISO} and Jones, Ann"), Some("2016"), None),
            "IJ16"
        );
    }

    #[test]
    fn transliterates_and_uses_fallbacks() {
        assert_eq!(key(Some("张, 伟"), None, Some("The 机器")), "Zha");
        assert_eq!(key(Some("张伟 and 李娜"), Some("2024"), None), "ZL24");
        assert_eq!(
            key(None, Some("2019"), Some("An Uncollected Book")),
            "Unc19"
        );
        assert_eq!(key(None, None, None), "Anon");
    }

    #[test]
    fn appends_deterministic_collision_suffixes() {
        assert_eq!(
            generate_citation_key(
                Some("Doe, Jane"),
                Some("2026"),
                Some("Example"),
                ["Doe26", "Doe26a", "Doe26b"],
            ),
            "Doe26c"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// A generated key has to survive the round trip through the file: the
        /// entry parser reads a key as an identifier, which admits only
        /// alphanumerics and `_:.-`.
        #[test]
        fn a_generated_key_is_always_one_the_parser_reads_back(
            author in "\\PC{0,40}",
            date in "\\PC{0,12}",
            title in "\\PC{0,40}",
        ) {
            let key = generate_citation_key(Some(&author), Some(&date), Some(&title), []);
            prop_assert!(!key.is_empty());
            prop_assert!(
                key.chars()
                    .all(|character| character.is_ascii_alphanumeric()
                        || matches!(character, '_' | ':' | '.' | '-')),
                "{key:?} cannot be written as an entry key"
            );
        }
    }

    #[test]
    fn suffixes_continue_after_z() {
        assert_eq!(alphabetic_suffix(25), "z");
        assert_eq!(alphabetic_suffix(26), "aa");
        assert_eq!(alphabetic_suffix(27), "ab");
    }
}
