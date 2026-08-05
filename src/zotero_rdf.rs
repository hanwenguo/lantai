use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};
use serde_json::Value as JsonValue;

use crate::dates::normalize as normalize_date;
use crate::zotero::{ZoteroCreator, ZoteroItem};
use crate::{Error, Result};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const Z: &str = "http://www.zotero.org/namespaces/export#";
const DC: &str = "http://purl.org/dc/elements/1.1/";
const DCTERMS: &str = "http://purl.org/dc/terms/";
const BIB: &str = "http://purl.org/net/biblio#";
const FOAF: &str = "http://xmlns.com/foaf/0.1/";
const LINK: &str = "http://purl.org/rss/1.0/modules/link/";
const VCARD: &str = "http://nwalsh.com/rdf/vCard#";
const PRISM: &str = "http://prismstandard.org/namespaces/";

/// Guards against a `dcterms:hasPart` cycle between collections.
const MAX_COLLECTION_DEPTH: usize = 32;

/// One Zotero RDF export, translated into the Connector item shape.
#[derive(Clone, Debug)]
pub struct RdfImport {
    pub items: Vec<RdfItem>,
    pub collections: Vec<String>,
    pub skipped: Vec<SkippedAttachment>,
}

#[derive(Clone, Debug)]
pub struct RdfItem {
    pub item: ZoteroItem,
    pub attachments: Vec<RdfAttachment>,
}

#[derive(Clone, Debug)]
pub struct RdfAttachment {
    pub path: PathBuf,
    pub title: String,
    pub media_type: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SkippedAttachment {
    pub item: String,
    pub title: String,
    pub reason: String,
}

/// Read a Zotero RDF export.
///
/// `path` locates the `.rdf` document itself; exported attachment files are
/// resolved relative to its parent directory. `base_directory` resolves the
/// `attachments:` scheme Zotero writes for linked files it did not copy into
/// the export, and corresponds to Zotero's linked attachment base directory.
pub fn parse(path: &Path, source: &str, base_directory: Option<&Path>) -> Result<RdfImport> {
    let document = Document::parse(source).map_err(|error| Error::ParseZoteroRdf {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));

    let mut collections = Vec::new();
    let mut attachments = HashMap::new();
    let mut items = Vec::new();
    for node in document.root_element().children().filter(Node::is_element) {
        if tagged(node, Z, "Collection") {
            collections.push(node);
        } else if tagged(node, Z, "Attachment") || item_type(node).as_deref() == Some("attachment")
        {
            if let Some(about) = node.attribute((RDF, "about")) {
                attachments.insert(about.to_owned(), node);
            }
        } else if item_type(node).is_some() {
            items.push(node);
        }
    }

    let (memberships, paths) = collection_memberships(&collections);

    let mut skipped = Vec::new();
    let items = items
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            read_item(
                node,
                index,
                &memberships,
                &attachments,
                base,
                base_directory,
                &mut skipped,
            )
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Err(Error::ZoteroRdfHasNoItems {
            path: path.to_path_buf(),
        });
    }

    Ok(RdfImport {
        items,
        collections: paths,
        skipped,
    })
}

fn read_item(
    node: Node<'_, '_>,
    index: usize,
    memberships: &HashMap<String, Vec<String>>,
    attachments: &HashMap<String, Node<'_, '_>>,
    base: &Path,
    base_directory: Option<&Path>,
    skipped: &mut Vec<SkippedAttachment>,
) -> RdfItem {
    let about = node.attribute((RDF, "about")).unwrap_or_default();
    let item_type = item_type(node).unwrap_or_default();

    let mut data = BTreeMap::new();
    let mut creators = Vec::new();
    let mut links = Vec::new();
    for child in node.children().filter(Node::is_element) {
        read_item_child(child, &item_type, &mut data, &mut creators, &mut links);
    }
    for container in node
        .children()
        .filter(|child| tagged(*child, DCTERMS, "isPartOf") || tagged(*child, BIB, "presentedAt"))
    {
        for child in container.children().filter(Node::is_element) {
            read_container(child, &mut data, 0);
        }
    }

    let label = data
        .get("citationKey")
        .or_else(|| data.get("title"))
        .and_then(JsonValue::as_str)
        .unwrap_or(about)
        .to_owned();
    let resolved = links
        .into_iter()
        .filter_map(|reference| attachments.get(&reference))
        .filter_map(
            |attachment| match read_attachment(*attachment, base, base_directory) {
                Ok(attachment) => Some(attachment),
                Err(reason) => {
                    skipped.push(SkippedAttachment {
                        item: label.clone(),
                        title: attachment_title(*attachment),
                        reason,
                    });
                    None
                }
            },
        )
        .collect();

    let collections = memberships.get(about).cloned().unwrap_or_default();

    RdfItem {
        item: ZoteroItem {
            id: if about.is_empty() {
                format!("rdf-item-{index}")
            } else {
                about.to_owned()
            },
            item_type,
            creators,
            // Zotero's own `dc:subject` tags are not imported.
            tags: Vec::new(),
            collections,
            attachments: Vec::new(),
            data,
        },
        attachments: resolved,
    }
}

fn read_item_child(
    node: Node<'_, '_>,
    item_type: &str,
    data: &mut BTreeMap<String, JsonValue>,
    creators: &mut Vec<ZoteroCreator>,
    links: &mut Vec<String>,
) {
    let scalar = |name: &str, data: &mut BTreeMap<String, JsonValue>| {
        if let Some(value) = text(node) {
            data.entry(name.to_owned())
                .or_insert(JsonValue::String(value));
        }
    };

    match () {
        () if tagged(node, Z, "citationKey") => scalar("citationKey", data),
        () if tagged(node, DC, "title") => scalar("title", data),
        () if tagged(node, DCTERMS, "abstract") => scalar("abstractNote", data),
        () if tagged(node, DC, "date") => {
            if let Some(value) = text(node) {
                data.entry("date".to_owned())
                    .or_insert(JsonValue::String(normalize_date(&value)));
            }
        }
        () if tagged(node, DCTERMS, "dateSubmitted") => {
            if let Some(value) = text(node) {
                data.entry("accessDate".to_owned())
                    .or_insert(JsonValue::String(normalize_date(&value)));
            }
        }
        () if tagged(node, DC, "description") => scalar("extra", data),
        () if tagged(node, DC, "rights") => scalar("rights", data),
        () if tagged(node, Z, "language") => scalar("language", data),
        () if tagged(node, Z, "shortTitle") => scalar("shortTitle", data),
        () if tagged(node, Z, "libraryCatalog") => scalar("libraryCatalog", data),
        () if tagged(node, Z, "numPages") => scalar("numPages", data),
        () if tagged(node, Z, "archiveLocation") => scalar("archiveLocation", data),
        () if tagged(node, DCTERMS, "alternative") => scalar("journalAbbreviation", data),
        () if tagged(node, BIB, "pages") => {
            if let Some(value) = text(node) {
                data.entry("pages".to_owned())
                    .or_insert(JsonValue::String(normalize_pages(&value)));
            }
        }
        () if prism(node, "volume") => scalar("volume", data),
        () if prism(node, "edition") => scalar("edition", data),
        () if prism(node, "number") => {
            let name = if item_type == "report" {
                "reportNumber"
            } else {
                "issue"
            };
            scalar(name, data);
        }
        () if tagged(node, Z, "type") => {
            let name = match item_type {
                "thesis" => "thesisType",
                "report" => "reportType",
                "manuscript" => "manuscriptType",
                "presentation" => "presentationType",
                _ => "type",
            };
            scalar(name, data);
        }
        () if tagged(node, DC, "publisher") => read_publisher(node, data),
        () if tagged(node, DC, "identifier") => read_identifier(node, data, "callNumber"),
        () if tagged(node, BIB, "authors") => read_creators(node, "author", creators),
        () if tagged(node, BIB, "editors") => read_creators(node, "editor", creators),
        () if tagged(node, BIB, "translators") => read_creators(node, "translator", creators),
        () if tagged(node, BIB, "contributors") => read_creators(node, "contributor", creators),
        () if tagged(node, Z, "seriesEditors") => read_creators(node, "seriesEditor", creators),
        () if tagged(node, LINK, "link") => {
            if let Some(reference) = node.attribute((RDF, "resource")) {
                links.push(reference.to_owned());
            }
        }
        // Zotero's own `dc:subject` tags and its notes are not imported.
        () => {}
    }
}

/// Read a `dcterms:isPartOf` or `bib:presentedAt` container.
///
/// Zotero records a conference paper's DOI, ISBN, and volume on the container
/// rather than on the item, so container values fill any gap the item left.
fn read_container(node: Node<'_, '_>, data: &mut BTreeMap<String, JsonValue>, depth: usize) {
    if depth > MAX_COLLECTION_DEPTH {
        return;
    }
    let series = tagged(node, BIB, "Series");
    let conference = tagged(node, BIB, "Conference");
    for child in node.children().filter(Node::is_element) {
        if tagged(child, DC, "title") {
            let name = if series {
                "series"
            } else if conference {
                "conferenceName"
            } else {
                "publicationTitle"
            };
            if let Some(value) = text(child) {
                data.entry(name.to_owned())
                    .or_insert(JsonValue::String(value));
            }
        } else if tagged(child, DC, "identifier") {
            read_identifier(child, data, if series { "seriesNumber" } else { "number" });
        } else if prism(child, "volume") && !series {
            if let Some(value) = text(child) {
                data.entry("volume".to_owned())
                    .or_insert(JsonValue::String(value));
            }
        } else if prism(child, "number") && !series {
            if let Some(value) = text(child) {
                data.entry("issue".to_owned())
                    .or_insert(JsonValue::String(value));
            }
        } else if tagged(child, DCTERMS, "isPartOf") {
            for nested in child.children().filter(Node::is_element) {
                read_container(nested, data, depth + 1);
            }
        }
    }
}

/// `dc:identifier` carries either a prefixed literal or a nested `dcterms:URI`.
fn read_identifier(node: Node<'_, '_>, data: &mut BTreeMap<String, JsonValue>, bare: &str) {
    if let Some(uri) = descendant(node, DCTERMS, "URI")
        .and_then(|uri| descendant(uri, RDF, "value"))
        .and_then(text)
    {
        data.entry("url".to_owned())
            .or_insert(JsonValue::String(uri));
        return;
    }
    let Some(value) = text(node) else {
        return;
    };
    let name = [
        ("DOI ", "DOI"),
        ("ISBN ", "ISBN"),
        ("ISSN ", "ISSN"),
        ("PMID ", "PMID"),
        ("PMCID ", "PMCID"),
    ]
    .into_iter()
    .find_map(|(prefix, name)| {
        value
            .strip_prefix(prefix)
            .map(|rest| (name, rest.to_owned()))
    });
    match name {
        Some((name, value)) => {
            data.entry(name.to_owned())
                .or_insert(JsonValue::String(value.trim().to_owned()));
        }
        None => {
            data.entry(bare.to_owned())
                .or_insert(JsonValue::String(value));
        }
    }
}

fn read_publisher(node: Node<'_, '_>, data: &mut BTreeMap<String, JsonValue>) {
    if let Some(name) = descendant(node, FOAF, "name").and_then(text) {
        data.entry("publisher".to_owned())
            .or_insert(JsonValue::String(name));
    } else if let Some(name) = text(node) {
        data.entry("publisher".to_owned())
            .or_insert(JsonValue::String(name));
    }
    if let Some(locality) = descendant(node, VCARD, "locality").and_then(text) {
        data.entry("place".to_owned())
            .or_insert(JsonValue::String(locality));
    }
}

fn read_creators(node: Node<'_, '_>, creator_type: &str, creators: &mut Vec<ZoteroCreator>) {
    let Some(sequence) = descendant(node, RDF, "Seq") else {
        return;
    };
    for entry in sequence
        .children()
        .filter(|child| tagged(*child, RDF, "li"))
    {
        let person = entry
            .children()
            .find(|child| child.is_element())
            .unwrap_or(entry);
        let surname = descendant(person, FOAF, "surname").and_then(text);
        let given = descendant(person, FOAF, "givenName").and_then(text);
        let organization = descendant(person, FOAF, "name").and_then(text);
        let (last, first, corporate) = match (surname, organization) {
            (Some(surname), _) => (surname, given.unwrap_or_default(), false),
            (None, Some(name)) => (name, String::new(), true),
            (None, None) => continue,
        };
        creators.push(ZoteroCreator {
            first_name: first,
            last_name: last,
            name: String::new(),
            creator_type: creator_type.to_owned(),
            field_mode: JsonValue::Bool(corporate),
        });
    }
}

fn read_attachment(
    node: Node<'_, '_>,
    base: &Path,
    base_directory: Option<&Path>,
) -> std::result::Result<RdfAttachment, String> {
    let Some(reference) = descendant(node, Z, "path").and_then(|path| {
        path.attribute((RDF, "resource"))
            .map(str::to_owned)
            .or_else(|| text(path))
    }) else {
        return Err("the export contains no file for this attachment".to_owned());
    };
    let decoded = percent_decode(&reference);
    let path = if let Some(linked) = decoded.strip_prefix("attachments:") {
        // Zotero writes this scheme for a linked file it did not copy into the
        // export; it is relative to the linked attachment base directory.
        let Some(root) = base_directory else {
            return Err(format!(
                "{linked} is linked to Zotero's attachment base directory; \
                 re-run with --attachment-base to import it"
            ));
        };
        root.join(linked)
    } else {
        let relative = decoded.strip_prefix("file://").unwrap_or(&decoded);
        let relative = Path::new(relative);
        if relative.is_absolute() {
            relative.to_path_buf()
        } else {
            base.join(relative)
        }
    };
    if !path.is_file() {
        return Err(format!("{} is missing from the export", path.display()));
    }
    Ok(RdfAttachment {
        title: attachment_title(node),
        media_type: descendant(node, LINK, "type").and_then(text),
        path,
    })
}

fn attachment_title(node: Node<'_, '_>) -> String {
    descendant(node, DC, "title")
        .and_then(text)
        .unwrap_or_else(|| "Attachment".to_owned())
}

/// Resolve every collection into a `parent/child` path.
///
/// Returns the collections to apply per member identifier, and every path in
/// the export including collections that only contain other collections.
fn collection_memberships(
    collections: &[Node<'_, '_>],
) -> (HashMap<String, Vec<String>>, Vec<String>) {
    let mut names = HashMap::new();
    let mut parents = HashMap::new();
    let mut members: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in collections {
        let Some(about) = node.attribute((RDF, "about")) else {
            continue;
        };
        let name = descendant(*node, DC, "title")
            .and_then(text)
            .unwrap_or_else(|| "Untitled".to_owned());
        // Lantai splits `keywords` on commas, so a comma cannot survive in a name.
        names.insert(about, name.replace(',', " "));
        members.insert(
            about,
            node.children()
                .filter(|child| tagged(*child, DCTERMS, "hasPart"))
                .filter_map(|child| child.attribute((RDF, "resource")))
                .collect(),
        );
    }
    for (about, parts) in &members {
        for part in parts {
            if names.contains_key(part) {
                parents.insert(*part, *about);
            }
        }
    }

    let mut paths = HashMap::new();
    for about in names.keys() {
        let mut segments = Vec::new();
        let mut cursor = Some(*about);
        while let Some(current) = cursor {
            if segments.len() > MAX_COLLECTION_DEPTH {
                break;
            }
            segments.push(names[current].as_str());
            cursor = parents.get(current).copied();
        }
        segments.reverse();
        paths.insert(*about, segments.join("/"));
    }

    let mut memberships: HashMap<String, Vec<String>> = HashMap::new();
    for (about, parts) in members {
        let Some(path) = paths.get(about) else {
            continue;
        };
        for part in parts {
            if names.contains_key(part) {
                continue;
            }
            memberships
                .entry(part.to_owned())
                .or_default()
                .push(path.clone());
        }
    }
    (
        memberships,
        paths
            .into_values()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    )
}

fn item_type(node: Node<'_, '_>) -> Option<String> {
    node.children()
        .find(|child| tagged(*child, Z, "itemType"))
        .and_then(text)
}

fn tagged(node: Node<'_, '_>, namespace: &str, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node.tag_name().namespace() == Some(namespace)
}

/// PRISM is versioned in the namespace URI, so match the family instead.
fn prism(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node
            .tag_name()
            .namespace()
            .is_some_and(|namespace| namespace.starts_with(PRISM))
}

fn descendant<'a>(node: Node<'a, 'a>, namespace: &str, name: &str) -> Option<Node<'a, 'a>> {
    node.descendants()
        .find(|child| tagged(*child, namespace, name))
}

fn text(node: Node<'_, '_>) -> Option<String> {
    let value = node
        .children()
        .filter(Node::is_text)
        .filter_map(|child| child.text())
        .collect::<String>();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// BibLaTeX page ranges use `--`; Zotero exports typographic dashes.
fn normalize_pages(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\u{2013}' | '\u{2014}' | '\u{2212}' => normalized.push_str("--"),
            character => normalized.push(character),
        }
    }
    normalized
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push(((high << 4) | low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).unwrap_or_else(|_| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zotero::map_item;

    const HEADER: &str = concat!(
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"",
        " xmlns:z=\"http://www.zotero.org/namespaces/export#\"",
        " xmlns:dc=\"http://purl.org/dc/elements/1.1/\"",
        " xmlns:dcterms=\"http://purl.org/dc/terms/\"",
        " xmlns:bib=\"http://purl.org/net/biblio#\"",
        " xmlns:foaf=\"http://xmlns.com/foaf/0.1/\"",
        " xmlns:vcard=\"http://nwalsh.com/rdf/vCard#\"",
        " xmlns:link=\"http://purl.org/rss/1.0/modules/link/\"",
        " xmlns:prism=\"http://prismstandard.org/namespaces/1.2/basic/\">"
    );

    fn document(body: &str) -> String {
        format!("{HEADER}{body}</rdf:RDF>")
    }

    fn parse_body(body: &str) -> RdfImport {
        parse(Path::new("library.rdf"), &document(body), None).unwrap()
    }

    fn field<'a>(item: &'a RdfItem, name: &str) -> Option<&'a str> {
        item.item.data.get(name).and_then(JsonValue::as_str)
    }

    #[test]
    fn conference_papers_take_identifiers_from_their_container() {
        let import = parse_body(concat!(
            "<rdf:Description rdf:about=\"urn:isbn:979-8-4007-1248-7\">",
            "<z:itemType>conferencePaper</z:itemType>",
            "<dcterms:isPartOf><bib:Journal>",
            "<dcterms:isPartOf><bib:Series><dc:title>ASE '24</dc:title></bib:Series></dcterms:isPartOf>",
            "<dc:identifier>ISBN 979-8-4007-1248-7</dc:identifier>",
            "<dc:title>Proceedings of ASE</dc:title>",
            "<dc:identifier>DOI 10.1145/3691620.3695549</dc:identifier>",
            "</bib:Journal></dcterms:isPartOf>",
            "<dc:title>Typed and Confused</dc:title>",
            "<bib:pages>1858\u{2013}1870</bib:pages>",
            "<z:citationKey>tfs-TypedConfused-2024</z:citationKey>",
            "</rdf:Description>",
        ));

        let item = &import.items[0];
        assert_eq!(field(item, "DOI"), Some("10.1145/3691620.3695549"));
        assert_eq!(field(item, "ISBN"), Some("979-8-4007-1248-7"));
        assert_eq!(field(item, "publicationTitle"), Some("Proceedings of ASE"));
        assert_eq!(field(item, "series"), Some("ASE '24"));
        assert_eq!(field(item, "pages"), Some("1858--1870"));

        let mapped = map_item(item.item.clone()).unwrap();
        let fields = mapped.item.fields.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(mapped.item.entry_type, "inproceedings");
        assert_eq!(
            mapped.item.citation_key.as_deref(),
            Some("tfs-TypedConfused-2024")
        );
        assert_eq!(fields["booktitle"].as_str(), "Proceedings of ASE");
        assert_eq!(fields["doi"].as_str(), "10.1145/3691620.3695549");
    }

    #[test]
    fn item_level_values_win_over_container_values() {
        let import = parse_body(concat!(
            "<bib:Article rdf:about=\"#item_1\">",
            "<z:itemType>journalArticle</z:itemType>",
            "<dc:identifier>DOI 10.1/item</dc:identifier>",
            "<prism:volume>9</prism:volume>",
            "<dcterms:isPartOf><bib:Journal>",
            "<dc:identifier>DOI 10.1/container</dc:identifier>",
            "<prism:volume>3</prism:volume>",
            "<prism:number>4</prism:number>",
            "</bib:Journal></dcterms:isPartOf>",
            "<dc:title>Article</dc:title>",
            "</bib:Article>",
        ));

        let item = &import.items[0];
        assert_eq!(field(item, "DOI"), Some("10.1/item"));
        assert_eq!(field(item, "volume"), Some("9"));
        assert_eq!(field(item, "issue"), Some("4"));
    }

    #[test]
    fn nested_collections_become_path_tags_and_zotero_tags_are_dropped() {
        let import = parse_body(concat!(
            "<bib:Article rdf:about=\"#item_1\">",
            "<z:itemType>journalArticle</z:itemType>",
            "<dc:title>Tagged</dc:title>",
            "<dc:subject><z:AutomaticTag><rdf:value>Computer Science</rdf:value></z:AutomaticTag></dc:subject>",
            "<dc:subject>manual</dc:subject>",
            "</bib:Article>",
            "<z:Collection rdf:about=\"#collection_1\">",
            "<dc:title>ResearchTopics</dc:title>",
            "<dcterms:hasPart rdf:resource=\"#collection_2\"/>",
            "</z:Collection>",
            "<z:Collection rdf:about=\"#collection_2\">",
            "<dc:title>Subtyping</dc:title>",
            "<dcterms:hasPart rdf:resource=\"#collection_3\"/>",
            "</z:Collection>",
            "<z:Collection rdf:about=\"#collection_3\">",
            "<dc:title>SemanticSubtyping</dc:title>",
            "<dcterms:hasPart rdf:resource=\"#item_1\"/>",
            "</z:Collection>",
            "<z:Collection rdf:about=\"#collection_4\">",
            "<dc:title>Inbox</dc:title>",
            "<dcterms:hasPart rdf:resource=\"#item_1\"/>",
            "</z:Collection>",
        ));

        assert_eq!(
            import.collections,
            [
                "Inbox",
                "ResearchTopics",
                "ResearchTopics/Subtyping",
                "ResearchTopics/Subtyping/SemanticSubtyping",
            ]
        );
        let mapped = map_item(import.items[0].item.clone()).unwrap();
        let fields = mapped.item.fields.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(
            fields["keywords"].as_str(),
            "Inbox, ResearchTopics/Subtyping/SemanticSubtyping"
        );
        assert!(!fields["keywords"].contains("Computer Science"));
        assert!(!fields["keywords"].contains("manual"));
    }

    #[test]
    fn creators_keep_their_roles_and_brace_organizations() {
        let import = parse_body(concat!(
            "<bib:BookSection rdf:about=\"#item_1\">",
            "<z:itemType>bookSection</z:itemType>",
            "<dc:title>Chapter</dc:title>",
            "<bib:authors><rdf:Seq>",
            "<rdf:li><foaf:Person><foaf:surname>Toro</foaf:surname><foaf:givenName>Mat\u{ed}as</foaf:givenName></foaf:Person></rdf:li>",
            "<rdf:li><foaf:Organization><foaf:name>ACM</foaf:name></foaf:Organization></rdf:li>",
            "</rdf:Seq></bib:authors>",
            "<bib:editors><rdf:Seq>",
            "<rdf:li><foaf:Person><foaf:surname>Ranzato</foaf:surname><foaf:givenName>Francesco</foaf:givenName></foaf:Person></rdf:li>",
            "</rdf:Seq></bib:editors>",
            "<dc:publisher><foaf:Organization>",
            "<vcard:adr><vcard:Address><vcard:locality>Cham</vcard:locality></vcard:Address></vcard:adr>",
            "<foaf:name>Springer</foaf:name>",
            "</foaf:Organization></dc:publisher>",
            "</bib:BookSection>",
        ));

        let item = &import.items[0];
        assert_eq!(field(item, "publisher"), Some("Springer"));
        assert_eq!(field(item, "place"), Some("Cham"));

        let mapped = map_item(item.item.clone()).unwrap();
        let fields = mapped.item.fields.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(mapped.item.entry_type, "incollection");
        assert_eq!(fields["author"].as_str(), "Toro, Mat\u{ed}as and {ACM}");
        assert_eq!(fields["editor"].as_str(), "Ranzato, Francesco");
        assert_eq!(fields["location"].as_str(), "Cham");
    }

    #[test]
    fn attachments_resolve_relative_paths_and_report_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let files = directory.path().join("files").join("200");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("a b.pdf"), b"%PDF-1.4\n").unwrap();
        let rdf = directory.path().join("library.rdf");

        let import = parse(
            &rdf,
            &document(concat!(
                "<bib:Article rdf:about=\"#item_1\">",
                "<z:itemType>journalArticle</z:itemType>",
                "<dc:title>Linked</dc:title>",
                "<z:citationKey>linked-2024</z:citationKey>",
                "<link:link rdf:resource=\"#item_200\"/>",
                "<link:link rdf:resource=\"#item_201\"/>",
                "</bib:Article>",
                "<z:Attachment rdf:about=\"#item_200\">",
                "<z:itemType>attachment</z:itemType>",
                "<z:path rdf:resource=\"files/200/a%20b.pdf\"/>",
                "<dc:title>Full Text PDF</dc:title>",
                "<z:linkMode>2</z:linkMode>",
                "<link:type>application/pdf</link:type>",
                "</z:Attachment>",
                "<z:Attachment rdf:about=\"#item_201\">",
                "<z:itemType>attachment</z:itemType>",
                "<dc:title>Google Books Link</dc:title>",
                "<z:linkMode>3</z:linkMode>",
                "<link:type>text/html</link:type>",
                "</z:Attachment>",
            )),
            None,
        )
        .unwrap();

        assert_eq!(import.items.len(), 1);
        let attachments = &import.items[0].attachments;
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, files.join("a b.pdf"));
        assert_eq!(attachments[0].title, "Full Text PDF");
        assert_eq!(
            attachments[0].media_type.as_deref(),
            Some("application/pdf")
        );

        assert_eq!(import.skipped.len(), 1);
        assert_eq!(import.skipped[0].item, "linked-2024");
        assert_eq!(import.skipped[0].title, "Google Books Link");
    }

    #[test]
    fn containers_and_attachments_are_never_imported_as_items() {
        let import = parse_body(concat!(
            "<bib:Journal rdf:about=\"urn:issn:1234-5678\">",
            "<dc:title>Hoisted Container</dc:title>",
            "</bib:Journal>",
            "<z:Attachment rdf:about=\"#item_9\">",
            "<z:itemType>attachment</z:itemType>",
            "<dc:title>Snapshot</dc:title>",
            "</z:Attachment>",
            "<bib:Book rdf:about=\"#item_1\">",
            "<z:itemType>book</z:itemType>",
            "<dc:title>Real Item</dc:title>",
            "</bib:Book>",
        ));

        assert_eq!(import.items.len(), 1);
        assert_eq!(import.items[0].item.item_type, "book");
    }

    #[test]
    fn thesis_type_and_series_number_reach_the_zotero_mapping() {
        let import = parse_body(concat!(
            "<bib:Thesis rdf:about=\"#item_1\">",
            "<z:itemType>thesis</z:itemType>",
            "<dc:title>Dissertation</dc:title>",
            "<z:type>Doctoral dissertation</z:type>",
            "<dc:publisher><foaf:Organization><foaf:name>Northeastern</foaf:name></foaf:Organization></dc:publisher>",
            "<dcterms:isPartOf><bib:Series>",
            "<dc:title>Monographs</dc:title>",
            "<dc:identifier>79</dc:identifier>",
            "</bib:Series></dcterms:isPartOf>",
            "</bib:Thesis>",
        ));

        let item = &import.items[0];
        assert_eq!(field(item, "thesisType"), Some("Doctoral dissertation"));
        assert_eq!(field(item, "seriesNumber"), Some("79"));

        let mapped = map_item(item.item.clone()).unwrap();
        let fields = mapped.item.fields.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(fields["institution"].as_str(), "Northeastern");
        assert_eq!(fields["type"].as_str(), "Doctoral dissertation");
        assert_eq!(fields["number"].as_str(), "79");
    }

    #[test]
    fn localized_dates_become_iso() {
        let import = parse_body(concat!(
            "<bib:Book rdf:about=\"#item_1\">",
            "<z:itemType>book</z:itemType>",
            "<dc:title>Locale Rendered</dc:title>",
            "<dc:date>October 27, 2024</dc:date>",
            "<dcterms:dateSubmitted>\u{5341}\u{6708} 12, 2017</dcterms:dateSubmitted>",
            "</bib:Book>",
        ));

        let item = &import.items[0];
        assert_eq!(field(item, "date"), Some("2024-10-27"));
        assert_eq!(field(item, "accessDate"), Some("2017-10-12"));
    }

    #[test]
    fn an_export_without_items_is_rejected() {
        let error = parse(
            Path::new("library.rdf"),
            &document("<z:Collection rdf:about=\"#collection_1\"><dc:title>Inbox</dc:title></z:Collection>"),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::ZoteroRdfHasNoItems { .. }));
    }

    #[test]
    fn malformed_xml_is_rejected() {
        let error = parse(Path::new("library.rdf"), "<rdf:RDF>", None).unwrap_err();
        assert!(matches!(error, Error::ParseZoteroRdf { .. }));
    }
}
