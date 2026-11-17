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
            "PATH,TYPE,FSTYPE,LABEL,UUID,RM,RO,MOUNTPOINTS,SIZE,PARTTYPE,TRAN,HOTPLUG",
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
    for point in ufm_mount::monitor::Observer::new().points() {
        if !matches!(
            point.fstype.as_str(),
            "cifs" | "nfs" | "nfs4" | "fuse.sshfs" | "fuse.rclone" | "davfs" | "fuse.davfs"
        ) || point.path.starts_with(ufm_core::paths::mount_root())
        {
            continue;
        }
        // Bound remote stat calls: an unreachable NFS server must not stall hotplug discovery.
        let hints = remote_hints(&point.path);
        cards.push(Card {
            id: CardId::Device {
                path: point.path.clone(),
                mountpoint: Some(point.path.clone()),
            },
            label: point.source.clone(),
            detail: point.path.display().to_string(),
            icon: "network-server".into(),
            fraction: hints.used_fraction(),
            usage: match (hints.total, hints.free) {
                (Some(total), Some(free)) => format!(
                    "{} used of {}",
                    ufm_core::entry::human_size(total.saturating_sub(free)),
                    ufm_core::entry::human_size(total)
                ),
                _ => "Usage unavailable".into(),
            },
            state: None,
            can_open: true,
            can_disconnect: false,
        });
    }
    let mut seen = std::collections::HashSet::new();
    cards.retain(|card| match &card.id {
        CardId::Device { path, .. } => seen.insert(path.clone()),
        _ => true,
    });
    cards
}

fn remote_hints(path: &std::path::Path) -> ufm_vfs::FsHints {
    let output = Command::new("timeout")
        .args(["1", "stat", "-f", "-c", "%S %b %a"])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return ufm_vfs::FsHints::default();
    };
    if !output.status.success() {
        return ufm_vfs::FsHints::default();
    }
    let values: Vec<u64> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    if let [block, total, free] = values.as_slice() {
        ufm_vfs::FsHints {
            total: block.checked_mul(*total),
            free: block.checked_mul(*free),
            remote: true,
            ..Default::default()
        }
    } else {
        ufm_vfs::FsHints::default()
    }
}

fn field<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn visit(node: &Value, fs: &FsInfo, cards: &mut Vec<Card>, parent_removable: bool) {
    let kind = field(node, "type").unwrap_or("");
    let removable = parent_removable
        || field(node, "tran") == Some("usb")
        || node.get("rm").and_then(Value::as_bool).unwrap_or(false);
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
    let _system = mountpoint
        .as_deref()
        .is_some_and(|p| p == std::path::Path::new("/"));
    let efi = field(node, "parttype")
        .is_some_and(|p| p.eq_ignore_ascii_case("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"));
    if matches!(kind, "part" | "lvm" | "disk")
        && !fstype.is_empty()
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
                "{} used of {}",
                ufm_core::entry::human_size(total.saturating_sub(free)),
                ufm_core::entry::human_size(total)
            ),
            _ if mountpoint.is_some() => "Mounted".to_string(),
            _ if locked => "Locked".to_string(),
            _ if !supported => "Unsupported filesystem".to_string(),
            _ => format!(
                "{} · Not mounted",
                ufm_core::entry::human_size(node.get("size").and_then(Value::as_u64).unwrap_or(0))
            ),
        };
        cards.push(Card {
            id: CardId::Device {
                path: PathBuf::from(path),
                mountpoint: mountpoint.clone(),
            },
            label: label.to_string(),
            detail: path.to_string(),
            icon: if removable {
                "drive-removable-media"
            } else {
                "drive-harddisk"
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
    fn usb_transport_marks_children_removable_even_when_rm_is_false() {
        let node = serde_json::json!({"path":"/dev/sdb", "type":"disk", "tran":"usb", "rm":false,
            "children":[{"path":"/dev/sdb1", "type":"part", "fstype":"exfat", "size":1024, "mountpoints":[null]}]});
        let mut cards = Vec::new();
        visit(&node, &FsInfo::new(), &mut cards, false);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].icon, "drive-removable-media");
        assert!(cards[0].usage.contains("1.0 KiB"));
    }

    #[test]
    fn inventory_keeps_system_unmounted_and_locked_volumes_but_skips_efi() {
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
            ["System", "Photos", "Private"]
        );
        assert_eq!(cards[1].state, Some("Not mounted"));
        assert_eq!(cards[2].state, Some("Locked"));
    }
}
