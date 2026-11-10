//! Rust port of `ede-conf`, EDE's control panel.
//!
//! Upstream reads `ede-conf.conf`, a list of settings tools — each with
//! a name, a tooltip, an `Exec` command and an icon — and shows one
//! button per tool; clicking runs it. This crate is the model: parsing
//! the panel's item list and resolving a click to the command to run.
//!
//! ```text
//! [EdeConf]
//!  items = ede-desktop-conf,ede-screensaver-conf,...
//!
//! [ede-desktop-conf]
//!  name = Background and icons
//!  tip  = This item will configure desktop background and icons
//!  exec = ede-desktop-conf
//!  icon = preferences-desktop-wallpaper
//! ```

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// How many tools one control panel holds.
pub const MAX_ITEMS: usize = 16;
/// The longest name, tooltip, or command kept.
pub const FIELD_MAX: usize = 64;
/// The longest icon theme name kept.
pub const ICON_MAX: usize = 40;

/// One control-panel tool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PanelItem {
    /// The display name, e.g. `Background and icons`.
    pub name: [u8; FIELD_MAX],
    name_len: usize,
    /// The tooltip shown while the pointer hovers the button.
    pub tip: [u8; FIELD_MAX],
    tip_len: usize,
    /// The `Exec` command, e.g. `ede-desktop-conf`.
    pub exec: [u8; FIELD_MAX],
    exec_len: usize,
    /// The icon theme name.
    pub icon: [u8; ICON_MAX],
    icon_len: usize,
}

impl PanelItem {
    /// The display name.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }

    /// The tooltip text.
    pub fn tip(&self) -> &str {
        core::str::from_utf8(&self.tip[..self.tip_len]).unwrap_or("")
    }

    /// The command to run.
    pub fn exec(&self) -> &str {
        core::str::from_utf8(&self.exec[..self.exec_len]).unwrap_or("")
    }

    /// The icon theme name.
    pub fn icon(&self) -> &str {
        core::str::from_utf8(&self.icon[..self.icon_len]).unwrap_or("")
    }
}

/// The control panel: its items in display order.
#[derive(Clone, Copy)]
pub struct ConfPanel {
    items: [Option<PanelItem>; MAX_ITEMS],
    count: usize,
}

impl ConfPanel {
    /// An empty panel.
    pub const fn new() -> Self {
        Self {
            items: [None; MAX_ITEMS],
            count: 0,
        }
    }

    /// Parse the control panel configuration: `items` names the
    /// sections in order; each section carries `name`, `tip`, `exec`,
    /// and `icon`. Sections that fail any of these are skipped — a
    /// malformed item costs one button, never the whole panel.
    pub fn from_config(config: &EdeConfig) -> Self {
        let mut panel = Self::new();
        let Some(list) = config.get("EdeConf", "items") else {
            return panel;
        };
        for section in list.split(',') {
            let section = section.trim();
            if section.is_empty() {
                continue;
            }
            let (Some(name), Some(exec)) =
                (config.get(section, "name"), config.get(section, "exec"))
            else {
                continue;
            };
            if panel.count >= MAX_ITEMS {
                break;
            }
            let mut item = PanelItem {
                name: [0; FIELD_MAX],
                name_len: 0,
                tip: [0; FIELD_MAX],
                tip_len: 0,
                exec: [0; FIELD_MAX],
                exec_len: 0,
                icon: [0; ICON_MAX],
                icon_len: 0,
            };
            copy(&mut item.name, &mut item.name_len, name.as_bytes());
            copy(&mut item.exec, &mut item.exec_len, exec.as_bytes());
            if let Some(tip) = config.get(section, "tip") {
                copy(&mut item.tip, &mut item.tip_len, tip.as_bytes());
            }
            if let Some(icon) = config.get(section, "icon") {
                copy(&mut item.icon, &mut item.icon_len, icon.as_bytes());
            }
            panel.items[panel.count] = Some(item);
            panel.count += 1;
        }
        panel
    }

    /// How many tools the panel shows.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The tool at `index`, in display order.
    pub fn item(&self, index: usize) -> Option<&PanelItem> {
        self.items.get(index)?.as_ref()
    }

    /// The command for the tool the user clicked, if any.
    pub fn command_for(&self, index: usize) -> Option<&str> {
        let item = self.item(index)?;
        Some(item.exec())
    }
}

impl Default for ConfPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// Copy a string into a fixed field.
fn copy(slot: &mut [u8], len: &mut usize, value: &[u8]) {
    let take = value.len().min(slot.len());
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDE_CONF: &[u8] = b"\
[EdeConf]
 items = ede-desktop-conf,ede-screensaver-conf,ede-timedate,broken-item,ede-bell-conf

[ede-desktop-conf]
 name = Background and icons
 tip  = This item will configure desktop background and icons
 exec = ede-desktop-conf
 icon = preferences-desktop-wallpaper

[ede-screensaver-conf]
 name = Screensaver
 tip  = This item will setup screensaver settings
 exec = ede-screensaver-conf
 icon = preferences-desktop-screensaver

[ede-timedate]
 name = Time and date
 tip  = This item will configure system date and time
 exec = ede-timedate
 icon = preferences-date-time

[broken-item]
 tip = no name, no exec

[ede-bell-conf]
 name = System bell
 exec = ede-bell-conf
";

    fn panel() -> ConfPanel {
        ConfPanel::from_config(&EdeConfig::parse(EDE_CONF))
    }

    #[test]
    fn items_are_parsed_in_order() {
        let panel = panel();
        assert_eq!(panel.count(), 4, "the broken item is skipped");
        assert_eq!(panel.item(0).unwrap().name(), "Background and icons");
        assert_eq!(panel.item(3).unwrap().name(), "System bell");
    }

    #[test]
    fn each_item_carries_its_fields() {
        let panel = panel();
        let item = panel.item(1).unwrap();
        assert_eq!(item.exec(), "ede-screensaver-conf");
        assert_eq!(item.icon(), "preferences-desktop-screensaver");
        assert_eq!(item.tip(), "This item will setup screensaver settings");
    }

    #[test]
    fn a_click_resolves_to_a_command() {
        let panel = panel();
        assert_eq!(panel.command_for(0), Some("ede-desktop-conf"));
        assert_eq!(panel.command_for(99), None);
    }

    #[test]
    fn an_unlisted_item_is_not_shown() {
        let panel = panel();
        let names: [&str; 4] = core::array::from_fn(|index| panel.item(index).unwrap().name());
        assert!(!names.contains(&"no name, no exec"));
    }

    #[test]
    fn a_missing_items_list_is_an_empty_panel() {
        let config = EdeConfig::parse(b"[EdeConf]\n");
        assert_eq!(ConfPanel::from_config(&config).count(), 0);
    }
}
