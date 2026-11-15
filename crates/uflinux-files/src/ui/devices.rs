//! Short-lived, off-thread inventory of local block volumes.
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use ufm_vfs::fsinfo::FsInfo;

use super::computer::{Card, CardId};

pub fn discover() -> Vec<Card> {
    let Ok(output) = Command::new("lsblk")
        .args([
            "--json",
            "--bytes",
            "--output",
            "PATH,TYPE,FSTYPE,LABEL,UUID,RM,RO,MOUNTPOINTS,SIZE,PARTTYPE",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    let fs = FsInfo::new();
    let mut cards = Vec::new();
    if let Some(roots) = json.get("blockdevices").and_then(Value::as_array) {
        for root in roots {
            visit(root, &fs, &mut cards, false);
        }
    }
    cards
}

fn field<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn visit(node: &Value, fs: &FsInfo, cards: &mut Vec<Card>, parent_removable: bool) {
    let kind = field(node, "type").unwrap_or("");
    let removable = parent_removable || node.get("rm").and_then(Value::as_bool).unwrap_or(false);
    let fstype = field(node, "fstype").unwrap_or("");
    let path = field(node, "path").unwrap_or("");
    let mountpoint = node
        .get("mountpoints")
        .and_then(Value::as_array)
        .and_then(|points| {
            points
                .iter()
                .filter_map(Value::as_str)
                .find(|p| !p.is_empty())
        })
        .map(PathBuf::from);
    let system = mountpoint
        .as_deref()
        .is_some_and(|p| p == std::path::Path::new("/"));
    let efi = field(node, "parttype")
        .is_some_and(|p| p.eq_ignore_ascii_case("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"));
    if matches!(kind, "part" | "lvm" | "disk")
        && !fstype.is_empty()
        && !system
        && !efi
        && !path.is_empty()
        && fstype != "swap"
    {
        let label = field(node, "label")
            .or_else(|| field(node, "uuid"))
            .unwrap_or(path);
        let hints = mountpoint
            .as_deref()
            .map(|p| fs.hints(p))
            .unwrap_or_default();
        let locked = fstype == "crypto_LUKS";
        let supported = matches!(
            fstype,
            "ext2"
                | "ext3"
                | "ext4"
                | "btrfs"
                | "xfs"
                | "f2fs"
                | "vfat"
                | "exfat"
                | "ntfs"
                | "ntfs3"
        );
        let usage = match (hints.free, hints.total) {
            (Some(free), Some(total)) => format!(
                "{} free of {}",
                ufm_core::entry::human_size(free),
                ufm_core::entry::human_size(total)
            ),
            _ if mountpoint.is_some() => "Mounted".to_string(),
            _ if locked => "Locked".to_string(),
            _ if !supported => "Unsupported filesystem".to_string(),
            _ => "Not mounted".to_string(),
        };
        cards.push(Card {
            id: CardId::Device {
                path: PathBuf::from(path),
                mountpoint: mountpoint.clone(),
            },
            label: label.to_string(),
            detail: path.to_string(),
            icon: if removable {
                "drive-removable-media-symbolic"
            } else {
                "drive-harddisk-symbolic"
            }
            .to_string(),
            fraction: hints.used_fraction(),
            usage,
            state: if locked {
                Some("Locked")
            } else if !supported && mountpoint.is_none() {
                Some("Unsupported")
            } else if mountpoint.is_some() {
                None
            } else {
                Some("Not mounted")
            },
            can_open: mountpoint.is_some(),
            can_disconnect: false,
        });
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            visit(child, fs, cards, removable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_skips_system_and_efi_but_keeps_unmounted_and_locked_volumes() {
        let nodes: Value = serde_json::json!({"blockdevices": [
            {"path":"/dev/vda1","type":"part","fstype":"ext4","label":"System","mountpoints":["/"]},
            {"path":"/dev/vda2","type":"part","fstype":"vfat","label":"EFI","parttype":"c12a7328-f81f-11d2-ba4b-00a0c93ec93b","mountpoints":[null]},
            {"path":"/dev/sdb1","type":"part","fstype":"exfat","label":"Photos","mountpoints":[null],"rm":true},
            {"path":"/dev/sdc1","type":"part","fstype":"crypto_LUKS","label":"Private","mountpoints":[null]}
        ]});
        let fs = FsInfo::new();
        let mut cards = Vec::new();
        for node in nodes["blockdevices"].as_array().unwrap() {
            visit(node, &fs, &mut cards, false);
        }
        assert_eq!(
            cards
                .iter()
                .map(|card| card.label.as_str())
                .collect::<Vec<_>>(),
            ["Photos", "Private"]
        );
        assert_eq!(cards[0].state, Some("Not mounted"));
        assert_eq!(cards[1].state, Some("Locked"));
    }
}
