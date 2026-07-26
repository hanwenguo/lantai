//! The search language shared by `lantai list` and the REST API.
//!
//! A query is a list of terms, all of which must match. A term is either a bare
//! word — the substring search the CLI has always had — or `name:value`, which
//! narrows the search to one part of the item. Terms are ANDed; a leading `-`
//! negates one.
//!
//! The CLI takes one term per argument, so the shell does the quoting. The REST
//! API takes them as a single `q` string, so this module also owns the
//! tokenizer that splits it and the encoder that produces it: the CLI encodes
//! its terms with [`Query::to_q`] when it proxies to the daemon, and that has
//! to round-trip through [`tokenize`] or daemon mode would answer a different
//! question than direct mode.

use std::cmp::Ordering;
use std::mem::take;

use crate::catalog::{CatalogField, CatalogItem, ItemView};
use crate::collections;
use crate::{Error, Result};

/// A parsed query: every term must match.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Query {
    terms: Vec<Term>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Term {
    negated: bool,
    matcher: Matcher,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Matcher {
    /// Substring of the citation key or any field value.
    Any(String),
    /// Substring of the citation key.
    Key(String),
    /// The entry type, matched whole rather than as a substring so that
    /// `type:book` does not also select `inbook`.
    Type(String),
    /// A collection, with the same nesting semantics as `--collection`. An
    /// empty name means "filed anywhere", so `-collection:` finds loose items.
    Collection(String),
    /// A publication year range. Both ends are optional, so an empty value
    /// asks only that the item have a year at all.
    Year { low: Option<i64>, high: Option<i64> },
    /// Substring of one named field. An absent field never matches, so an
    /// empty value asks only that the field be present.
    Field { name: String, value: String },
}

impl Query {
    /// Parse one term per element, taken verbatim.
    pub fn parse_terms<I, S>(terms: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        terms
            .into_iter()
            .map(|term| parse_term(term.as_ref()))
            .collect::<Result<Vec<_>>>()
            .map(|terms| Self { terms })
    }

    /// Parse the single-string form used on the wire.
    pub fn parse_str(query: &str) -> Result<Self> {
        Self::parse_terms(tokenize(query)?)
    }

    /// AND in the `--collection` flag, which predates the DSL and stays.
    #[must_use]
    pub fn with_collection(mut self, collection: Option<&str>) -> Self {
        if let Some(collection) = collection {
            self.terms.push(Term {
                negated: false,
                matcher: Matcher::Collection(collection.to_owned()),
            });
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn matches(&self, item: &CatalogItem) -> bool {
        self.terms
            .iter()
            .all(|term| term.matcher.matches(item) != term.negated)
    }

    /// Encode back into the single-string wire form, or `None` when empty.
    ///
    /// Every term is emitted in its scoped spelling, so nothing has to be
    /// guessed back apart on the far side.
    #[must_use]
    pub fn to_q(&self) -> Option<String> {
        if self.terms.is_empty() {
            return None;
        }
        let terms = self
            .terms
            .iter()
            .map(Term::encode)
            .collect::<Vec<_>>()
            .join(" ");
        Some(terms)
    }
}

impl Term {
    fn encode(&self) -> String {
        let body = match &self.matcher {
            Matcher::Any(value) => format!("any:{value}"),
            Matcher::Key(value) => format!("key:{value}"),
            Matcher::Type(value) => format!("type:{value}"),
            Matcher::Collection(value) => format!("collection:{value}"),
            Matcher::Year { low, high } => {
                let range = match (low, high) {
                    (Some(low), Some(high)) if low == high => low.to_string(),
                    (low, high) => format!(
                        "{}..{}",
                        low.map(|year| year.to_string()).unwrap_or_default(),
                        high.map(|year| year.to_string()).unwrap_or_default()
                    ),
                };
                format!("year:{range}")
            }
            Matcher::Field { name, value } => format!("{name}:{value}"),
        };
        let body = if self.negated {
            format!("-{body}")
        } else {
            body
        };
        quote(&body)
    }
}

impl Matcher {
    fn matches(&self, item: &CatalogItem) -> bool {
        match self {
            Self::Any(value) => {
                let value = value.to_lowercase();
                item.citation_key.to_lowercase().contains(&value)
                    || item
                        .fields
                        .iter()
                        .any(|field| field.value.to_lowercase().contains(&value))
            }
            Self::Key(value) => item
                .citation_key
                .to_lowercase()
                .contains(&value.to_lowercase()),
            Self::Type(value) => value.is_empty() || item.entry_type.eq_ignore_ascii_case(value),
            Self::Collection(name) => {
                if name.trim().is_empty() {
                    !item.collections.is_empty()
                } else {
                    item.collections
                        .iter()
                        .any(|candidate| collections::matches(candidate, name))
                }
            }
            Self::Year { low, high } => year_of(&item.fields).is_some_and(|year| {
                low.is_none_or(|low| year >= low) && high.is_none_or(|high| year <= high)
            }),
            Self::Field { name, value } => field_value(&item.fields, name)
                .is_some_and(|field| field.to_lowercase().contains(&value.to_lowercase())),
        }
    }
}

fn parse_term(raw: &str) -> Result<Term> {
    // A lone "-" is a word, not an empty negation.
    let (negated, body) = match raw.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, raw),
    };
    let matcher = match split_scope(body) {
        None => Matcher::Any(body.to_owned()),
        Some((name, value)) => match name.to_lowercase().as_str() {
            "any" => Matcher::Any(value.to_owned()),
            "key" => Matcher::Key(value.to_owned()),
            "type" => Matcher::Type(value.to_owned()),
            "collection" => Matcher::Collection(value.to_owned()),
            "year" => parse_year(raw, value)?,
            _ => Matcher::Field {
                name: name.to_lowercase(),
                value: value.to_owned(),
            },
        },
    };
    Ok(Term { negated, matcher })
}

/// Split `name:value` at the first colon, if the prefix is a plausible name.
///
/// The value keeps every later colon, so `doi:10.1000/a:b` needs no escaping.
fn split_scope(body: &str) -> Option<(&str, &str)> {
    let (name, value) = body.split_once(':')?;
    let mut characters = name.chars();
    if !characters.next()?.is_ascii_alphabetic() {
        return None;
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
        .then_some((name, value))
}

fn parse_year(raw: &str, value: &str) -> Result<Matcher> {
    let invalid = |message: &str| Error::InvalidQueryTerm {
        term: raw.to_owned(),
        message: message.to_owned(),
    };
    let bound = |text: &str| -> Result<Option<i64>> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        text.parse::<i64>()
            .map(Some)
            .map_err(|_| invalid("expected a year, YEAR.., ..YEAR, or YEAR..YEAR"))
    };

    let (low, high) = match value.split_once("..") {
        Some((low, high)) => {
            if high.contains("..") {
                return Err(invalid("a year range has only two ends"));
            }
            (bound(low)?, bound(high)?)
        }
        None => {
            let year = bound(value)?;
            (year, year)
        }
    };
    if let (Some(low), Some(high)) = (low, high)
        && low > high
    {
        return Err(invalid("the range starts after it ends"));
    }
    Ok(Matcher::Year { low, high })
}

/// The value of the first field with this name, compared case-insensitively.
pub fn field_value<'a>(fields: &'a [CatalogField], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|field| field.name.eq_ignore_ascii_case(name))
        .map(|field| field.value.as_str())
}

/// The publication year, taken from `year` and falling back to `date`.
///
/// `date` is commonly `2019-07`, so only its leading digits are read. The
/// extension scripts reimplement this rule in jq; keep the two in step.
pub fn year_of(fields: &[CatalogField]) -> Option<i64> {
    field_value(fields, "year")
        .and_then(leading_integer)
        .or_else(|| field_value(fields, "date").and_then(leading_integer))
}

fn leading_integer(value: &str) -> Option<i64> {
    let digits = value
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

/// Split the wire form into terms.
///
/// Whitespace separates terms; a double-quoted region protects whitespace, and
/// inside one `\"` and `\\` are literal. Quotes are removed: they are a
/// tokenization device, not part of any term.
pub fn tokenize(query: &str) -> Result<Vec<String>> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut characters = query.chars();

    while let Some(character) = characters.next() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match characters.next() {
                Some(escaped @ ('"' | '\\')) => current.push(escaped),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => return Err(unterminated(query)),
            },
            character if character.is_whitespace() && !quoted => {
                if started {
                    terms.push(take(&mut current));
                    started = false;
                }
            }
            character => {
                current.push(character);
                started = true;
            }
        }
    }

    if quoted {
        return Err(unterminated(query));
    }
    if started {
        terms.push(current);
    }
    Ok(terms)
}

fn unterminated(query: &str) -> Error {
    Error::InvalidQueryTerm {
        term: query.to_owned(),
        message: "unterminated quote".to_owned(),
    }
}

/// Quote a term so [`tokenize`] gives it back unchanged.
fn quote(term: &str) -> String {
    if !term
        .chars()
        .any(|character| character.is_whitespace() || character == '"' || character == '\\')
    {
        return term.to_owned();
    }
    let mut quoted = String::with_capacity(term.len() + 2);
    quoted.push('"');
    for character in term.chars() {
        if character == '"' || character == '\\' {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// An ordering for listed items: keys in priority order, each ascending or
/// descending.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sort {
    keys: Vec<SortTerm>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SortTerm {
    key: SortKey,
    descending: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SortKey {
    Key,
    Type,
    Title,
    Year,
    Field(String),
}

impl Sort {
    /// Parse `key,-other`: comma-separated keys, `-` for descending.
    pub fn parse(spec: &str) -> Result<Self> {
        let keys = spec
            .split(',')
            .map(|key| {
                let key = key.trim();
                let (descending, name) = match key.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, key.strip_prefix('+').unwrap_or(key)),
                };
                let name = name.trim();
                if name.is_empty() {
                    return Err(Error::InvalidSortKey {
                        key: key.to_owned(),
                    });
                }
                Ok(SortTerm {
                    key: match name.to_lowercase().as_str() {
                        "key" => SortKey::Key,
                        "type" => SortKey::Type,
                        "title" => SortKey::Title,
                        "year" => SortKey::Year,
                        other => SortKey::Field(other.to_owned()),
                    },
                    descending,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { keys })
    }

    /// Reorder in place. The sort is stable, so items that compare equal keep
    /// the order they have in the bibliography.
    pub fn apply(&self, items: &mut [ItemView]) {
        items.sort_by(|left, right| {
            for term in &self.keys {
                let ordering = match &term.key {
                    SortKey::Year => order(
                        year_of(&left.fields),
                        year_of(&right.fields),
                        term.descending,
                    ),
                    key => order(text_key(left, key), text_key(right, key), term.descending),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });
    }
}

fn text_key(item: &ItemView, key: &SortKey) -> Option<String> {
    let value = match key {
        SortKey::Key => Some(item.citation_key.as_str()),
        SortKey::Type => Some(item.entry_type.as_str()),
        SortKey::Title => item.title.as_deref(),
        SortKey::Field(name) => field_value(&item.fields, name),
        SortKey::Year => unreachable!("years are compared numerically"),
    };
    value.map(str::to_lowercase)
}

/// Compare two sort values, with missing ones last whichever way we are
/// sorting: an item with no year is not "the earliest", it is unknown.
fn order<T: Ord>(left: Option<T>, right: Option<T>, descending: bool) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => {
            if descending {
                right.cmp(&left)
            } else {
                left.cmp(&right)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        key: &str,
        entry_type: &str,
        fields: &[(&str, &str)],
        collections: &[&str],
    ) -> CatalogItem {
        CatalogItem {
            uuid: None,
            citation_key: key.to_owned(),
            entry_type: entry_type.to_owned(),
            fields: fields
                .iter()
                .map(|(name, value)| CatalogField {
                    name: (*name).to_owned(),
                    value: (*value).to_owned(),
                    raw: None,
                })
                .collect(),
            collections: collections.iter().map(|name| (*name).to_owned()).collect(),
            attachments: Vec::new(),
        }
    }

    fn sample() -> CatalogItem {
        item(
            "lovelace1843sketch",
            "article",
            &[
                ("title", "A Sketch of the Analytical Engine"),
                ("author", "Ada Lovelace"),
                ("year", "1843"),
            ],
            &["Computing/History"],
        )
    }

    fn matches(terms: &[&str], item: &CatalogItem) -> bool {
        Query::parse_terms(terms).unwrap().matches(item)
    }

    #[test]
    fn a_bare_term_still_searches_the_key_and_every_field() {
        let item = sample();
        assert!(matches(&["analytical"], &item));
        assert!(matches(&["LOVELACE"], &item), "case-insensitive");
        assert!(matches(&["1843sketch"], &item), "the citation key");
        assert!(!matches(&["babbage"], &item));
    }

    #[test]
    fn terms_are_anded_and_a_leading_hyphen_negates() {
        let item = sample();
        assert!(matches(&["author:lovelace", "year:1843"], &item));
        assert!(!matches(&["author:lovelace", "year:1844"], &item));
        assert!(matches(&["-author:babbage"], &item));
        assert!(!matches(&["-author:lovelace"], &item));
    }

    #[test]
    fn a_lone_hyphen_is_a_word_rather_than_an_empty_negation() {
        let term = Query::parse_terms(["-"]).unwrap();
        assert_eq!(
            term,
            Query {
                terms: vec![Term {
                    negated: false,
                    matcher: Matcher::Any("-".to_owned())
                }]
            }
        );
    }

    #[test]
    fn the_type_scope_matches_whole_types_rather_than_substrings() {
        let article = sample();
        let chapter = item("chapter", "inbook", &[], &[]);
        assert!(matches(&["type:article"], &article));
        assert!(matches(&["type:ARTICLE"], &article));
        assert!(!matches(&["type:book"], &chapter), "inbook is not book");
        assert!(matches(&["type:inbook"], &chapter));
    }

    #[test]
    fn the_collection_scope_follows_the_nesting_the_flag_does() {
        let item = sample();
        assert!(matches(&["collection:Computing"], &item));
        assert!(matches(&["collection:computing/history"], &item));
        assert!(!matches(&["collection:Computing/Theory"], &item));
    }

    #[test]
    fn an_empty_value_asks_only_that_something_be_there() {
        let filed = sample();
        let loose = item("loose", "misc", &[("title", "Loose")], &[]);

        assert!(matches(&["collection:"], &filed));
        assert!(!matches(&["collection:"], &loose));
        assert!(matches(&["-collection:"], &loose), "unfiled items");

        assert!(matches(&["author:"], &filed));
        assert!(!matches(&["author:"], &loose));
        assert!(matches(&["year:"], &filed));
        assert!(!matches(&["year:"], &loose));
    }

    #[test]
    fn an_unknown_field_matches_nothing_and_its_negation_matches_everything() {
        let item = sample();
        assert!(!matches(&["doi:10.1000"], &item));
        assert!(matches(&["-doi:10.1000"], &item));
    }

    #[test]
    fn years_come_from_the_year_field_and_fall_back_to_the_date_field() {
        let dated = item("dated", "online", &[("date", "2019-07-15")], &[]);
        assert!(matches(&["year:2019"], &dated));
        assert!(matches(&["year:2015..2020"], &dated));
        assert!(matches(&["year:..2020"], &dated));
        assert!(matches(&["year:2019.."], &dated));
        assert!(!matches(&["year:2020.."], &dated));

        let undated = item("undated", "misc", &[("title", "No date")], &[]);
        assert!(!matches(&["year:2019"], &undated));
        assert!(!matches(&["year:.."], &undated));
    }

    #[test]
    fn a_malformed_year_is_an_error_rather_than_a_silent_mismatch() {
        for term in ["year:recent", "year:2019..2020..2021", "year:2020..2019"] {
            assert!(
                matches!(
                    Query::parse_terms([term]),
                    Err(Error::InvalidQueryTerm { .. })
                ),
                "{term} was accepted"
            );
        }
    }

    #[test]
    fn a_scope_splits_at_the_first_colon_and_any_is_the_escape_hatch() {
        let item = item(
            "doi",
            "article",
            &[("doi", "10.1000/nested:value"), ("url", "http://x/y")],
            &[],
        );
        assert!(matches(&["doi:10.1000/nested:value"], &item));
        assert!(matches(&["any:http://x/y"], &item));
        assert!(matches(&["url:x/y"], &item), "a scoped substring");
        assert!(
            !matches(&["http://x/y"], &item),
            "a bare URL reads as a scope; any: is how you mean it literally"
        );
    }

    #[test]
    fn a_prefix_that_is_not_name_shaped_is_a_bare_word() {
        assert_eq!(
            Query::parse_terms(["10.1000:x"]).unwrap().terms[0].matcher,
            Matcher::Any("10.1000:x".to_owned())
        );
    }

    #[test]
    fn the_wire_form_round_trips_every_term() {
        let terms = [
            "attention",
            "any:spaced value",
            "key:vaswani",
            "type:online",
            "collection:Projects/IfT",
            "year:2019..2024",
            "year:2019..",
            "year:..2024",
            "year:1843",
            "year:",
            "-collection:",
            "title:a \"quoted\" phrase",
            "note:back\\slash",
            "-author:knuth",
        ];
        let parsed = Query::parse_terms(terms).unwrap();
        let encoded = parsed.to_q().unwrap();
        assert_eq!(Query::parse_str(&encoded).unwrap(), parsed, "{encoded}");
        assert_eq!(Query::default().to_q(), None);
    }

    #[test]
    fn tokenizing_protects_quoted_whitespace_and_rejects_an_open_quote() {
        assert_eq!(tokenize("a b").unwrap(), ["a", "b"]);
        assert_eq!(tokenize("  a   b  ").unwrap(), ["a", "b"]);
        assert_eq!(tokenize("title:\"a b\"").unwrap(), ["title:a b"]);
        assert_eq!(tokenize("\"a b\" c").unwrap(), ["a b", "c"]);
        assert_eq!(tokenize("\"\"").unwrap(), [""]);
        assert!(matches!(
            tokenize("\"unclosed"),
            Err(Error::InvalidQueryTerm { .. })
        ));
    }

    #[test]
    fn sorting_puts_missing_values_last_in_both_directions() {
        let views =
            |items: Vec<CatalogItem>| items.into_iter().map(ItemView::from).collect::<Vec<_>>();
        let mut items = views(vec![
            item("b", "misc", &[("year", "2001")], &[]),
            item("a", "misc", &[], &[]),
            item("c", "misc", &[("year", "1999")], &[]),
        ]);

        Sort::parse("year").unwrap().apply(&mut items);
        let keys = |items: &[ItemView]| {
            items
                .iter()
                .map(|item| item.citation_key.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&items), ["c", "b", "a"]);

        Sort::parse("-year").unwrap().apply(&mut items);
        assert_eq!(keys(&items), ["b", "c", "a"]);
    }

    #[test]
    fn sorting_falls_through_to_later_keys_and_is_otherwise_stable() {
        let mut items = vec![
            ItemView::from(item("second", "book", &[("year", "2001")], &[])),
            ItemView::from(item("first", "book", &[("year", "2001")], &[])),
            ItemView::from(item("third", "article", &[("year", "2001")], &[])),
        ];
        Sort::parse("year,key").unwrap().apply(&mut items);
        assert_eq!(
            items
                .iter()
                .map(|item| item.citation_key.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );

        Sort::parse("year").unwrap().apply(&mut items);
        assert_eq!(
            items
                .iter()
                .map(|item| item.citation_key.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"],
            "equal keys keep their order"
        );
    }

    #[test]
    fn sort_keys_cover_titles_types_and_arbitrary_fields() {
        let mut items = vec![
            ItemView::from(item(
                "b",
                "book",
                &[("title", "Zeta"), ("author", "Ada")],
                &[],
            )),
            ItemView::from(item(
                "a",
                "article",
                &[("title", "alpha"), ("author", "Zed")],
                &[],
            )),
        ];
        Sort::parse("title").unwrap().apply(&mut items);
        assert_eq!(items[0].citation_key, "a", "case-insensitive text order");
        Sort::parse("author").unwrap().apply(&mut items);
        assert_eq!(items[0].citation_key, "b");
        Sort::parse("type").unwrap().apply(&mut items);
        assert_eq!(items[0].citation_key, "a");
    }

    #[test]
    fn an_empty_sort_key_is_rejected() {
        for spec in ["", "year,", "-", " , "] {
            assert!(
                matches!(Sort::parse(spec), Err(Error::InvalidSortKey { .. })),
                "{spec:?} was accepted"
            );
        }
    }
}
