//! Links from the repository's own Markdown (FEATURES.md, README.md, CHANGELOG.md, AGENTS.md, the examples' READMEs)
//! into the book and the tree: each target file exists, and an anchor into a book page names one of its headings.
//! `vz check` and the book's strict build check the links inside docs/; these files lie outside the book.

use std::path::{Path, PathBuf};

/// The repository root, above this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// The Markdown files outside docs/ whose links are checked.
fn sources() -> Vec<PathBuf> {
    let root = root();
    let mut files: Vec<PathBuf> = ["FEATURES.md", "README.md", "CHANGELOG.md", "AGENTS.md"]
        .iter()
        .map(|f| root.join(f))
        .collect();
    for entry in std::fs::read_dir(root.join("examples")).unwrap() {
        let readme = entry.unwrap().path().join("README.md");
        if readme.is_file() {
            files.push(readme);
        }
    }
    files
}

/// The targets of the inline links `[text](target)` in `text`, outside code spans and fences.
fn link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // drop code spans, whose brackets are no links
        let plain: String = line.split('`').step_by(2).collect::<Vec<_>>().join("");
        let mut rest = plain.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let Some(end) = after.find(')') else { break };
            targets.push(after[..end].to_owned());
            rest = &after[end..];
        }
    }
    targets
}

/// The text a reader sees of a heading's Markdown: links, glossary marks, code and emphasis reduced to their words.
fn heading_text(md: &str) -> String {
    let mut out = String::new();
    let mut chars = md.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `[[term|shown]]` shows `shown`, `[[term]]` shows `term`
            '[' if chars.peek() == Some(&'[') => {
                chars.next();
                let inner: String = chars.by_ref().take_while(|&c| c != ']').collect();
                chars.next();
                out.push_str(inner.rsplit('|').next().unwrap_or(&inner));
            }
            // `[text](target)` shows `text`
            '[' => {
                let inner: String = chars.by_ref().take_while(|&c| c != ']').collect();
                if chars.peek() == Some(&'(') {
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&inner);
            }
            '`' | '*' => {}
            c => out.push(c),
        }
    }
    out
}

/// The anchor Python-Markdown's `toc` gives a heading: ASCII word characters, spaces and hyphens kept, lowercased,
/// runs of spaces and hyphens one hyphen.
fn slug(text: &str) -> String {
    let kept: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || c.is_whitespace())
        .collect::<String>()
        .trim()
        .to_lowercase();
    let mut slug = String::new();
    let mut gap = false;
    for c in kept.chars() {
        if c == '-' || c.is_whitespace() {
            gap = true;
        } else {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c);
        }
    }
    slug
}

/// The anchors of a book page: each heading's slug, or its `{#id}`.
fn anchors(page: &str) -> Vec<String> {
    let mut anchors = Vec::new();
    let mut fenced = false;
    for line in page.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim();
        // an attribute list `{#id}` or `{ #id }` names the anchor itself
        let id = heading.rfind('{').and_then(|at| {
            heading[at + 1..]
                .trim_end_matches('}')
                .trim()
                .strip_prefix('#')
                .map(str::to_owned)
        });
        anchors.push(id.unwrap_or_else(|| slug(&heading_text(heading))));
    }
    anchors
}

#[test]
fn heading_slugs_follow_the_build() {
    // headings of the book as the build anchors them
    for (heading, anchor) in [
        ("The 4 KiB end and cloning", "the-4-kib-end-and-cloning"),
        ("What a track's items are", "what-a-tracks-items-are"),
        ("Near, puts and copies", "near-puts-and-copies"),
        (
            "Grids: where samples are in time",
            "grids-where-samples-are-in-time",
        ),
        ("Definitions on [[track]] 0", "definitions-on-track-0"),
        (
            "`TapeReader::open` from the tails",
            "tapereaderopen-from-the-tails",
        ),
        ("5.5 Deferred blocks {#deferred-blocks}", "deferred-blocks"),
        (
            "The kinds of item { #what-a-tracks-items-are }",
            "what-a-tracks-items-are",
        ),
    ] {
        let line = format!("## {heading}");
        assert_eq!(anchors(&line), [anchor], "{heading}");
    }
}

#[test]
fn links_outside_the_book_resolve() {
    let root = root();
    let mut dead = Vec::new();
    for source in sources() {
        let text = std::fs::read_to_string(&source).unwrap();
        let dir = source.parent().unwrap();
        for target in link_targets(&text) {
            let target = target.split_whitespace().next().unwrap_or_default();
            if target.contains("://") || target.starts_with("mailto:") || target.is_empty() {
                continue;
            }
            let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
            let name = source.strip_prefix(&root).unwrap_or(&source).display();
            if path.is_empty() {
                continue; // an anchor into the file itself: GitHub's, not the book's
            }
            let file = dir.join(path);
            if !file.exists() {
                dead.push(format!("{name}: {target} (no such file)"));
                continue;
            }
            let in_book = file
                .canonicalize()
                .unwrap()
                .starts_with(root.join("docs").canonicalize().unwrap());
            if !anchor.is_empty() && in_book && file.extension().is_some_and(|e| e == "md") {
                let page = std::fs::read_to_string(&file).unwrap();
                if !anchors(&page).iter().any(|a| a == anchor) {
                    dead.push(format!("{name}: {target} (no such heading)"));
                }
            }
        }
    }
    assert!(dead.is_empty(), "dead links:\n{}", dead.join("\n"));
}
