//! Path scopes: which files a task may change, and whether two tasks may run
//! at the same time without touching the same files.

use globset::{Glob, GlobSetBuilder};

/// Paths in `changed` that no pattern in `write` allows. Empty means the
/// change stays inside the scope.
pub fn violations(write: &[String], changed: &[String]) -> Vec<String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in write {
        // Patterns are validated when a task is created, so this does not fail
        // for stored tasks. Skip anything invalid rather than panic.
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    let set = match builder.build() {
        Ok(set) => set,
        Err(_) => return changed.to_vec(),
    };
    changed
        .iter()
        .filter(|path| !set.is_match(path.as_str()))
        .cloned()
        .collect()
}

/// Conservative overlap test: true when any write pattern of `a` could match a
/// path that a pattern of `b` could also match. It may report an overlap that
/// does not really exist, but it never misses one.
pub fn overlaps(a: &[String], b: &[String]) -> bool {
    a.iter().any(|p| {
        b.iter()
            .any(|q| prefixes_overlap(&static_prefix(p), &static_prefix(q)))
    })
}

/// The directory part of a glob before its first wildcard, without a trailing
/// slash. `src/upload/**` gives `src/upload`; `*.rs` gives the empty string.
fn static_prefix(pattern: &str) -> String {
    let literal_end = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    let literal = &pattern[..literal_end];
    if literal_end == pattern.len() {
        // No wildcard: the pattern names one file.
        literal.to_string()
    } else {
        // Keep only whole path components.
        match literal.rfind('/') {
            Some(idx) => literal[..idx].to_string(),
            None => String::new(),
        }
    }
}

fn prefixes_overlap(p: &str, q: &str) -> bool {
    let p = p.trim_end_matches('/');
    let q = q.trim_end_matches('/');
    p.is_empty()
        || q.is_empty()
        || p == q
        || q.starts_with(&format!("{p}/"))
        || p.starts_with(&format!("{q}/"))
}

/// Rejects patterns that would not compile as globs.
pub fn validate_patterns(patterns: &[String]) -> Result<(), String> {
    for pattern in patterns {
        if pattern.trim().is_empty() {
            return Err("empty path pattern".to_string());
        }
        Glob::new(pattern).map_err(|e| format!("invalid pattern {pattern:?}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn violations_lists_paths_outside_write_scope() {
        let write = s(&["src/upload/**", "tests/upload/**"]);
        let changed = s(&["src/upload/client.rs", "src/main.rs", "tests/upload/a.rs"]);
        assert_eq!(violations(&write, &changed), s(&["src/main.rs"]));
    }

    #[test]
    fn empty_write_scope_allows_nothing() {
        assert_eq!(violations(&[], &s(&["a.txt"])), s(&["a.txt"]));
    }

    #[test]
    fn sibling_directories_do_not_overlap() {
        assert!(!overlaps(&s(&["src/upload/**"]), &s(&["src/billing/**"])));
    }

    #[test]
    fn nested_directories_overlap() {
        assert!(overlaps(&s(&["src/**"]), &s(&["src/upload/**"])));
        assert!(overlaps(
            &s(&["src/upload/**"]),
            &s(&["src/upload/client.rs"])
        ));
    }

    #[test]
    fn name_prefix_is_not_a_directory_overlap() {
        // "src/up" is not a parent of "src/upload".
        assert!(!overlaps(&s(&["src/up/**"]), &s(&["src/upload/**"])));
    }

    #[test]
    fn wildcard_in_root_overlaps_everything() {
        assert!(overlaps(&s(&["*.md"]), &s(&["docs/**"])));
    }

    #[test]
    fn validate_rejects_bad_glob() {
        assert!(validate_patterns(&s(&["src/[a"])).is_err());
        assert!(validate_patterns(&s(&["src/**"])).is_ok());
        assert!(validate_patterns(&s(&[""])).is_err());
    }
}
