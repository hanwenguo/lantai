//! A derived, read-only collection view over tags.
//!
//! Lantai stores no collection model. The Zotero Connector's save popup renders
//! a collection tree, so this module reshapes the library's tags into the flat,
//! depth-first target list that popup expects: a tag containing `/` nests, and
//! choosing a target applies that tag path to the saved item.

use std::collections::BTreeMap;

/// One row of the Connector's save-target picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Connector target identifier, stable for a given path.
    pub id: String,
    /// Leaf name shown in the picker.
    pub name: String,
    /// Indent depth; the library root is level 0, so collections start at 1.
    pub level: usize,
    /// Full tag applied when this target is chosen.
    pub path: String,
}

/// The Connector target identifier for the library root.
pub const LIBRARY_TARGET: &str = "L1";

/// Build the picker rows for a tag set.
///
/// The Connector finds a row's parent by scanning backwards for the first row
/// one level shallower, so every ancestor must be present and each parent must
/// immediately precede its children. Ancestors that no item is tagged with are
/// synthesized: an imported library commonly holds `Projects/IfT` without
/// holding `Projects` itself.
pub fn tree(tags: impl IntoIterator<Item = String>) -> Vec<Target> {
    let mut roots = Node::default();
    for tag in tags {
        let mut node = &mut roots;
        let mut path = String::new();
        for segment in tag.split('/').map(str::trim).filter(|s| !s.is_empty()) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(segment);
            node = node
                .children
                .entry(sort_key(segment))
                .or_insert_with(|| Node {
                    name: segment.to_owned(),
                    path: path.clone(),
                    children: BTreeMap::new(),
                });
        }
    }

    let mut targets = Vec::new();
    flatten(&roots, 1, &mut targets);
    targets
}

/// Resolve a picker identifier back to the tag it applies.
///
/// Recomputing from the current tags keeps identifiers meaningful without
/// server-side state, even if the library changed while the popup was open.
pub fn resolve(tags: impl IntoIterator<Item = String>, id: &str) -> Option<String> {
    tree(tags)
        .into_iter()
        .find(|target| target.id == id)
        .map(|target| target.path)
}

/// Derive a target identifier from its path.
///
/// Zotero uses `C<collection id>`, and the Connector treats any identifier that
/// does not start with `L` as a collection. Hashing the path rather than
/// numbering rows keeps the identifier stable when unrelated tags appear
/// between the popup opening and its update.
fn target_id(path: &str) -> String {
    let hash = blake3::hash(path.as_bytes());
    let bytes: [u8; 4] = hash.as_bytes()[..4]
        .try_into()
        .expect("blake3 digests are 32 bytes");
    format!("C{}", u32::from_be_bytes(bytes))
}

#[derive(Default)]
struct Node {
    name: String,
    path: String,
    children: BTreeMap<(String, String), Node>,
}

/// Order siblings case-insensitively, keeping distinct spellings apart.
fn sort_key(segment: &str) -> (String, String) {
    (segment.to_lowercase(), segment.to_owned())
}

fn flatten(node: &Node, level: usize, targets: &mut Vec<Target>) {
    for child in node.children.values() {
        targets.push(Target {
            id: target_id(&child.path),
            name: child.name.clone(),
            level,
            path: child.path.clone(),
        });
        flatten(child, level + 1, targets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(tags: &[&str]) -> Vec<(String, usize, String)> {
        tree(tags.iter().map(|tag| (*tag).to_owned()))
            .into_iter()
            .map(|target| (target.name, target.level, target.path))
            .collect()
    }

    #[test]
    fn missing_ancestors_are_synthesized_and_precede_their_children() {
        // Neither "Projects" nor "ResearchTopics" is itself a tag.
        let rows = rows(&[
            "Projects/IfT",
            "ResearchTopics/Subtyping/SemanticSubtyping",
            "ResearchTopics/Subtyping",
            "Inbox",
        ]);
        assert_eq!(
            rows,
            [
                ("Inbox".to_owned(), 1, "Inbox".to_owned()),
                ("Projects".to_owned(), 1, "Projects".to_owned()),
                ("IfT".to_owned(), 2, "Projects/IfT".to_owned()),
                ("ResearchTopics".to_owned(), 1, "ResearchTopics".to_owned()),
                (
                    "Subtyping".to_owned(),
                    2,
                    "ResearchTopics/Subtyping".to_owned()
                ),
                (
                    "SemanticSubtyping".to_owned(),
                    3,
                    "ResearchTopics/Subtyping/SemanticSubtyping".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn every_row_has_a_parent_one_level_shallower() {
        let targets = tree(
            ["a/b/c/d", "a/x", "z", "m/n"]
                .into_iter()
                .map(str::to_owned),
        );
        for (index, target) in targets.iter().enumerate() {
            if target.level == 1 {
                continue;
            }
            let parent = targets[..index]
                .iter()
                .rev()
                .find(|candidate| candidate.level == target.level - 1)
                .expect("a deeper row always follows its parent");
            assert!(
                target.path.starts_with(&format!("{}/", parent.path)),
                "{} is not under {}",
                target.path,
                parent.path
            );
        }
    }

    #[test]
    fn flat_tags_are_ordered_case_insensitively() {
        assert_eq!(
            rows(&["zeta", "Alpha", "beta"])
                .into_iter()
                .map(|(name, _, _)| name)
                .collect::<Vec<_>>(),
            ["Alpha", "beta", "zeta"]
        );
    }

    #[test]
    fn identifiers_are_stable_when_unrelated_tags_appear() {
        let before = tree(["Projects/IfT".to_owned()]);
        let after = tree(["Aardvark".to_owned(), "Projects/IfT".to_owned()]);
        let find = |targets: &[Target], path: &str| {
            targets
                .iter()
                .find(|target| target.path == path)
                .expect("path present")
                .id
                .clone()
        };
        assert_eq!(find(&before, "Projects/IfT"), find(&after, "Projects/IfT"));
        assert_ne!(find(&after, "Aardvark"), find(&after, "Projects/IfT"));
    }

    #[test]
    fn identifiers_never_collide_with_the_library_root() {
        let targets = tree(
            (0..500)
                .map(|index| format!("tag-{index}"))
                .collect::<Vec<_>>(),
        );
        assert!(targets.iter().all(|target| target.id != LIBRARY_TARGET));
        assert!(targets.iter().all(|target| !target.id.starts_with('L')));
        let unique = targets
            .iter()
            .map(|target| target.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), targets.len(), "identifiers collided");
    }

    #[test]
    fn resolve_round_trips_every_row() {
        let tags = ["Inbox", "Projects/IfT", "a/b/c"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for target in tree(tags.clone()) {
            assert_eq!(
                resolve(tags.clone(), &target.id).as_deref(),
                Some(target.path.as_str())
            );
        }
        assert_eq!(resolve(tags, "C1"), None);
    }

    #[test]
    fn empty_and_malformed_segments_are_dropped() {
        assert!(tree(["".to_owned(), "/".to_owned(), "  ".to_owned()]).is_empty());
        assert_eq!(
            rows(&["/leading", "trailing/", "a//b"]),
            [
                ("a".to_owned(), 1, "a".to_owned()),
                ("b".to_owned(), 2, "a/b".to_owned()),
                ("leading".to_owned(), 1, "leading".to_owned()),
                ("trailing".to_owned(), 1, "trailing".to_owned()),
            ]
        );
    }
}
