use super::{Error, Images, Result, SlotEvidence, Task};
use chrono::DateTime;
use std::{collections::HashSet, path::Path};

#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    pub boot_slot: String,
    pub primary: String,
    pub slots: Vec<SlotEvidence>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Bundle {
    pub version: String,
    pub hash: String,
    pub images: Images,
}

pub(super) fn checksum(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn system_images(images: &Images) -> bool {
    images.len() == 2
        && ["rootfs", "boot"]
            .iter()
            .all(|class| images.get(*class).is_some_and(|hash| checksum(hash)))
}

pub(super) fn loader_images(images: &Images) -> bool {
    images.len() == 2
        && ["uboot", "idb"]
            .iter()
            .all(|class| images.get(*class).is_some_and(|hash| checksum(hash)))
}

pub(super) fn bootloader_task(task: &Task) -> bool {
    loader_images(&task.images)
}

pub(super) fn validate_bundle(mut bundle: Bundle) -> Result<Bundle> {
    if !checksum(&bundle.hash) || (!system_images(&bundle.images) && !loader_images(&bundle.images))
    {
        return Err(Error::Unavailable(
            "bundle requires a signed manifest with exactly rootfs and boot, or uboot and idb checksums".into(),
        ));
    }
    for hash in bundle.images.values_mut() {
        hash.make_ascii_lowercase();
    }
    Ok(bundle)
}

pub(super) fn target(live: &Snapshot) -> Result<&SlotEvidence> {
    let mut slots = live.slots.iter().filter(|slot| {
        slot.class == "rootfs"
            && slot.parent.is_empty()
            && slot.state != "booted"
            && !matches_boot(slot, &live.boot_slot)
    });
    let first = slots.next();
    if let Some(slot) = first.filter(|slot| !slot.bootname.is_empty() && slots.next().is_none()) {
        Ok(slot)
    } else {
        Err(Error::Unavailable(
            "cannot unambiguously select inactive rootfs slot".into(),
        ))
    }
}

fn matches_boot(slot: &SlotEvidence, boot: &str) -> bool {
    !boot.is_empty() && (slot.name == boot || slot.bootname == boot || slot.device == boot)
}

// BootSlot and RAUC's slot state must agree on the identity of one rootfs.
pub(super) fn actual_root<'a>(slots: &'a [SlotEvidence], boot: &str) -> Option<&'a SlotEvidence> {
    let mut running = None;
    for slot in slots
        .iter()
        .filter(|s| s.class == "rootfs" && s.parent.is_empty())
    {
        let matches = matches_boot(slot, boot);
        if matches != (slot.state == "booted") || matches && running.is_some() {
            return None;
        }
        if matches && !slot.name.is_empty() {
            running = Some(slot);
        }
    }
    running
}

pub(super) fn slot_pair<'a>(
    slots: &'a [SlotEvidence],
    target: &str,
) -> Option<(&'a SlotEvidence, &'a SlotEvidence)> {
    let mut root = None;
    let mut boot = None;
    for slot in slots
        .iter()
        .filter(|s| s.name == target || s.parent == target)
    {
        if slot.name.is_empty() {
            return None;
        }
        if slot.name == target && slot.class == "rootfs" && slot.parent.is_empty() && root.is_none()
        {
            root = Some(slot);
        } else if slot.parent == target && slot.class == "boot" && boot.is_none() {
            boot = Some(slot);
        } else {
            return None;
        }
    }
    root.zip(boot)
}

pub(super) fn loader_pair(slots: &[SlotEvidence]) -> Option<(&SlotEvidence, &SlotEvidence)> {
    let mut uboot = None;
    let mut idb = None;
    for slot in slots
        .iter()
        .filter(|s| matches!(s.class.as_str(), "uboot" | "idb"))
    {
        if !loader_slot(slot) {
            return None;
        }
        let entry = if slot.class == "uboot" {
            &mut uboot
        } else {
            &mut idb
        };
        if entry.replace(slot).is_some() {
            return None;
        }
    }
    uboot.zip(idb)
}

fn loader_slot(slot: &SlotEvidence) -> bool {
    let (name, device) = if slot.class == "uboot" {
        ("uboot.0", "/dev/naos/uboot")
    } else {
        ("idb.0", "/dev/naos/disk")
    };
    slot.name == name
        && slot.device == device
        && slot.parent.is_empty()
        && slot.bootname.is_empty()
        && slot.boot_status.is_empty()
        && slot.state == "inactive"
}

pub(super) fn check_topology(slots: &[SlotEvidence]) -> bool {
    if !(4..=32).contains(&slots.len()) {
        return false;
    }
    let mut devices = HashSet::new();
    let mut bootnames = HashSet::new();
    let mut names = HashSet::new();
    let mut roots = 0;
    let mut bootloaders = 0;
    for slot in slots {
        if slot.name.is_empty()
            || !names.insert(&slot.name)
            || slot.device == "/"
            || !Path::new(&slot.device).is_absolute()
            || !devices.insert(&slot.device)
            || !matches!(slot.state.as_str(), "booted" | "active" | "inactive")
        {
            return false;
        }
        match slot.class.as_str() {
            "rootfs" => {
                if !slot.parent.is_empty()
                    || slot.bootname.is_empty()
                    || !bootnames.insert(&slot.bootname)
                    || !matches!(slot.boot_status.as_str(), "good" | "bad")
                    || slot_pair(slots, &slot.name).is_none()
                {
                    return false;
                }
                roots += 1;
            }
            "boot" => {
                if slot.parent.is_empty()
                    || !slot.bootname.is_empty()
                    || !slot.boot_status.is_empty()
                    || slot.state == "booted"
                    || !slots
                        .iter()
                        .any(|parent| parent.name == slot.parent && parent.class == "rootfs")
                {
                    return false;
                }
            }
            "idb" | "uboot" if loader_slot(slot) => bootloaders += 1,
            _ => return false,
        }
    }
    roots * 2 + bootloaders == slots.len()
}

fn complete_image(slot: &SlotEvidence) -> bool {
    slot.status == "ok"
        && !slot.version.is_empty()
        && checksum(&slot.manifest_hash)
        && checksum(&slot.sha256)
        && slot.installed_count > 0
        && DateTime::parse_from_rfc3339(&slot.installed_at).is_ok()
}

fn complete_check_record(slot: &SlotEvidence) -> bool {
    complete_image(slot)
        || slot.status.is_empty()
            && slot.version.is_empty()
            && slot.manifest_hash.is_empty()
            && slot.sha256.is_empty()
            && slot.transaction.is_empty()
            && slot.installed_at.is_empty()
            && slot.installed_count == 0
}

pub(super) fn unchanged_check(t: &Task, live: &Snapshot) -> bool {
    t.before.len() == live.slots.len()
        && check_topology(&t.before)
        && t.boot_slot == live.boot_slot
        && t.primary == live.primary
        && actual_root(&t.before, &t.boot_slot)
            .zip(actual_root(&live.slots, &live.boot_slot))
            .is_some_and(|(old, new)| old.name == new.name)
        && t.before
            .iter()
            .all(|before| complete_check_record(before) && live.slots.contains(before))
}

fn unchanged_outside(t: &Task, live: &Snapshot, first: &str, second: &str) -> bool {
    t.before.len() == live.slots.len()
        && t.before
            .iter()
            .filter(|s| s.name != first && s.name != second)
            .all(|before| {
                live.slots
                    .iter()
                    .find(|after| after.name == before.name)
                    .is_some_and(|after| {
                        if after.transaction.is_empty() && !before.transaction.is_empty() {
                            let mut record = before.clone();
                            record.transaction.clear();
                            &record == after
                        } else {
                            before == after
                        }
                    })
            })
}

pub(super) fn unchanged_os(t: &Task, live: &Snapshot) -> bool {
    actual_root(&t.before, &t.boot_slot)
        .zip(actual_root(&live.slots, &live.boot_slot))
        .is_some_and(|(before, running)| {
            before.name == running.name
                && running.boot_status == "good"
                && t.primary == before.name
                && live.primary == before.name
        })
        && check_topology(&t.before)
        && check_topology(&live.slots)
        && loader_pair(&live.slots)
            .is_some_and(|(u, i)| unchanged_outside(t, live, &u.name, &i.name))
}

fn progressed(t: &Task, slot: &SlotEvidence) -> bool {
    let Some(before) = t.before.iter().find(|s| s.name == slot.name) else {
        return false;
    };
    DateTime::parse_from_rfc3339(&slot.installed_at)
        .is_ok_and(|time| time.timestamp() >= t.created_at.timestamp())
        && slot.installed_count > before.installed_count
        && slot.transaction != before.transaction
}

fn record_matches(t: &Task, record: &SlotEvidence, other: &SlotEvidence) -> bool {
    !record.transaction.is_empty()
        && record.transaction == other.transaction
        && record.version == other.version
        && record.manifest_hash == other.manifest_hash
        && (t.version.is_empty() || record.version == t.version)
        && (t.manifest_hash.is_empty() || record.manifest_hash == t.manifest_hash)
        && t.images
            .get(&record.class)
            .is_none_or(|hash| record.sha256 == *hash)
        && (t.source != "local" || record.transaction == t.id)
}

fn loader_installed_evidence(t: &Task, live: &Snapshot) -> Option<Vec<SlotEvidence>> {
    if !unchanged_os(t, live) || loader_pair(&t.before).is_none() {
        return None;
    }
    let (uboot, idb) = loader_pair(&live.slots)?;
    let cohort = [uboot, idb];
    cohort
        .iter()
        .all(|slot| complete_image(slot) && record_matches(t, slot, uboot) && progressed(t, slot))
        .then(|| vec![uboot.clone(), idb.clone()])
}

pub(super) fn installed_evidence(t: &Task, live: &Snapshot) -> Option<Vec<SlotEvidence>> {
    if bootloader_task(t) {
        return loader_installed_evidence(t, live);
    }
    if t.source == "hawkbit"
        && t.images.is_empty()
        && let Some(evidence) = loader_installed_evidence(t, live)
    {
        return Some(evidence);
    }
    if live.primary != t.target_slot {
        return None;
    }
    let (root, boot) = slot_pair(&live.slots, &t.target_slot)?;
    if t.source == "hawkbit" && !unchanged_outside(t, live, &root.name, &boot.name) {
        return None;
    }
    let cohort = [root, boot];
    cohort
        .iter()
        .all(|slot| {
            slot.status == "ok"
                && checksum(&slot.sha256)
                && checksum(&slot.manifest_hash)
                && !slot.version.is_empty()
                && record_matches(t, slot, root)
                && (t.source == "local" || progressed(t, slot))
        })
        .then(|| cohort.into_iter().cloned().collect())
}

pub(super) fn same_installed_evidence(t: &Task, live: &Snapshot) -> bool {
    t.installed.len() == 2
        && t.installed.iter().all(|record| {
            !record.transaction.is_empty()
                && complete_image(record)
                && live.slots.iter().any(|slot| {
                    slot.name == record.name
                        && slot.status == "ok"
                        && slot.version == record.version
                        && slot.manifest_hash == record.manifest_hash
                        && slot.sha256 == record.sha256
                        && slot.installed_at == record.installed_at
                        && slot.installed_count == record.installed_count
                        && (slot.transaction.is_empty() || slot.transaction == record.transaction)
                })
        })
}

pub(super) fn verified_loader_task(t: &Task) -> bool {
    let Some((uboot, idb)) = loader_pair(&t.installed) else {
        return false;
    };
    t.installed.len() == 2
        && loader_images(&t.images)
        && complete_image(uboot)
        && complete_image(idb)
        && record_matches(t, uboot, idb)
        && record_matches(t, idb, uboot)
        && t.version == uboot.version
        && t.manifest_hash == uboot.manifest_hash
        && actual_root(&t.before, &t.boot_slot).is_some_and(|original| {
            original.name == t.target_slot
                && original.bootname == t.target_bootname
                && original.boot_status == "good"
                && t.primary == original.name
        })
        && matches!(t.source.as_str(), "hawkbit" | "local")
}

pub(super) fn verified_reboot_task(t: &Task) -> bool {
    if bootloader_task(t) {
        return verified_loader_task(t);
    }
    let Some((root, boot)) = slot_pair(&t.installed, &t.target_slot) else {
        return false;
    };
    t.installed.len() == 2
        && complete_image(root)
        && complete_image(boot)
        && !root.bootname.is_empty()
        && root.bootname == t.target_bootname
        && record_matches(t, root, boot)
        && record_matches(t, boot, root)
        && root.version == t.version
        && root.manifest_hash == t.manifest_hash
        && matches!(t.source.as_str(), "hawkbit" | "local")
}

pub(super) fn superseded(t: &Task, live: &Snapshot) -> bool {
    let Some((old_root, old_boot)) = slot_pair(&t.installed, &t.target_slot) else {
        return false;
    };
    let Some((root, boot)) = slot_pair(&live.slots, &t.target_slot) else {
        return false;
    };
    complete_image(old_root)
        && complete_image(old_boot)
        && !old_root.transaction.is_empty()
        && old_root.transaction == old_boot.transaction
        && old_root.version == old_boot.version
        && old_root.manifest_hash == old_boot.manifest_hash
        && root.name == old_root.name
        && boot.name == old_boot.name
        && root.bootname == old_root.bootname
        && complete_image(root)
        && complete_image(boot)
        && root.version == boot.version
        && root.manifest_hash == boot.manifest_hash
        && root.transaction == boot.transaction
        && (root.transaction.is_empty() || root.transaction != old_root.transaction)
        && [(old_root, root), (old_boot, boot)]
            .into_iter()
            .all(|(old, new)| {
                new.installed_count > old.installed_count
                    && DateTime::parse_from_rfc3339(&new.installed_at).is_ok_and(|after| {
                        DateTime::parse_from_rfc3339(&old.installed_at)
                            .is_ok_and(|before| after >= before)
                    })
            })
}

pub(super) fn rolled_back(t: &Task, live: &Snapshot, running: &SlotEvidence) -> bool {
    actual_root(&t.before, &t.boot_slot).is_some_and(|old| {
        old.name != t.target_slot
            && old.name == running.name
            && running.boot_status == "good"
            && live.primary == running.name
    }) && same_installed_evidence(t, live)
        && slot_pair(&live.slots, &t.target_slot)
            .is_some_and(|(target, _)| target.boot_status == "bad")
}

pub(super) fn ready_for_reboot(t: &Task, live: &Snapshot) -> bool {
    if !same_installed_evidence(t, live) {
        return false;
    }
    if bootloader_task(t) {
        return unchanged_os(t, live);
    }
    actual_root(&live.slots, &live.boot_slot)
        .zip(actual_root(&t.before, &t.boot_slot))
        .is_some_and(|(running, old)| running.name == old.name && running.name != t.target_slot)
        && live.primary == t.target_slot
        && slot_pair(&live.slots, &t.target_slot)
            .is_some_and(|(target, _)| target.boot_status == "good")
}
