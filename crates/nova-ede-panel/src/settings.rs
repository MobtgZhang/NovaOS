//! The panel's configuration — the `~/.ede/panel.conf` file EDE's panel
//! reads at startup.
//!
//! Upstream keys:
//!
//! ```text
//! [Panel]
//! position = 0|1      ; 0 = top, 1 = bottom
//! width = 10..100     ; percent of the screen
//! hider = 0|1
//! applets = name1,name2,...
//! ```

use nova_edelib::EdeConfig;

/// Which edge the panel is docked to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Position {
    /// The top edge — EDE's default.
    Top,
    /// The bottom edge — NovaOS's default, so the desktop's icon column
    /// keeps its top-left flow.
    Bottom,
}

/// How many applet names one panel configuration may list.
pub const MAX_APPLETS: usize = 10;
/// The longest applet name kept.
pub const APPLET_NAME_MAX: usize = 24;

/// The parsed panel configuration, with defaults for anything unset.
#[derive(Clone, Copy, Debug)]
pub struct PanelSettings {
    /// Which edge the panel docks to.
    pub position: Position,
    /// The panel's height in pixels; 32 when unset.
    pub height: u32,
    /// Whether the auto-hider is enabled.
    pub hider: bool,
    /// The applet names, in order.
    pub applets: [Option<[u8; APPLET_NAME_MAX]>; MAX_APPLETS],
    applet_count: usize,
}

impl PanelSettings {
    /// The defaults: bottom, 32px, no hider, EDE's applet order.
    pub const fn new() -> Self {
        Self {
            position: Position::Bottom,
            height: 32,
            hider: false,
            applets: [None; MAX_APPLETS],
            applet_count: 0,
        }
    }

    /// Parse a configuration file, falling back to the defaults for any
    /// key that is missing or malformed — a broken line costs one entry,
    /// never the whole panel.
    pub fn from_config(config: &EdeConfig) -> Self {
        let mut settings = Self::new();
        match config.get_u32("Panel", "height") {
            Some(height @ 16..=64) => settings.height = height,
            _ => settings.height = 32,
        }
        settings.position = match config.get_u32("Panel", "position") {
            Some(0) => Position::Top,
            Some(1) => Position::Bottom,
            _ => Position::Bottom,
        };
        settings.hider = config.get_bool("Panel", "hider").unwrap_or(false);
        if let Some(applets) = config.get("Panel", "applets") {
            settings.set_applets(applets);
        }
        settings
    }

    /// Fill the applet list from a comma-separated string. Unknown names
    /// are kept verbatim — the panel decides what to do with them.
    pub fn set_applets(&mut self, list: &str) {
        self.applet_count = 0;
        for name in list.split(',') {
            let name = name.trim();
            if name.is_empty() || self.applet_count >= MAX_APPLETS {
                continue;
            }
            let mut slot = [0u8; APPLET_NAME_MAX];
            let bytes = name.as_bytes();
            let take = bytes.len().min(APPLET_NAME_MAX);
            slot[..take].copy_from_slice(&bytes[..take]);
            self.applets[self.applet_count] = Some(slot);
            self.applet_count += 1;
        }
    }

    /// How many applets are configured.
    pub const fn applet_count(&self) -> usize {
        self.applet_count
    }

    /// The applet name at `index`.
    pub fn applet(&self, index: usize) -> Option<&str> {
        let slot = self.applets.get(index)?.as_ref()?;
        core::str::from_utf8(&slot[..slot.iter().position(|&b| b == 0).unwrap_or(slot.len())]).ok()
    }

    /// Whether the panel lists a given applet by name.
    pub fn has_applet(&self, name: &str) -> bool {
        (0..self.applet_count).any(|index| self.applet(index) == Some(name))
    }
}

impl Default for PanelSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] =
        b"[Panel]\nposition = 1\nheight = 36\nhider = true\napplets = start-menu,quick-launch,taskbar\n";

    #[test]
    fn parses_the_upstream_keys() {
        let config = EdeConfig::parse(SAMPLE);
        let settings = PanelSettings::from_config(&config);
        assert_eq!(settings.position, Position::Bottom);
        assert_eq!(settings.height, 36);
        assert!(settings.hider);
        assert!(settings.has_applet("start-menu"));
        assert!(settings.has_applet("quick-launch"));
        assert!(settings.has_applet("taskbar"));
        assert!(!settings.has_applet("clock"));
        assert_eq!(settings.applet_count(), 3);
    }

    #[test]
    fn top_position_parses() {
        let config = EdeConfig::parse(b"[Panel]\nposition = 0\n");
        let settings = PanelSettings::from_config(&config);
        assert_eq!(settings.position, Position::Top);
    }

    #[test]
    fn defaults_fill_missing_keys() {
        let config = EdeConfig::parse(b"[Panel]\n");
        let settings = PanelSettings::from_config(&config);
        assert_eq!(settings.height, 32);
        assert_eq!(settings.position, Position::Bottom);
        assert!(!settings.hider);
        assert_eq!(settings.applet_count(), 0);
    }

    #[test]
    fn an_out_of_range_height_falls_back() {
        let config = EdeConfig::parse(b"[Panel]\nheight = 999\n");
        let settings = PanelSettings::from_config(&config);
        assert_eq!(settings.height, 32);
    }

    #[test]
    fn the_applet_list_is_capped() {
        let mut settings = PanelSettings::new();
        settings.set_applets("a,b,c,d,e,f,g,h,i,j,k,l");
        assert_eq!(settings.applet_count(), MAX_APPLETS);
        assert_eq!(settings.applet(0), Some("a"));
        assert_eq!(settings.applet(MAX_APPLETS), None);
    }
}
