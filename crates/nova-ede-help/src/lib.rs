//! Rust port of `ede-help`, EDE's help browser launcher.
//!
//! Upstream keeps a catalog of help titles (one HTML page per title),
//! opens the start page when invoked bare, lists the titles with
//! `--titles`, and picks a browser from `$BROWSER` or a fallback list.
//! The catalog and the browser selection are pure policy here: the
//! shell supplies the environment and runs the chosen browser.

#![no_std]
#![forbid(unsafe_code)]

/// How many help titles one catalog holds.
pub const MAX_TITLES: usize = 32;
/// The longest title kept.
pub const TITLE_MAX: usize = 32;
/// How many browser candidates the fallback list holds.
pub const MAX_BROWSERS: usize = 6;

/// The catalog of help titles, in display order.
#[derive(Clone, Copy)]
pub struct HelpCatalog {
    titles: [[u8; TITLE_MAX]; MAX_TITLES],
    title_lens: [usize; MAX_TITLES],
    count: usize,
}

impl HelpCatalog {
    /// An empty catalog.
    pub const fn new() -> Self {
        Self {
            titles: [[0; TITLE_MAX]; MAX_TITLES],
            title_lens: [0; MAX_TITLES],
            count: 0,
        }
    }

    /// The catalog as upstream's `ede-help --titles` prints it: the
    /// titles sorted, one per line.
    pub fn novaos() -> Self {
        let mut catalog = Self::new();
        for title in [
            "about",
            "calc",
            "desktop",
            "index",
            "introduction",
            "panel",
            "session",
            "tip",
        ] {
            catalog.add(title);
        }
        catalog
    }

    /// Add a title; duplicates and overflow are ignored.
    pub fn add(&mut self, title: &str) -> bool {
        let bytes = title.as_bytes();
        if bytes.is_empty() || bytes.len() > TITLE_MAX || self.count >= MAX_TITLES {
            return false;
        }
        let existing = (0..self.count).any(|index| {
            self.title_lens[index] == bytes.len()
                && &self.titles[index][..self.title_lens[index]] == bytes
        });
        if existing {
            return true;
        }
        self.titles[self.count][..bytes.len()].copy_from_slice(bytes);
        self.title_lens[self.count] = bytes.len();
        self.count += 1;
        true
    }

    /// How many titles the catalog holds.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The title at `index`, in insertion order.
    pub fn title(&self, index: usize) -> Option<&str> {
        if index >= self.count {
            return None;
        }
        core::str::from_utf8(&self.titles[index][..self.title_lens[index]]).ok()
    }

    /// Whether a help page exists for `title`.
    pub fn has(&self, title: &str) -> bool {
        (0..self.count).any(|index| self.title(index) == Some(title))
    }
}

impl Default for HelpCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// Which document the help command opens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelpTarget {
    /// The bare invocation: the start page.
    StartPage,
    /// A specific title's page.
    Page(usize),
    /// `--titles`: the catalog listing, shown on stdout.
    ListTitles,
}

/// Resolve the command line into a target. Unknown titles fall back to
/// the start page rather than failing — upstream's behaviour.
pub fn resolve(catalog: &HelpCatalog, argument: Option<&str>) -> HelpTarget {
    match argument {
        None => HelpTarget::StartPage,
        Some("--titles") | Some("-t") => HelpTarget::ListTitles,
        Some(title) => {
            match (0..catalog.count()).find(|&index| catalog.title(index) == Some(title)) {
                Some(index) => HelpTarget::Page(index),
                None => HelpTarget::StartPage,
            }
        }
    }
}

/// The document path for a target, relative to the help root.
pub fn document_path(catalog: &HelpCatalog, target: HelpTarget) -> Option<&'static str> {
    match target {
        HelpTarget::StartPage => Some("index.html"),
        HelpTarget::ListTitles => None,
        HelpTarget::Page(index) => {
            if index >= catalog.count() {
                return None;
            }
            // One page per title; the name is the title itself.
            match catalog.title(index) {
                Some("about") => Some("about.html"),
                Some("calc") => Some("calc.html"),
                Some("desktop") => Some("desktop.html"),
                Some("index") => Some("index.html"),
                Some("introduction") => Some("introduction.html"),
                Some("panel") => Some("panel.html"),
                Some("session") => Some("session.html"),
                Some("tip") => Some("tip.html"),
                _ => None,
            }
        }
    }
}

/// Pick the browser: the `$BROWSER` candidate first, then the
/// fallback list in order. Returns the index into `candidates`.
pub fn pick_browser(candidates: &[&str]) -> Option<usize> {
    // `candidates` is already in preference order — the caller put
    // $BROWSER first when it was set. Pick the first non-empty one.
    candidates
        .iter()
        .position(|candidate| !candidate.is_empty())
        .filter(|index| candidates.len() > *index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_page_is_the_default() {
        let catalog = HelpCatalog::novaos();
        let target = resolve(&catalog, None);
        assert_eq!(target, HelpTarget::StartPage);
        assert_eq!(document_path(&catalog, target), Some("index.html"));
    }

    #[test]
    fn titles_resolve_to_their_pages() {
        let catalog = HelpCatalog::novaos();
        assert!(catalog.has("about"));
        let target = resolve(&catalog, Some("calc"));
        assert_eq!(target, HelpTarget::Page(1));
        assert_eq!(document_path(&catalog, target), Some("calc.html"));
    }

    #[test]
    fn an_unknown_title_falls_back_to_the_start_page() {
        let catalog = HelpCatalog::novaos();
        let target = resolve(&catalog, Some("nonexistent"));
        assert_eq!(target, HelpTarget::StartPage);
    }

    #[test]
    fn titles_flag_lists_the_catalog() {
        let catalog = HelpCatalog::novaos();
        assert_eq!(resolve(&catalog, Some("--titles")), HelpTarget::ListTitles);
        assert_eq!(resolve(&catalog, Some("-t")), HelpTarget::ListTitles);
        assert_eq!(document_path(&catalog, HelpTarget::ListTitles), None);
    }

    #[test]
    fn the_catalog_lists_titles_in_order() {
        let catalog = HelpCatalog::novaos();
        assert_eq!(catalog.count(), 8);
        assert_eq!(catalog.title(0), Some("about"));
        assert_eq!(catalog.title(7), Some("tip"));
        assert_eq!(catalog.title(8), None);
    }

    #[test]
    fn duplicates_are_ignored() {
        let mut catalog = HelpCatalog::new();
        assert!(catalog.add("about"));
        assert!(catalog.add("about"));
        assert_eq!(catalog.count(), 1);
    }

    #[test]
    fn browser_candidates_are_picked_in_order() {
        assert_eq!(
            pick_browser(&["", "firefox", "epiphany"]),
            Some(1),
            "empty $BROWSER slots are skipped"
        );
        assert_eq!(pick_browser(&[]), None);
    }
}
