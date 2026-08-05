use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::dates::normalize as normalize_date;
use crate::library::NewItem;
use crate::{Error, Result};

#[derive(Clone, Debug, Deserialize)]
pub struct ZoteroItem {
    pub id: String,
    #[serde(rename = "itemType")]
    pub item_type: String,
    #[serde(default)]
    pub creators: Vec<ZoteroCreator>,
    /// Zotero's tags. Accepted so the field does not land in `data`, and then
    /// ignored: on this wire they are whatever a translator scraped, which
    /// describes the paper rather than records a decision about it.
    #[serde(default)]
    pub tags: Vec<JsonValue>,
    /// The collections this item joins. Never parsed from Zotero — the caller
    /// sets it from the save popup's target or a Zotero RDF export's
    /// membership, so a change to tag handling cannot silently unfile items.
    #[serde(skip)]
    pub collections: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<JsonValue>,
    #[serde(flatten)]
    pub data: BTreeMap<String, JsonValue>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ZoteroCreator {
    #[serde(default, rename = "firstName")]
    pub first_name: String,
    #[serde(default, rename = "lastName")]
    pub last_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "creatorType")]
    pub creator_type: String,
    #[serde(default, rename = "fieldMode")]
    pub field_mode: JsonValue,
}

#[derive(Clone, Debug)]
pub struct MappedItem {
    pub connector_id: String,
    pub item: NewItem,
}

pub fn map_item(source: ZoteroItem) -> Result<MappedItem> {
    if source.id.trim().is_empty() {
        return Err(Error::InvalidFieldArgument {
            argument: "Connector item id is empty".to_owned(),
        });
    }
    if source.item_type.trim().is_empty() {
        return Err(Error::InvalidEntryType {
            entry_type: source.item_type,
        });
    }

    let mut fields = BTreeMap::new();
    let mut consumed = HashSet::new();
    for (biblatex, zotero) in [
        ("location", "place"),
        ("chapter", "chapter"),
        ("edition", "edition"),
        ("title", "title"),
        ("volume", "volume"),
        ("rights", "rights"),
        ("isbn", "ISBN"),
        ("issn", "ISSN"),
        ("url", "url"),
        ("doi", "DOI"),
        ("series", "series"),
        ("shorttitle", "shortTitle"),
        ("holder", "assignee"),
        ("abstract", "abstractNote"),
        ("volumes", "numberOfVolumes"),
        ("version", "version"),
        ("eventtitle", "conferenceName"),
        ("pages", "pages"),
        ("pagetotal", "numPages"),
    ] {
        map_scalar(&source.data, &mut fields, &mut consumed, zotero, biblatex);
    }

    map_first_scalar(
        &source.data,
        &mut fields,
        &mut consumed,
        &[
            "reportNumber",
            "seriesNumber",
            "billNumber",
            "episodeNumber",
            "number",
        ],
        "number",
    );
    if let Some(issue) = scalar(&source.data, "issue") {
        consumed.insert("issue".to_owned());
        let field = if issue.chars().all(|character| character.is_ascii_digit()) {
            "number"
        } else {
            "issue"
        };
        fields.entry(field.to_owned()).or_insert(issue);
    }

    if let Some(publication) = scalar(&source.data, "publicationTitle") {
        consumed.insert("publicationTitle".to_owned());
        let field = match source.item_type.as_str() {
            "bookSection" | "conferencePaper" | "dictionaryEntry" | "encyclopediaArticle" => {
                "booktitle"
            }
            _ => "journaltitle",
        };
        fields.insert(field.to_owned(), publication);
        if source.item_type == "journalArticle"
            && let Some(abbreviation) = scalar(&source.data, "journalAbbreviation")
        {
            consumed.insert("journalAbbreviation".to_owned());
            fields.insert("shortjournal".to_owned(), abbreviation);
        }
    }

    map_first_scalar(
        &source.data,
        &mut fields,
        &mut consumed,
        &["websiteTitle", "forumTitle", "blogTitle", "programTitle"],
        "titleaddon",
    );
    if let Some(publisher) = scalar(&source.data, "publisher") {
        consumed.insert("publisher".to_owned());
        let field = if matches!(source.item_type.as_str(), "thesis" | "report") {
            "institution"
        } else {
            "publisher"
        };
        fields.insert(field.to_owned(), publisher);
    }

    map_type_fields(&source, &mut fields, &mut consumed);
    map_archive(&source.data, &mut fields, &mut consumed);
    map_creators(&source.creators, &mut fields);

    if let Some(access_date) = scalar(&source.data, "accessDate") {
        consumed.insert("accessDate".to_owned());
        if access_date != "CURRENT_TIMESTAMP" {
            fields.insert("urldate".to_owned(), normalize_date(&access_date));
        }
    }
    if let Some(date) = scalar(&source.data, "date") {
        consumed.insert("date".to_owned());
        fields.insert("date".to_owned(), normalize_date(&date));
    }
    if let Some(language) = scalar(&source.data, "language") {
        consumed.insert("language".to_owned());
        fields.insert("langid".to_owned(), map_language(&language));
    }

    let collections = crate::collections::normalize(source.collections.iter().map(String::as_str));
    if !collections.is_empty() {
        fields.insert("keywords".to_owned(), collections.join(", "));
    }
    fields.insert("zotero-item-type".to_owned(), source.item_type.clone());

    let citation_key = scalar(&source.data, "citationKey");
    consumed.insert("citationKey".to_owned());
    for (name, value) in &source.data {
        if consumed.contains(name) {
            continue;
        }
        if let Some(value) = scalar_value(value) {
            fields.insert(format!("zotero-{}", kebab_case(name)), value);
        }
    }

    let entry_type = map_entry_type(&source.item_type, &source.creators, &source.data).to_owned();
    Ok(MappedItem {
        connector_id: source.id,
        item: NewItem {
            entry_type,
            citation_key,
            fields: fields.into_iter().collect(),
        },
    })
}

pub fn map_entry_type<'a>(
    item_type: &'a str,
    creators: &[ZoteroCreator],
    data: &BTreeMap<String, JsonValue>,
) -> &'a str {
    if item_type == "bookSection"
        && creators
            .iter()
            .any(|creator| creator.creator_type == "bookAuthor")
    {
        return "inbook";
    }
    if item_type == "book" {
        let has_author = creators
            .iter()
            .any(|creator| creator.creator_type == "author");
        let has_editor = creators
            .iter()
            .any(|creator| creator.creator_type == "editor");
        if !has_author && has_editor {
            return "collection";
        }
        if scalar(data, "numberOfVolumes").is_some() {
            return "mvbook";
        }
    }
    match item_type {
        "book" => "book",
        "bookSection" => "incollection",
        "journalArticle" | "magazineArticle" | "newspaperArticle" => "article",
        "thesis" => "thesis",
        "letter" | "email" => "letter",
        "manuscript" | "presentation" => "unpublished",
        "film" => "movie",
        "artwork" => "artwork",
        "webpage" | "blogPost" | "forumPost" | "preprint" => "online",
        "conferencePaper" => "inproceedings",
        "report" => "report",
        "bill" | "statute" => "legislation",
        "case" | "hearing" => "jurisdiction",
        "patent" => "patent",
        "audioRecording" | "podcast" => "audio",
        "videoRecording" => "video",
        "computerProgram" => "software",
        "encyclopediaArticle" | "dictionaryEntry" => "inreference",
        "dataset" => "dataset",
        "standard" => "standard",
        _ => "misc",
    }
}

fn map_type_fields(
    source: &ZoteroItem,
    fields: &mut BTreeMap<String, String>,
    consumed: &mut HashSet<String>,
) {
    let typed = match source.item_type.as_str() {
        "letter" => scalar(&source.data, "letterType").or_else(|| Some("Letter".to_owned())),
        "email" => Some("E-mail".to_owned()),
        "thesis" => scalar(&source.data, "thesisType").map_or_else(
            || Some("phdthesis".to_owned()),
            |value| {
                if value.to_ascii_lowercase().replace('.', "").contains("phd") {
                    Some("phdthesis".to_owned())
                } else {
                    Some(value)
                }
            },
        ),
        _ => [
            "manuscriptType",
            "websiteType",
            "presentationType",
            "reportType",
            "mapType",
            "standardType",
        ]
        .into_iter()
        .find_map(|name| scalar(&source.data, name)),
    };
    for name in [
        "letterType",
        "thesisType",
        "manuscriptType",
        "websiteType",
        "presentationType",
        "reportType",
        "mapType",
        "standardType",
    ] {
        if source.data.contains_key(name) {
            consumed.insert(name.to_owned());
        }
    }
    if let Some(typed) = typed {
        fields.insert("type".to_owned(), typed);
    }
    if let Some(how) =
        scalar(&source.data, "presentationType").or_else(|| scalar(&source.data, "manuscriptType"))
    {
        fields.insert("howpublished".to_owned(), how);
    }
    if let Some(meeting) = scalar(&source.data, "meetingName") {
        consumed.insert("meetingName".to_owned());
        fields.insert("note".to_owned(), meeting);
    }
    if source.item_type == "patent"
        && let Some(number) = scalar(&source.data, "patentNumber")
    {
        consumed.insert("patentNumber".to_owned());
        let (patent_type, number) = [
            ("US", "patentus"),
            ("EP", "patenteu"),
            ("GB", "patentuk"),
            ("DE", "patentde"),
            ("FR", "patentfr"),
        ]
        .into_iter()
        .find_map(|(prefix, patent_type)| {
            number
                .strip_prefix(prefix)
                .map(|number| (patent_type, number.to_owned()))
        })
        .unwrap_or(("patent", number));
        fields.insert("type".to_owned(), patent_type.to_owned());
        fields.insert("number".to_owned(), number);
    }
}

fn map_archive(
    data: &BTreeMap<String, JsonValue>,
    fields: &mut BTreeMap<String, String>,
    consumed: &mut HashSet<String>,
) {
    let (Some(archive), Some(location)) =
        (scalar(data, "archive"), scalar(data, "archiveLocation"))
    else {
        return;
    };
    consumed.insert("archive".to_owned());
    consumed.insert("archiveLocation".to_owned());
    let eprint_type = match archive.to_ascii_lowercase().as_str() {
        "arxiv" => "arxiv",
        "jstor" => "jstor",
        "pubmed" => "pubmed",
        "hdl" => "hdl",
        "google books" | "googlebooks" => "googlebooks",
        _ => {
            fields.insert("zotero-archive".to_owned(), archive);
            fields.insert("zotero-archive-location".to_owned(), location);
            return;
        }
    };
    fields.insert("eprinttype".to_owned(), eprint_type.to_owned());
    fields.insert("eprint".to_owned(), location);
    if eprint_type == "arxiv"
        && let Some(class) = scalar(data, "callNumber")
    {
        consumed.insert("callNumber".to_owned());
        fields.insert("eprintclass".to_owned(), class);
    }
}

fn map_creators(creators: &[ZoteroCreator], fields: &mut BTreeMap<String, String>) {
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for creator in creators {
        let field = match creator.creator_type.as_str() {
            "author" | "interviewer" | "inventor" | "director" | "programmer" | "artist"
            | "podcaster" | "presenter" => "author",
            "bookAuthor" => "bookauthor",
            "commenter" => "commentator",
            "editor" => "editor",
            "translator" => "translator",
            "seriesEditor" => "editorb",
            _ => "editora",
        };
        let corporate = creator.field_mode == JsonValue::Bool(true)
            || creator.field_mode.as_i64().is_some_and(|value| value != 0);
        let last = if creator.last_name.is_empty() {
            &creator.name
        } else {
            &creator.last_name
        };
        if last.trim().is_empty() {
            continue;
        }
        let rendered = if corporate {
            format!("{{{}}}", last.trim())
        } else if creator.first_name.trim().is_empty() {
            last.trim().to_owned()
        } else {
            format!("{}, {}", last.trim(), creator.first_name.trim())
        };
        groups.entry(field).or_default().push(rendered);
    }
    for (field, creators) in groups {
        fields.insert(field.to_owned(), creators.join(" and "));
        if field == "editora" {
            fields.insert("editoratype".to_owned(), "collaborator".to_owned());
        } else if field == "editorb" {
            fields.insert("editorbtype".to_owned(), "redactor".to_owned());
        }
    }
}

fn map_scalar(
    data: &BTreeMap<String, JsonValue>,
    fields: &mut BTreeMap<String, String>,
    consumed: &mut HashSet<String>,
    source: &str,
    destination: &str,
) {
    if let Some(value) = scalar(data, source) {
        consumed.insert(source.to_owned());
        fields.insert(destination.to_owned(), value);
    }
}

fn map_first_scalar(
    data: &BTreeMap<String, JsonValue>,
    fields: &mut BTreeMap<String, String>,
    consumed: &mut HashSet<String>,
    sources: &[&str],
    destination: &str,
) {
    for source in sources {
        if let Some(value) = scalar(data, source) {
            consumed.insert((*source).to_owned());
            fields.insert(destination.to_owned(), value);
            return;
        }
    }
}

fn scalar(data: &BTreeMap<String, JsonValue>, name: &str) -> Option<String> {
    data.get(name).and_then(scalar_value)
}

fn scalar_value(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn map_language(value: &str) -> String {
    match value
        .split(['-', '_'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase()
        .as_str()
    {
        "en" => "english",
        "de" => "german",
        "fr" => "french",
        "es" => "spanish",
        "it" => "italian",
        "pt" => "portuguese",
        "zh" => "pinyin",
        "ja" => "japanese",
        "ru" => "russian",
        _ => value,
    }
    .to_owned()
}

fn kebab_case(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 && !output.ends_with('-') {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
        } else if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
        } else if !output.ends_with('-') {
            output.push('-');
        }
    }
    output.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_translated_item_and_preserves_custom_scalars() {
        let source: ZoteroItem = serde_json::from_value(serde_json::json!({
            "id": "connector-1",
            "itemType": "journalArticle",
            "title": "A Sketch",
            "creators": [
                {"firstName": "Ada", "lastName": "Lovelace", "creatorType": "author"},
                {"lastName": "Analytical Society", "creatorType": "editor", "fieldMode": 1}
            ],
            "date": "1843",
            "publicationTitle": "Scientific Memoirs",
            "journalAbbreviation": "Sci. Mem.",
            "DOI": "10.1000/example",
            "tags": [
                {"tag": "Scraped Keyword", "type": 1},
                {"tag": "History", "type": 0},
                "Computing"
            ],
            "customScalar": "retained",
            "relations": {"ignored": true}
        }))
        .unwrap();

        let mapped = map_item(source).unwrap();
        let fields = mapped.item.fields.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(mapped.item.entry_type, "article");
        assert!(
            !fields.contains_key("keywords"),
            "nothing in the wire tags array becomes a collection"
        );
        assert_eq!(fields["author"], "Lovelace, Ada");
        assert_eq!(fields["editor"], "{Analytical Society}");
        assert_eq!(fields["journaltitle"], "Scientific Memoirs");
        assert_eq!(fields["shortjournal"], "Sci. Mem.");
        assert_eq!(fields["doi"], "10.1000/example");
        assert_eq!(fields["date"], "1843");
        assert_eq!(fields["zotero-custom-scalar"], "retained");
        assert!(!fields.contains_key("zotero-relations"));
    }

    #[test]
    fn collections_come_from_the_caller_not_from_zotero_tags() {
        let item = |tags: serde_json::Value, collections: &[&str]| {
            let mut source: ZoteroItem = serde_json::from_value(serde_json::json!({
                "id": "connector-1",
                "itemType": "journalArticle",
                "title": "A Sketch",
                "tags": tags
            }))
            .unwrap();
            source.collections = collections.iter().map(|name| (*name).to_owned()).collect();
            map_item(source)
                .unwrap()
                .item
                .fields
                .into_iter()
                .collect::<BTreeMap<_, _>>()
                .remove("keywords")
        };

        // Every encoding a translator might use, and none of them file the item.
        assert_eq!(
            item(
                serde_json::json!([
                    {"tag": "cs.LG", "type": 1},
                    {"tag": "Economics", "type": 0},
                    {"tag": "Physics", "type": "1"},
                    "Machine Learning"
                ]),
                &[]
            ),
            None
        );
        assert_eq!(
            item(serde_json::json!([{"tag": "cs.LG", "type": 1}]), &["Inbox"]),
            Some("Inbox".to_owned()),
            "the caller's choice is unaffected by whatever the translator sent"
        );
        assert_eq!(
            item(
                serde_json::json!([]),
                &[" Reading ", "READING", "", "Projects/IfT"]
            ),
            Some("Projects/IfT, Reading".to_owned()),
            "memberships are trimmed, deduplicated, and ordered"
        );
    }

    #[test]
    fn date_and_access_date_arrive_as_iso() {
        let dates = |date: &str, access_date: &str| {
            let source: ZoteroItem = serde_json::from_value(serde_json::json!({
                "id": "connector-1",
                "itemType": "journalArticle",
                "title": "A Sketch",
                "date": date,
                "accessDate": access_date
            }))
            .unwrap();
            let fields = map_item(source)
                .unwrap()
                .item
                .fields
                .into_iter()
                .collect::<BTreeMap<_, _>>();
            (fields["date"].clone(), fields["urldate"].clone())
        };

        assert_eq!(
            dates("2026/7/3", "2026-07-03T12:30:00Z"),
            ("2026-07-03".to_owned(), "2026-07-03".to_owned())
        );
        assert_eq!(
            dates("Spring 2026", "October 27, 2024"),
            ("2026".to_owned(), "2024-10-27".to_owned()),
            "a date a translator scraped as prose still reaches BibLaTeX as a date"
        );
    }

    #[test]
    fn every_current_zotero_parent_type_has_a_biblatex_fallback() {
        for item_type in [
            "annotation",
            "artwork",
            "attachment",
            "audioRecording",
            "bill",
            "blogPost",
            "book",
            "bookSection",
            "case",
            "computerProgram",
            "conferencePaper",
            "dataset",
            "dictionaryEntry",
            "document",
            "email",
            "encyclopediaArticle",
            "film",
            "forumPost",
            "hearing",
            "instantMessage",
            "interview",
            "journalArticle",
            "letter",
            "magazineArticle",
            "manuscript",
            "map",
            "newspaperArticle",
            "note",
            "patent",
            "podcast",
            "preprint",
            "presentation",
            "radioBroadcast",
            "report",
            "standard",
            "statute",
            "thesis",
            "tvBroadcast",
            "videoRecording",
            "webpage",
        ] {
            assert!(!map_entry_type(item_type, &[], &BTreeMap::new()).is_empty());
        }
    }
}
