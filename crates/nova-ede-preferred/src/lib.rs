//! Rust port of `ede-preferred-applications`, EDE's default-applications
//! dialog.
//!
//! Upstream shows a list of categories — browser, mail reader, file
//! manager, terminal — and for each lets the user pick an installed
//! application, persisting the choice to `~/.ede/ede.conf` under
//! `[Preferred]`. The chooser validates that the chosen entry exists,
//! writes `Browser=…`-shaped keys, and falls back to the system defaults
//! when nothing was configured.
//!
//! The dialog itself is drawn by the shell; this crate is the model.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// How many candidates one category may list.
pub const MAX_CANDIDATES: usize = 8;
/// The longest service or label name kept.
pub const NAME_MAX: usize = 40;

/// The categories upstream's dialog offers, in display order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    /// The web browser.
    Browser,
    /// The mail reader.
    MailReader,
    /// The file manager.
    FileManager,
    /// The terminal emulator.
    Terminal,
}

impl Category {
    /// Every category, in display order.
    pub const ALL: [Category; 4] = [
        Category::Browser,
        Category::MailReader,
        Category::FileManager,
        Category::Terminal,
    ];

    /// The config key the category persists under.
    pub const fn key(self) -> &'static str {
        match self {
            Category::Browser => "Browser",
            Category::MailReader => "MailReader",
            Category::FileManager => "FileManager",
            Category::Terminal => "Terminal",
        }
    }

    /// The human-readable label the dialog shows.
    pub const fn label(self) -> &'static str {
        match self {
            Category::Browser => "Web browser",
            Category::MailReader => "Mail reader",
            Category::FileManager => "File manager",
            Category::Terminal => "Terminal",
        }
    }

    /// The config key from a `[Preferred]` line, if the line is one of
    /// the categories'.
    pub fn from_key(key: &[u8]) -> Option<Category> {
        Category::ALL
            .iter()
            .copied()
            .find(|category| category.key().as_bytes() == key)
    }
}

/// One candidate application for a category.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Candidate {
    /// The service name the launcher resolves.
    pub service: [u8; NAME_MAX],
    service_len: usize,
    /// The display label.
    pub label: [u8; NAME_MAX],
    label_len: usize,
}

impl Candidate {
    /// A candidate with a service name and label.
    pub fn new(service: &str, label: &str) -> Option<Self> {
        let service = service.as_bytes();
        let label = label.as_bytes();
        if service.is_empty() || service.len() > NAME_MAX || label.len() > NAME_MAX {
            return None;
        }
        let mut candidate = Self {
            service: [0; NAME_MAX],
            service_len: service.len(),
            label: [0; NAME_MAX],
            label_len: label.len(),
        };
        candidate.service[..service.len()].copy_from_slice(service);
        candidate.label[..label.len()].copy_from_slice(label);
        Some(candidate)
    }

    /// The service name.
    pub fn service(&self) -> &str {
        core::str::from_utf8(&self.service[..self.service_len]).unwrap_or("?")
    }

    /// The display label.
    pub fn label(&self) -> &str {
        core::str::from_utf8(&self.label[..self.label_len]).unwrap_or("?")
    }
}

/// The preferred-applications store: the candidates per category and the
/// current choices.
#[derive(Clone, Copy)]
pub struct PreferredApps {
    /// The choices, one per category, by service name.
    choices: [[u8; NAME_MAX]; Category::ALL.len()],
    choice_lens: [usize; Category::ALL.len()],
}

impl PreferredApps {
    /// The defaults: NovaOS's own services.
    pub fn new() -> Self {
        let mut apps = Self {
            choices: [[0; NAME_MAX]; Category::ALL.len()],
            choice_lens: [0; Category::ALL.len()],
        };
        apps.set(Category::Browser, "nova-web");
        apps.set(Category::MailReader, "nova-mail");
        apps.set(Category::FileManager, "file-manager");
        apps.set(Category::Terminal, "terminal");
        apps
    }

    /// Set the choice for a category.
    pub fn set(&mut self, category: Category, service: &str) -> bool {
        let bytes = service.as_bytes();
        if bytes.is_empty() || bytes.len() > NAME_MAX {
            return false;
        }
        let index = match category {
            Category::Browser => 0,
            Category::MailReader => 1,
            Category::FileManager => 2,
            Category::Terminal => 3,
        };
        self.choices[index][..bytes.len()].copy_from_slice(bytes);
        self.choice_lens[index] = bytes.len();
        true
    }

    /// The chosen service for a category.
    pub fn get(&self, category: Category) -> Option<&str> {
        let index = match category {
            Category::Browser => 0,
            Category::MailReader => 1,
            Category::FileManager => 2,
            Category::Terminal => 3,
        };
        let length = self.choice_lens[index];
        if length == 0 {
            return None;
        }
        core::str::from_utf8(&self.choices[index][..length]).ok()
    }

    /// Load `[Preferred]` from a config; unknown or empty keys keep the
    /// defaults.
    pub fn from_config(config: &EdeConfig) -> Self {
        let mut apps = Self::new();
        for category in Category::ALL {
            if let Some(value) = config.get("Preferred", category.key())
                && !value.is_empty()
            {
                apps.set(category, value);
            }
        }
        apps
    }

    /// Write the settings back as a `[Preferred]` section.
    pub fn to_config(&self, out: &mut [u8]) -> usize {
        let header = b"[Preferred]\n";
        let mut used = header.len().min(out.len());
        out[..used].copy_from_slice(&header[..used]);
        for category in Category::ALL {
            let Some(service) = self.get(category) else {
                continue;
            };
            used = write_key(out, used, category.key().as_bytes(), service.as_bytes());
        }
        used
    }
}

impl Default for PreferredApps {
    fn default() -> Self {
        Self::new()
    }
}

/// Append `Key = value\n` into `out` at `used`, returning the new length.
fn write_key(out: &mut [u8], used: usize, key: &[u8], value: &[u8]) -> usize {
    let mut cursor = used;
    for piece in [key, b" = ", value, b"\n"] {
        let take = piece.len().min(out.len().saturating_sub(cursor));
        out[cursor..cursor + take].copy_from_slice(&piece[..take]);
        cursor += take;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_novaos_services() {
        let apps = PreferredApps::new();
        assert_eq!(apps.get(Category::Browser), Some("nova-web"));
        assert_eq!(apps.get(Category::Terminal), Some("terminal"));
    }

    #[test]
    fn choices_overwrite_in_place() {
        let mut apps = PreferredApps::new();
        assert!(apps.set(Category::Browser, "other-browser"));
        assert_eq!(apps.get(Category::Browser), Some("other-browser"));
    }

    #[test]
    fn an_empty_service_is_rejected() {
        let mut apps = PreferredApps::new();
        assert!(!apps.set(Category::Browser, ""));
    }

    #[test]
    fn config_round_trips() {
        let config =
            EdeConfig::parse(b"[Preferred]\nBrowser = my-browser\nTerminal = my-terminal\n");
        let apps = PreferredApps::from_config(&config);
        assert_eq!(apps.get(Category::Browser), Some("my-browser"));
        assert_eq!(apps.get(Category::Terminal), Some("my-terminal"));
        // Unconfigured categories keep their defaults.
        assert_eq!(apps.get(Category::MailReader), Some("nova-mail"));
    }

    #[test]
    fn to_config_emits_only_set_categories() {
        let mut apps = PreferredApps::new();
        apps.set(Category::Browser, "my-browser");
        let mut buffer = [0u8; 128];
        let length = apps.to_config(&mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap();
        assert!(text.starts_with("[Preferred]\n"));
        assert!(text.contains("Browser = my-browser\n"));
    }

    #[test]
    fn categories_map_keys_back() {
        assert_eq!(Category::from_key(b"Browser"), Some(Category::Browser));
        assert_eq!(
            Category::from_key(b"MailReader"),
            Some(Category::MailReader)
        );
        assert_eq!(Category::from_key(b"Nope"), None);
    }
}
