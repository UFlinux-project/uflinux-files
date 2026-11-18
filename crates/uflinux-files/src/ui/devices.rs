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
            "--tree",
            "--all",
            "--bytes",
            "--output",
            "PATH,TYPE,MODEL,FSTYPE,LABEL,UUID,RM,RO,MOUNTPOINTS,SIZE,PARTTYPE,TRAN,HOTPLUG",
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
            visit(root, &fs, &mut cards, false, None);
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
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn visit(
    node: &Value,
    fs: &FsInfo,
    cards: &mut Vec<Card>,
    parent_removable: bool,
    parent_model: Option<&str>,
) {
    let kind = field(node, "type").unwrap_or("");
    let removable = parent_removable
        || field(node, "tran") == Some("usb")
        || node.get("rm").and_then(Value::as_bool).unwrap_or(false);
    let fstype = field(node, "fstype").unwrap_or("");
    let path = field(node, "path").unwrap_or("");
    let model = field(node, "model").or(parent_model);
    let first_child = cards.len();
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            visit(child, fs, cards, removable, model);
        }
    }
    let count_children = cards.len() - first_child;
    let optical = kind == "rom";
    let efi = field(node, "parttype")
        .is_some_and(|p| p.eq_ignore_ascii_case("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"));
    let standalone_disk = kind == "disk" && count_children == 0;
    if path.is_empty()
        || path.starts_with("/dev/ram")
        || path.starts_with("/dev/zram")
        || efi
        || fstype == "swap"
        || !(optical
            || standalone_disk
            || (matches!(kind, "part" | "lvm" | "disk") && !fstype.is_empty()))
    {
        return;
    }

    let mountpoint = node
        .get("mountpoints")
        .and_then(Value::as_array)
        .and_then(|points| {
            let paths = || {
                points
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|path| !path.is_empty())
            };
            paths().find(|path| *path == "/").or_else(|| paths().next())
        })
        .map(PathBuf::from);
    let root = mountpoint
        .as_deref()
        .is_some_and(|p| p == std::path::Path::new("/"));
    let size = node.get("size").and_then(Value::as_u64).unwrap_or(0);
    let label = if root {
        "System"
    } else if optical {
        field(node, "label").unwrap_or("DVD Drive")
    } else {
        field(node, "label").unwrap_or(path)
    };
    let icon = if optical {
        "media-optical-dvd"
    } else if removable {
        "drive-removable-media"
    } else {
        "drive-harddisk"
    };
    let hints = mountpoint
        .as_deref()
        .map(|p| fs.hints(p))
        .unwrap_or_default();
    let state = if mountpoint.is_some() {
        None
    } else {
        Some(match fstype {
            "" if optical && size == 0 => "No disc",
            "" if optical => "No readable disc",
            "" => "Unformatted",
            "crypto_LUKS" => "Locked",
            "ext2" | "ext3" | "ext4" | "btrfs" | "xfs" | "f2fs" | "vfat" | "exfat" | "ntfs"
            | "ntfs3" | "iso9660" | "udf" => "Not mounted",
            _ => "Unsupported",
        })
    };
    let usage = match (hints.free, hints.total) {
        (Some(free), Some(total)) => format!(
            "{} used of {}",
            ufm_core::entry::human_size(total.saturating_sub(free)),
            ufm_core::entry::human_size(total)
        ),
        _ if size > 0 && !(optical && fstype.is_empty()) => ufm_core::entry::human_size(size),
        _ => state.unwrap_or("Mounted").into(),
    };
    cards.push(Card {
        id: CardId::Device {
            path: PathBuf::from(path),
            mountpoint: mountpoint.clone(),
        },
        label: label.into(),
        detail: model.map_or_else(|| path.into(), |model| format!("{model} · {path}")),
        icon: icon.into(),
        fraction: hints.used_fraction(),
        usage,
        state,
        can_open: mountpoint.is_some(),
        can_disconnect: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_transport_marks_children_removable_even_when_rm_is_false() {
        let node = serde_json::json!({"path":"/dev/sdb", "type":"disk", "tran":"usb", "rm":false,
            "children":[{"path":"/dev/sdb1", "type":"part", "fstype":"exfat", "size":1024, "mountpoints":[null]}]});
        let mut cards = Vec::new();
        visit(&node, &FsInfo::new(), &mut cards, false, None);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].icon, "drive-removable-media");
        assert!(cards[0].usage.contains("1.0 kB"));
    }

    #[test]
    fn optical_drives_remain_visible_with_or_without_a_data_disc() {
        let nodes = [
            serde_json::json!({"path":"/dev/sr0","type":"rom","model":"  DVD Writer  ","size":0}),
            serde_json::json!({"path":"/dev/sr1","type":"rom","model":"DVD-ROM","fstype":"udf","label":"Backup","size":4_700_000_000u64}),
        ];
        let mut cards = Vec::new();
        for node in &nodes {
            visit(node, &FsInfo::new(), &mut cards, false, None);
        }
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].icon, "media-optical-dvd");
        assert_eq!(cards[0].detail, "DVD Writer · /dev/sr0");
        assert_eq!(cards[0].usage, "No disc");
        assert_eq!(cards[1].icon, "media-optical-dvd");
        assert_eq!(cards[1].label, "Backup");
        assert_eq!(cards[1].usage, "4.7 GB");
        assert_eq!(cards[1].state, Some("Not mounted"));
    }

    #[test]
    fn drive_models_follow_partitions_and_blank_drives_keep_their_own_card() {
        let nodes = [
            serde_json::json!({"path":"/dev/sda","type":"disk","model":"Internal SSD",
                "children":[{"path":"/dev/sda1","type":"part","label":"Data","fstype":"ext4","size":100_000_000_000u64}]}),
            serde_json::json!({"path":"/dev/sdb","type":"disk","model":"External SSD","size":250_000_000_000u64}),
        ];
        let mut cards = Vec::new();
        for node in &nodes {
            visit(node, &FsInfo::new(), &mut cards, false, None);
        }
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].detail, "Internal SSD · /dev/sda1");
        assert_eq!(cards[1].detail, "External SSD · /dev/sdb");
        assert_eq!(cards[1].usage, "250 GB");
        assert_eq!(cards[1].state, Some("Unformatted"));
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
            visit(node, &fs, &mut cards, false, None);
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
