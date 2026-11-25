//! The fallback dependency source: issue references listed under a
//! `Dependencies / Blockers` heading in the issue body.

use std::sync::LazyLock;

use regex::Regex;

use crate::model::IssueRef;

static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$").unwrap());
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        https://github\.com/(?P<uo>[A-Za-z0-9-]+)/(?P<ur>[A-Za-z0-9._-]+)/(?:issues|pull)/(?P<un>\d+)
        | (?:^|[^A-Za-z0-9_/\#&.:-])(?:(?P<o>[A-Za-z0-9-]+)/(?P<r>[A-Za-z0-9._-]+))?\#(?P<n>\d+)\b",
    )
    .unwrap()
});

/// A section that opens with this declares no blockers: "None.", "n/a",
/// "No blockers", "Keine". References after it are context, such as a
/// workaround elsewhere, not something to wait for.
static NONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:[-*]\s+)?[_*]*(?:none|n/a|no\s+(?:blockers?|dependencies)|nothing|keine)[_*]*\s*(?:[.,:;!()\u{2014}-]|$)",
    )
    .unwrap()
});

/// True for the heading the issue template uses, in any capitalization and
/// with `/`, `&` or `and` between the words.
fn is_dependencies_heading(text: &str) -> bool {
    let t = text.to_lowercase();
    let words: Vec<&str> = t
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && *w != "and")
        .collect();
    words == ["dependencies", "blockers"]
        || words == ["dependencies"]
        || words == ["blockers"]
        || words == ["blocked", "by"]
}

/// Issue references under the `Dependencies / Blockers` heading, in order,
/// without duplicates. Bare `#123` refers to `repo`.
pub fn dependency_refs(body: &str, repo: &str) -> Vec<IssueRef> {
    let mut out: Vec<IssueRef> = Vec::new();
    let mut in_section = false;
    let mut section_level = 0;
    let mut in_fence = false;
    // The section's first line of text decides whether it declares "none".
    let mut seen_text = false;
    let mut declared_none = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && let Some(c) = HEADING.captures(line) {
            let level = c[1].len();
            if in_section && level <= section_level {
                in_section = false;
            }
            if !in_section && is_dependencies_heading(&c[2]) {
                in_section = true;
                section_level = level;
                seen_text = false;
                declared_none = false;
                continue;
            }
        }
        if !in_section || in_fence {
            continue;
        }
        if !seen_text && !line.trim().is_empty() && !HEADING.is_match(line) {
            seen_text = true;
            declared_none = NONE.is_match(line);
        }
        if declared_none {
            continue;
        }
        for c in REFERENCE.captures_iter(line) {
            let r = if let Some(n) = c.name("un") {
                IssueRef::new(
                    format!("{}/{}", &c["uo"], &c["ur"]),
                    n.as_str().parse().unwrap(),
                )
            } else {
                let n: u64 = c["n"].parse().unwrap();
                match (c.name("o"), c.name("r")) {
                    (Some(o), Some(r)) => {
                        IssueRef::new(format!("{}/{}", o.as_str(), r.as_str()), n)
                    }
                    _ => IssueRef::new(repo, n),
                }
            };
            if !out.iter().any(|x| x.key() == r.key()) {
                out.push(r);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(body: &str) -> Vec<String> {
        dependency_refs(body, "acme/app")
            .iter()
            .map(|r| r.to_string())
            .collect()
    }

    #[test]
    fn only_the_dependencies_section_counts() {
        let body = "### Context\n\nSee #1 and acme/web#2.\n\n### Dependencies / Blockers\n\n- #10\n- acme/web#11 (needs the API)\n- https://github.com/acme/ops/issues/12\n\n### Acceptance criteria\n\n- like #13\n";
        assert_eq!(refs(body), ["acme/app#10", "acme/web#11", "acme/ops#12"]);
    }

    #[test]
    fn no_section_no_refs() {
        assert!(refs("Blocked by #4, see the dependencies.").is_empty());
    }

    #[test]
    fn subheadings_stay_in_the_section() {
        let body = "## Dependencies / Blockers\n#### Hard\n- #5\n## Next\n- #6";
        assert_eq!(refs(body), ["acme/app#5"]);
    }

    #[test]
    fn ignores_code_fences_anchors_and_duplicates() {
        let body = "### Dependencies / Blockers\n```\n#7\n```\n- #8, #8, https://x.org/page#9, a&#10;\n_No response_\n";
        assert_eq!(refs(body), ["acme/app#8"]);
    }

    #[test]
    fn a_section_declaring_none_has_no_blockers() {
        for none in [
            "None. The console has a workaround in acme/web#611, which hides it.",
            "None",
            "- None (see #5 for background)",
            "_None_",
            "n/a, but acme/web#3 is related",
            "No blockers: #4 only touches the same file.",
            "Keine. Siehe #9.",
        ] {
            let body =
                format!("### Dependencies / Blockers\n\n{none}\n- #12\n\n### Notes\n- #13\n");
            assert!(refs(&body).is_empty(), "{none}");
        }
        // Only an opening "none" counts, and only for its own section.
        let body = "### Dependencies / Blockers\n- #20\n- None of #21's work is needed\n";
        assert_eq!(refs(body), ["acme/app#20", "acme/app#21"]);
        let body = "### Dependencies / Blockers\nNone.\n## Context\n### Blocked by\n- #22\n";
        assert_eq!(refs(body), ["acme/app#22"]);
        let body = "### Dependencies / Blockers\nNoneworthy #23\n";
        assert_eq!(refs(body), ["acme/app#23"]);
    }

    #[test]
    fn heading_variants() {
        assert!(is_dependencies_heading("Dependencies / Blockers"));
        assert!(is_dependencies_heading("dependencies & blockers"));
        assert!(is_dependencies_heading("Blocked by"));
        assert!(!is_dependencies_heading("Dependencies to update"));
    }
}
