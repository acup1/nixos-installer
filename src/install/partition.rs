use super::*;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::thread;
use std::time::Duration;

pub(super) const SWAP_GUID: &str = "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f";
const MIB: u64 = 1024 * 1024;
const BIOS_SIZE_MIB: u32 = 4;
pub const MAX_DATA_PARTITIONS: usize = 125;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataRole {
    Home,
    Data,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataPartition {
    pub role: DataRole,
    pub filesystem: FileSystem,
    pub size_gib: u32,
    pub name: String,
}

impl DataPartition {
    pub fn mount_point(&self) -> String {
        match self.role {
            DataRole::Home => "/home".into(),
            DataRole::Data => format!("/mnt/{}", self.name),
        }
    }
}

pub fn hibernation_swap_gib() -> Result<u32, String> {
    let info = fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("Cannot determine RAM size for hibernation: {error}"))?;
    let kib = info
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or("Cannot determine RAM size for hibernation.")?;
    u32::try_from(kib.div_ceil(GIB / 1024)).map_err(|_| "RAM size exceeds supported swap.".into())
}

pub(super) fn validate_fresh_sizes(
    request: &InstallRequest,
    data: &[DataPartition],
    disk_size: u64,
) -> Result<(), String> {
    if data.len() > MAX_DATA_PARTITIONS {
        return Err(format!(
            "Add at most {MAX_DATA_PARTITIONS} data or home partitions."
        ));
    }
    let mut mount_points = BTreeSet::new();
    for partition in data {
        if partition.role == DataRole::Data && !safe_path_component(&partition.name) {
            return Err("Data partitions need a directory name below /mnt, such as games.".into());
        }
        let point = partition.mount_point();
        if !mount_points.insert(point.clone()) {
            return Err(format!(
                "The mount point {point} is used by more than one partition."
            ));
        }
        if partition.size_gib == 0 {
            return Err(format!(
                "The partition for {point} needs a size in GiB greater than zero."
            ));
        }
    }
    let gib = u64::from(request.swap_gib) + data.iter().map(|p| u64::from(p.size_gib)).sum::<u64>();
    let needed = gib
        .checked_mul(GIB)
        .and_then(|bytes| bytes.checked_add(ROOT_MINIMUM + 2 * GIB));
    if needed.is_none_or(|bytes| bytes > disk_size) {
        return Err("The disk needs room for the selected partitions and swap, plus at least 8 GiB for / and 2 GiB for boot and alignment.".into());
    }
    Ok(())
}

fn partition_specs(request: &InstallRequest, data: &[DataPartition]) -> Vec<(&'static str, u64)> {
    let boot = match request.boot_mode {
        BootMode::Uefi => (EFI_GUID, u64::from(ESP_SIZE_MIB) * MIB),
        BootMode::Bios => (BIOS_GUID, u64::from(BIOS_SIZE_MIB) * MIB),
    };
    let mut specs = vec![boot];
    if request.partition_swap() {
        specs.push((SWAP_GUID, u64::from(request.swap_gib) * GIB));
    }
    specs.extend(
        data.iter()
            .map(|p| (LINUX_GUID, u64::from(p.size_gib) * GIB)),
    );
    specs.push((LINUX_GUID, 0));
    specs
}

fn sfdisk_script(specs: &[(&str, u64)]) -> String {
    let mut script = String::from("label: gpt\nunit: sectors\n\n");
    for (kind, bytes) in specs {
        if *bytes > 0 {
            write!(script, "size={}MiB, ", bytes / MIB).expect("write to string");
        }
        writeln!(script, "type={kind}").expect("write to string");
    }
    script
}

pub(super) struct NewPartitions<'a> {
    pub(super) root: DeviceIdentity,
    pub(super) esp: Option<DeviceIdentity>,
    pub(super) swap: Option<DeviceIdentity>,
    pub(super) data: Vec<(DeviceIdentity, &'a DataPartition)>,
}

pub(super) fn create<'a>(
    disk: &DeviceIdentity,
    request: &InstallRequest,
    data: &'a [DataPartition],
) -> Result<NewPartitions<'a>, String> {
    let specs = partition_specs(request, data);
    partition_disk(disk, &sfdisk_script(&specs))?;
    let table = read_gpt(&disk.record.path)?;
    let minimum_root = ROOT_MINIMUM
        + if request.file_swap() {
            u64::from(request.swap_gib) * GIB
        } else {
            0
        };
    if table.partitions.len() != specs.len()
        || table
            .partitions
            .iter()
            .zip(&specs)
            .any(|(entry, (kind, bytes))| {
                let size = entry.size.checked_mul(table.sector_size);
                !entry.type_guid.eq_ignore_ascii_case(kind)
                    || if *bytes > 0 {
                        size != Some(*bytes)
                    } else {
                        size.is_none_or(|size| size < minimum_root)
                    }
            })
    {
        return Err("The new GPT does not match the requested partition types and sizes.".into());
    }

    let entries = wait_for_partitions(disk, &table)?;
    let mut entries = entries.into_iter();
    let boot = entries.next().expect("checked boot partition");
    let root = entries.next_back().expect("checked root partition");
    let swap = if request.partition_swap() {
        entries.next()
    } else {
        None
    };
    Ok(NewPartitions {
        root,
        esp: (request.boot_mode == BootMode::Uefi).then_some(boot),
        swap,
        data: entries.zip(data).collect(),
    })
}

fn wait_for_partitions(disk: &DeviceIdentity, table: &Gpt) -> Result<Vec<DeviceIdentity>, String> {
    for _ in 0..20 {
        let devices = probe()?;
        let current = identity(lookup(&devices, &disk.record.path)?)?;
        if !same_physical_disk(&current, disk) {
            return Err("The selected disk changed while partitioning.".into());
        }
        if table.partitions.iter().all(|entry| {
            current
                .record
                .children
                .iter()
                .any(|device| matches_gpt_part(device, entry))
        }) {
            return table
                .partitions
                .iter()
                .map(|entry| identify_gpt_partition(&devices, &current, table.sector_size, entry))
                .collect();
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("The new partitions were not visible to the kernel.".into())
}

pub(super) fn format_filesystem(filesystem: FileSystem, path: &Path) -> Result<(), String> {
    let force = if filesystem == FileSystem::Ext4 {
        "-F"
    } else {
        "-f"
    };
    command(
        filesystem.formatter(),
        [OsStr::new(force), path.as_os_str()],
    )
}

pub(super) struct PreparedSwap {
    pub persistent_path: String,
    number: u64,
}

impl PreparedSwap {
    pub fn current_path(&self) -> Result<PathBuf, String> {
        let path = fs::canonicalize(&self.persistent_path)
            .map_err(|error| format!("{}: {error}", self.persistent_path))?;
        let metadata =
            fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !metadata.file_type().is_block_device() || metadata.rdev() != self.number {
            return Err(format!(
                "{} points to a different swap device.",
                self.persistent_path
            ));
        }
        Ok(path)
    }
}

pub(super) fn prepare_swap(device: &DeviceIdentity, format: bool) -> Result<PreparedSwap, String> {
    ensure_same_idle(device)?;
    let uuid = device
        .record
        .partuuid
        .as_deref()
        .filter(|uuid| {
            let segments: &[usize] = match uuid.len() {
                11 => &[8, 2],
                36 => &[8, 4, 4, 4, 12],
                _ => return false,
            };
            uuid.split('-').map(str::len).eq(segments.iter().copied())
                && uuid
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        })
        .ok_or("The swap partition needs a valid partition ID.")?;
    if format {
        command("mkswap", ["--", device.record.path.as_str()])?;
    }
    Ok(PreparedSwap {
        persistent_path: format!("/dev/disk/by-partuuid/{uuid}"),
        number: device.number,
    })
}

pub(super) fn swap_configuration(
    request: &InstallRequest,
    swap_device: Option<&str>,
) -> Result<String, String> {
    let device = if request.partition_swap() {
        swap_device.ok_or("Missing the prepared swap partition.")?
    } else if request.file_swap() {
        "/swapfile"
    } else {
        return Ok("  swapDevices = lib.mkForce [];\n".into());
    };
    let device = crate::config::quote(device);
    let mut config = format!("  swapDevices = lib.mkForce [ {{ device = {device}; }} ];\n");
    if request.hibernate {
        writeln!(config, "  boot.resumeDevice = {device};").expect("write to string");
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::super::tests::request;
    use super::*;

    #[test]
    fn gpt_script_places_fixed_size_partitions_before_root() {
        let home = DataPartition {
            role: DataRole::Home,
            filesystem: FileSystem::Xfs,
            size_gib: 12,
            name: String::new(),
        };
        let mut request = request(DiskLayout::Erase {
            disk: "/dev/test".into(),
            data: vec![home.clone()],
        });
        request.swap_gib = 8;
        let specs = partition_specs(&request, &[home]);
        assert_eq!(
            specs,
            [
                (EFI_GUID, GIB),
                (SWAP_GUID, 8 * GIB),
                (LINUX_GUID, 12 * GIB),
                (LINUX_GUID, 0)
            ]
        );
        assert!(sfdisk_script(&specs).ends_with(&format!(
            "size=12288MiB, type={LINUX_GUID}\ntype={LINUX_GUID}\n"
        )));
    }

    #[test]
    fn rejects_duplicate_mounts_and_sizes_that_leave_no_space_for_root() {
        let data = DataPartition {
            role: DataRole::Data,
            filesystem: FileSystem::Btrfs,
            size_gib: 12,
            name: "media".into(),
        };
        let request = request(DiskLayout::Erase {
            disk: "/dev/test".into(),
            data: vec![],
        });
        assert!(validate_fresh_sizes(&request, std::slice::from_ref(&data), 40 * GIB).is_ok());
        assert!(validate_fresh_sizes(&request, std::slice::from_ref(&data), 20 * GIB).is_err());
        assert!(
            validate_fresh_sizes(&request, &[data.clone(), data], 40 * GIB)
                .unwrap_err()
                .contains("/mnt/media")
        );
    }
}
