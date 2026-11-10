//! Rust port of `ede-about`: the About dialog for NovaOS's EDE desktop.
//!
//! EDE's about box shows the desktop name, version, licence, and credits in
//! one page. NovaOS's equivalent is one page of text with the same job:
//! tell the user what this desktop is, what it is built on, and who built it.

#![no_std]
#![forbid(unsafe_code)]

/// How many credit lines the dialog shows at most; the shell paginates
/// beyond this.
pub const MAX_CREDITS: usize = 7;

/// The credit lines shown in the dialog.
pub const CREDITS: &[&str] = &[
    "NovaOS Desktop is a port of EDE",
    "(Equinox Desktop Environment),",
    "licensed under GPL-2.0.",
    "edelib is LGPL-2.0.",
    "",
    "Kernel, compositor and shells: Rust.",
    "Fonts: Sarasa Gothic, Source Han Sans.",
];

/// The About content, as lines of text.
#[derive(Clone, Copy)]
pub struct AboutContent {
    /// The product name, e.g. `NovaOS Desktop`.
    pub product: &'static str,
    /// The version string.
    pub version: &'static str,
    /// The one-line tagline.
    pub tagline: &'static str,
    /// The build description shown last.
    pub build: &'static str,
    /// The credit lines.
    pub credits: &'static [&'static str],
}

impl AboutContent {
    /// The canonical NovaOS desktop about content.
    pub const fn novaos() -> Self {
        Self {
            product: "NovaOS Desktop",
            version: "0.1.0",
            tagline: "A small, fast desktop, written in Rust.",
            build: "NovaOS 0.1.0 - x86_64 - EDE port",
            credits: CREDITS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_content_is_complete() {
        let about = AboutContent::novaos();
        assert!(about.product.contains("NovaOS"));
        assert_eq!(about.version, "0.1.0");
        assert!(!about.credits.is_empty());
        assert!(!about.build.is_empty());
        assert!(
            about.credits.len() <= MAX_CREDITS,
            "the dialog paginates past MAX_CREDITS"
        );
        assert!(about.credits.iter().any(|line| line.contains("EDE")));
    }
}
