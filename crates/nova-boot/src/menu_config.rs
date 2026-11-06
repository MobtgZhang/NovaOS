//! NovaBoot 菜单配置文件的解析器。
//!
//! 配置文件按空行分成若干块，每块描述一个可启动项，例如：
//!
//! ```text
//! title NovaOS x86_64
//! path /boot/nova-kernel.elf
//! arch x86_64
//!
//! title NovaOS aarch64
//! path /boot/nova-kernel-aarch64.elf
//! arch aarch64
//! ```
//!
//! 该解析器只依赖 `core`，不读取任何固件页，因此可以在宿主机上
//! 单元测试，也可被 UEFI 启动菜单应用复用。

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::BootArch;

/// 解析出的单个启动项，与 [`crate::BootMenuEntry`] 对应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntryConfig {
    pub id: u16,
    pub name: String,
    pub architecture: BootArch,
    pub path: String,
    pub flags: u32,
}

/// 解析整个配置文件，返回按文件顺序排列的启动项。
///
/// 每个块必须包含 `title`、`path`、`arch`；缺少任何一项都视为错误。
/// `id` 可省略，省略时按出现顺序从 1 开始自动编号。
pub fn parse(input: &str) -> Result<Vec<MenuEntryConfig>, MenuConfigError> {
    let mut entries = Vec::new();
    let mut next_id: u16 = 1;
    let mut current: Option<PartialEntry> = None;

    // 追加一个虚拟的结尾块，以便统一处理最后一个条目。
    for line in input.lines().chain(core::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // 块结束：提交当前条目。
            if let Some(partial) = current.take() {
                let entry = partial.finish(next_id)?;
                next_id = entry.id.wrapping_add(1);
                entries.push(entry);
            }
            continue;
        }

        // 跳过注释行。
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }

        let Some((key, value)) = trimmed.split_once(|c: char| c.is_whitespace()) else {
            return Err(MenuConfigError::BadLine {
                line: trimmed.to_string(),
            });
        };

        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            return Err(MenuConfigError::BadLine {
                line: trimmed.to_string(),
            });
        }

        match key {
            "title" | "name" => {
                current.get_or_insert_with(PartialEntry::default).name = Some(value.to_string());
            }
            "path" => {
                current.get_or_insert_with(PartialEntry::default).path = Some(value.to_string());
            }
            "arch" | "architecture" => {
                let arch = BootArch::from_name(value).ok_or(MenuConfigError::BadArch {
                    value: value.to_string(),
                })?;
                current
                    .get_or_insert_with(PartialEntry::default)
                    .architecture = Some(arch);
            }
            "id" => {
                let id = value.parse::<u16>().map_err(|_| MenuConfigError::BadId {
                    value: value.to_string(),
                })?;
                current.get_or_insert_with(PartialEntry::default).id = Some(id);
            }
            "flags" => {
                let flags = value
                    .parse::<u32>()
                    .map_err(|_| MenuConfigError::BadFlags {
                        value: value.to_string(),
                    })?;
                current.get_or_insert_with(PartialEntry::default).flags = Some(flags);
            }
            _ => {
                // 未知键忽略，保持对未来字段的兼容性。
            }
        }
    }

    // 检查 id 唯一性。
    let mut seen_ids = [false; crate::MAX_MENU_ENTRIES];
    for entry in &entries {
        let index = entry.id as usize;
        if index == 0 || index > crate::MAX_MENU_ENTRIES {
            return Err(MenuConfigError::BadId {
                value: entry.id.to_string(),
            });
        }
        if seen_ids[index - 1] {
            return Err(MenuConfigError::DuplicateId);
        }
        seen_ids[index - 1] = true;
    }

    Ok(entries)
}

/// 单个条目在提交前的中间状态。
#[derive(Clone, Default, Debug)]
struct PartialEntry {
    id: Option<u16>,
    name: Option<String>,
    path: Option<String>,
    architecture: Option<BootArch>,
    flags: Option<u32>,
}

impl PartialEntry {
    fn finish(self, default_id: u16) -> Result<MenuEntryConfig, MenuConfigError> {
        let id = self.id.unwrap_or(default_id);
        let name = self
            .name
            .ok_or(MenuConfigError::MissingField { field: "title" })?;
        let path = self
            .path
            .ok_or(MenuConfigError::MissingField { field: "path" })?;
        let architecture = self
            .architecture
            .ok_or(MenuConfigError::MissingField { field: "arch" })?;
        let flags = self.flags.unwrap_or(0);

        if name.is_empty() {
            return Err(MenuConfigError::EmptyField { field: "title" });
        }
        if path.is_empty() {
            return Err(MenuConfigError::EmptyField { field: "path" });
        }
        if name.len() > crate::MAX_MENU_NAME {
            return Err(MenuConfigError::NameTooLong);
        }
        if path.len() > crate::MAX_MENU_PATH {
            return Err(MenuConfigError::PathTooLong);
        }

        Ok(MenuEntryConfig {
            id,
            name,
            architecture,
            path,
            flags,
        })
    }
}

/// 配置文件解析失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuConfigError {
    /// 某一行不是 `key value` 形式。
    BadLine {
        line: String,
    },
    /// `arch` 的值不是四种受支持架构之一。
    BadArch {
        value: String,
    },
    /// `id` 或 `flags` 不是数字。
    BadId {
        value: String,
    },
    BadFlags {
        value: String,
    },
    /// 缺少必填字段。
    MissingField {
        field: &'static str,
    },
    /// 字段为空字符串。
    EmptyField {
        field: &'static str,
    },
    /// 名称或路径超出协议上限。
    NameTooLong,
    PathTooLong,
    /// 条目 id 重复。
    DuplicateId,
}

impl MenuConfigError {
    pub fn tag(self) -> &'static str {
        match self {
            Self::BadLine { .. } => "bad-line",
            Self::BadArch { .. } => "bad-arch",
            Self::BadId { .. } => "bad-id",
            Self::BadFlags { .. } => "bad-flags",
            Self::MissingField { .. } => "missing-field",
            Self::EmptyField { .. } => "empty-field",
            Self::NameTooLong => "name-too-long",
            Self::PathTooLong => "path-too-long",
            Self::DuplicateId => "duplicate-id",
        }
    }
}

impl core::fmt::Display for MenuConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadLine { line } => write!(f, "bad config line: {line:?}"),
            Self::BadArch { value } => write!(f, "unsupported architecture: {value}"),
            Self::BadId { value } => write!(f, "invalid id: {value}"),
            Self::BadFlags { value } => write!(f, "invalid flags: {value}"),
            Self::MissingField { field } => write!(f, "missing required field: {field}"),
            Self::EmptyField { field } => write!(f, "empty field: {field}"),
            Self::NameTooLong => write!(f, "menu entry name exceeds the limit"),
            Self::PathTooLong => write!(f, "menu entry path exceeds the limit"),
            Self::DuplicateId => write!(f, "duplicate menu entry id"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_entries() {
        let input = "title NovaOS x86_64\npath /boot/nova-kernel.elf\narch x86_64\n\ntitle NovaOS aarch64\npath /boot/kernel-aarch64.elf\narch aarch64\n";
        let entries = parse(input).expect("parse should succeed");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[0].name, "NovaOS x86_64");
        assert_eq!(entries[0].path, "/boot/nova-kernel.elf");
        assert_eq!(entries[0].architecture, BootArch::X86_64);
        assert_eq!(entries[1].id, 2);
        assert_eq!(entries[1].architecture, BootArch::AArch64);
    }

    #[test]
    fn explicit_ids_are_honored() {
        let input = "title A\npath /a\narch x86_64\nid 9\n\ntitle B\npath /b\narch aarch64\nid 3\n";
        let entries = parse(input).expect("parse should succeed");
        assert_eq!(entries[0].id, 9);
        assert_eq!(entries[1].id, 3);
    }

    #[test]
    fn missing_path_is_rejected() {
        let input = "title A\narch x86_64\n";
        assert_eq!(
            parse(input),
            Err(MenuConfigError::MissingField { field: "path" })
        );
    }

    #[test]
    fn unknown_arch_is_rejected() {
        let input = "title A\npath /a\narch mips\n";
        assert_eq!(
            parse(input),
            Err(MenuConfigError::BadArch {
                value: "mips".to_string()
            })
        );
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let input = "title A\npath /a\narch x86_64\nid 1\n\ntitle B\npath /b\narch aarch64\nid 1\n";
        assert_eq!(parse(input), Err(MenuConfigError::DuplicateId));
    }

    #[test]
    fn comments_and_whitespace_are_ignored() {
        let input = "# NovaOS boot menu\n\ntitle NovaOS\npath /boot/kernel.elf\narch x86_64\n   \n// trailing comment\n";
        let entries = parse(input).expect("parse should succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "NovaOS");
    }

    #[test]
    fn name_truncated_to_limit_is_rejected() {
        let long_name = "a".repeat(crate::MAX_MENU_NAME + 1);
        let mut input = "title ".to_string();
        input.push_str(&long_name);
        input.push_str("\npath /a\narch x86_64\n");
        assert_eq!(parse(&input), Err(MenuConfigError::NameTooLong));
    }

    #[test]
    fn path_truncated_to_limit_is_rejected() {
        let long_path = "p".repeat(crate::MAX_MENU_PATH + 1);
        let mut input = "title A\npath ".to_string();
        input.push_str(&long_path);
        input.push_str("\narch x86_64\n");
        assert_eq!(parse(&input), Err(MenuConfigError::PathTooLong));
    }
}
