//! Date normalization shared by Lantai's Zotero ingest paths.

/// Month names a Zotero date can carry. Abbreviations sit beside full names so
/// that both `October` and `Oct.` resolve, and the longest match wins so that
/// `March` is not read as `Mar` of some other month.
const MONTHS: [(&str, u32); 36] = [
    ("january", 1),
    ("jan", 1),
    ("\u{4e00}\u{6708}", 1),
    ("february", 2),
    ("feb", 2),
    ("\u{4e8c}\u{6708}", 2),
    ("march", 3),
    ("mar", 3),
    ("\u{4e09}\u{6708}", 3),
    ("april", 4),
    ("apr", 4),
    ("\u{56db}\u{6708}", 4),
    ("may", 5),
    ("\u{4e94}\u{6708}", 5),
    ("mai", 5),
    ("june", 6),
    ("jun", 6),
    ("\u{516d}\u{6708}", 6),
    ("july", 7),
    ("jul", 7),
    ("\u{4e03}\u{6708}", 7),
    ("august", 8),
    ("aug", 8),
    ("\u{516b}\u{6708}", 8),
    ("september", 9),
    ("sep", 9),
    ("\u{4e5d}\u{6708}", 9),
    ("october", 10),
    ("oct", 10),
    ("\u{5341}\u{6708}", 10),
    ("november", 11),
    ("nov", 11),
    ("\u{5341}\u{4e00}\u{6708}", 11),
    ("december", 12),
    ("dec", 12),
    ("\u{5341}\u{4e8c}\u{6708}", 12),
];

/// Rewrite an incoming Zotero date as ISO 8601.
///
/// Neither wire carries a promise about the format: a translator passes on
/// whatever the page printed, and the RDF exporter renders `dc:date` in the
/// running application's locale, so `October 27, 2024`, `27/10/2024`,
/// `2024-10-27T00:00:00Z`, and `Spring 2026` all arrive as dates. BibLaTeX
/// wants `YYYY-MM-DD`, `YYYY-MM`, or `YYYY`, so recover what is recognizable
/// and narrow to the bare year, which is always valid, rather than writing
/// prose into a date field.
///
/// A value with no four-digit year at all is returned unchanged. There is no
/// date to recover from it, and replacing it with nothing would discard the
/// only thing the source said.
pub fn normalize(value: &str) -> String {
    let value = value.trim();
    // `2020/2021` and `2020-01-01/2021-01-01` are ISO 8601 intervals, which
    // BibLaTeX accepts. Normalize each endpoint rather than collapsing the
    // range onto whichever year happens to come first.
    if let Some((start, end)) = value.split_once('/')
        && has_year(start)
        && has_year(end)
    {
        return format!("{}/{}", normalize_one(start), normalize_one(end));
    }
    normalize_one(value)
}

fn normalize_one(value: &str) -> String {
    let value = value.trim();
    let lowered = value.to_lowercase();
    let named = MONTHS
        .iter()
        .filter(|(name, _)| lowered.contains(name))
        .max_by_key(|(name, _)| name.len())
        .map(|(_, month)| *month);

    let mut numbers = digit_groups(value);
    let Some(year_index) = numbers.iter().position(|(width, _)| *width == 4) else {
        return value.to_owned();
    };
    let year = numbers.remove(year_index).1;

    let mut rest = numbers.into_iter().map(|(_, number)| number);
    let (month, day) = match named {
        Some(month) => (Some(month), rest.next()),
        None => {
            let (first, second) = (rest.next(), rest.next());
            match (first, second) {
                // A day-first locale puts the day where the month is expected.
                // Only a source that led with the day can be in that order: one
                // that led with the year states the month next, so an
                // out-of-range value there is a broken date, not a swapped one.
                (Some(first), Some(second)) if year_index > 0 && first > 12 && second <= 12 => {
                    (Some(second), Some(first))
                }
                other => other,
            }
        }
    };

    match (month, day) {
        (Some(month), Some(day)) if (1..=12).contains(&month) && (1..=31).contains(&day) => {
            format!("{year:04}-{month:02}-{day:02}")
        }
        (Some(month), _) if (1..=12).contains(&month) => format!("{year:04}-{month:02}"),
        _ => format!("{year:04}"),
    }
}

/// The runs of digits in `value`, each with its width, so that a four-digit
/// run can be recognized as the year wherever the source chose to put it.
fn digit_groups(value: &str) -> Vec<(usize, u32)> {
    let mut numbers = Vec::new();
    for chunk in value.split(|character: char| !character.is_ascii_digit()) {
        if !chunk.is_empty()
            && let Ok(number) = chunk.parse::<u32>()
        {
            numbers.push((chunk.len(), number));
        }
    }
    numbers
}

fn has_year(value: &str) -> bool {
    digit_groups(value).iter().any(|(width, _)| *width == 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizable_dates_become_iso() {
        for (input, expected) in [
            ("2024-10-27", "2024-10-27"),
            ("2024", "2024"),
            (" 2024-10-27 ", "2024-10-27"),
            ("2024-10-27T12:30:00Z", "2024-10-27"),
            ("2026/7/3", "2026-07-03"),
            ("2026-02-29", "2026-02-29"),
            ("October 27, 2024", "2024-10-27"),
            ("27 Oct. 2024", "2024-10-27"),
            ("\u{5341}\u{6708} 12, 2017", "2017-10-12"),
            ("\u{5341}\u{4e8c}\u{6708} 20, 2019", "2019-12-20"),
            ("June, 2001", "2001-06"),
            ("2023/02/15", "2023-02-15"),
            ("07/2006", "2006-07"),
            ("2021/01", "2021-01"),
            ("2019.8", "2019-08"),
            ("27/10/2024", "2024-10-27"),
        ] {
            assert_eq!(normalize(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn unrecognizable_parts_narrow_to_the_year() {
        for (input, expected) in [
            ("Spring 2026", "2026"),
            ("Winter 2019-2020", "2019"),
            ("2024-13-01", "2024"),
            ("published 2024, revised later", "2024"),
        ] {
            assert_eq!(normalize(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn a_value_without_a_year_is_left_alone() {
        for input in ["no date here", "n.d.", "forthcoming", ""] {
            assert_eq!(normalize(input), input, "input {input:?}");
        }
    }

    #[test]
    fn an_iso_interval_keeps_both_endpoints() {
        assert_eq!(normalize("2020/2021"), "2020/2021");
        assert_eq!(normalize("2020-01-01/2021-06-30"), "2020-01-01/2021-06-30");
        assert_eq!(normalize("March 2020/April 2021"), "2020-03/2021-04");
    }
}
