//! NixOS live-system installation. Planning inspects disks and never modifies them.
mod inplace;
mod partition;
pub use partition::{DataPartition, DataRole, MAX_DATA_PARTITIONS, hibernation_swap_gib};
use serde::Deserialize;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GIB: u64 = 1024 * 1024 * 1024;
const ROOT_MINIMUM: u64 = 8 * GIB;
const ESP_SIZE_MIB: u32 = 1024;
const TARGET: &str = "/mnt";
const EFI_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const LINUX_GUID: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const BIOS_GUID: &str = "21686148-6449-6e6f-744e-656564454649";

pub fn suggested_account() -> Option<String> {
    inplace::suggested_account()
}

pub fn default_install_layout() -> DiskLayout {
    if inplace::is_installed_linux_root() {
        DiskLayout::InPlace
    } else {
        DiskLayout::Erase {
            disk: String::new(),
            data: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct BlockDevice {
    #[serde(rename = "name")]
    pub path: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub fstype: Option<String>,
    #[serde(default, deserialize_with = "deserialize_mountpoints")]
    pub mountpoints: Vec<String>,
    #[serde(default)]
    pub children: Vec<BlockDevice>,
    #[serde(rename = "pkname")]
    parent: Option<String>,
    #[serde(rename = "maj:min")]
    number: String,
    partuuid: Option<String>,
    parttype: Option<String>,
    pttype: Option<String>,
    serial: Option<String>,
    wwn: Option<String>,
    #[serde(rename = "ro")]
    read_only: bool,
}

fn deserialize_mountpoints<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    let values = Option::<Vec<Option<String>>>::deserialize(deserializer)?;
    Ok(values.unwrap_or_default().into_iter().flatten().collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskLayout {
    Erase {
        disk: String,
        data: Vec<DataPartition>,
    },
    Alongside {
        disk: String,
        esp: Option<String>,
    },
    Partitions {
        root: String,
        esp: Option<String>,
        boot_disk: Option<String>,
        format_root: bool,
        format_esp: bool,
        swap: Option<String>,
        format_swap: bool,
    },
    Mounted {
        target: PathBuf,
    },
    InPlace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMode {
    Uefi,
    Bios,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileSystem {
    Ext4,
    Btrfs,
    Xfs,
}

impl FileSystem {
    pub const ALL: [Self; 3] = [Self::Ext4, Self::Btrfs, Self::Xfs];

    pub fn name(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Btrfs => "btrfs",
            Self::Xfs => "xfs",
        }
    }

    fn formatter(self) -> &'static str {
        match self {
            Self::Ext4 => "mkfs.ext4",
            Self::Btrfs => "mkfs.btrfs",
            Self::Xfs => "mkfs.xfs",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallRequest {
    pub layout: DiskLayout,
    pub boot_mode: BootMode,
    pub filesystem: FileSystem,
    pub encrypt: bool,
    pub swap_gib: u32,
    pub hibernate: bool,
    pub hostname: String,
    pub username: String,
    pub timezone: String,
    pub locale: String,
    pub keymap: String,
    pub desktop: Option<String>,
    pub network_manager: bool,
}

impl InstallRequest {
    pub fn partition_swap(&self) -> bool {
        self.swap_gib > 0
            && !self.encrypt
            && matches!(
                self.layout,
                DiskLayout::Erase { .. } | DiskLayout::Partitions { swap: Some(_), .. }
            )
    }

    fn file_swap(&self) -> bool {
        self.swap_gib > 0 && !self.partition_swap()
    }
}

#[derive(Clone, Debug)]
pub struct InstallPlan {
    pub request: InstallRequest,
    pub target: PathBuf,
    pub steps: Vec<String>,
    approved_request: InstallRequest,
    checked: CheckedTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckedTarget {
    target: PathBuf,
    boot_disk: Option<DeviceIdentity>,
    root: Option<DeviceIdentity>,
    esp: Option<DeviceIdentity>,
    swap: Option<DeviceIdentity>,
    mounts: Vec<Mount>,
    alongside: Option<AlongsidePlan>,
    in_place: Option<inplace::Account>,
    bios_entry: Option<inplace::BiosEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Gpt {
    label: String,
    id: String,
    device: String,
    unit: String,
    #[serde(rename = "firstlba")]
    first_sector: u64,
    #[serde(rename = "lastlba")]
    last_sector: u64,
    #[serde(rename = "sectorsize")]
    sector_size: u64,
    #[serde(default)]
    partitions: Vec<GptPartition>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct GptPartition {
    node: String,
    start: u64,
    size: u64,
    #[serde(rename = "type")]
    type_guid: String,
    uuid: String,
    #[serde(flatten)]
    attributes: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    start: u64,
    size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AlongsidePlan {
    present: Gpt,
    new_esp: Option<Extent>,
    new_root: Extent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceIdentity {
    record: BlockDevice,
    kernel_path: PathBuf,
    number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    id: u64,
    device_number: String,
    source: PathBuf,
    destination: PathBuf,
    filesystem: String,
    read_only: bool,
}

#[derive(Deserialize)]
struct DeviceList {
    blockdevices: Vec<BlockDevice>,
}

pub fn probe() -> Result<Vec<BlockDevice>, String> {
    linux()?;
    let data = capture(
        "lsblk",
        [
            "--json",
            "--bytes",
            "--paths",
            "--tree",
            "--output",
            "NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS,PKNAME,MAJ:MIN,PARTUUID,PARTTYPE,PTTYPE,SERIAL,WWN,RO",
        ],
    )?;
    let devices: DeviceList =
        serde_json::from_slice(&data).map_err(|error| format!("Invalid lsblk data: {error}"))?;
    Ok(devices.blockdevices)
}

pub fn edit_partitions(disk: &str) -> Result<(), String> {
    linux()?;
    tools_available(&["cfdisk", "udevadm"])?;
    require_root()?;
    require_terminal()?;
    let devices = probe()?;
    let device = lookup(&devices, disk)?;
    require_kind(device, "disk")?;
    require_idle_tree(device)?;
    check_exclusive(device)?;
    let mut editor = Command::new("cfdisk");
    editor.args(["--lock=yes", "--", disk]);
    if capture("infocmp", [] as [&str; 0]).is_err()
        && capture("infocmp", ["xterm-256color"]).is_ok()
    {
        editor.env("TERM", "xterm-256color");
    }
    run_command("cfdisk", editor)?;
    command("udevadm", ["settle"])?;
    Ok(())
}

pub fn build_plan(request: InstallRequest) -> Result<InstallPlan, String> {
    linux()?;
    validate_settings(&request)?;
    let checked = inspect_target(&request, &probe()?, &mounts()?)?;
    let target = checked.target.clone();
    let steps = describe_steps(&request, &checked);
    Ok(InstallPlan {
        approved_request: request.clone(),
        request,
        target,
        steps,
        checked,
    })
}

pub fn confirmation_token(plan: &InstallPlan) -> String {
    match &plan.approved_request.layout {
        DiskLayout::Erase { disk, .. } => format!("ERASE {disk}"),
        DiskLayout::Alongside { disk, .. } => format!("INSTALL ALONGSIDE {disk}"),
        DiskLayout::Partitions {
            root,
            esp,
            format_root,
            format_esp,
            swap,
            format_swap,
            ..
        } => {
            let mut formatted = Vec::new();
            if *format_root {
                formatted.push(root.as_str());
            }
            if *format_esp && let Some(esp) = esp {
                formatted.push(esp.as_str());
            }
            if plan.approved_request.partition_swap()
                && *format_swap
                && let Some(swap) = swap
            {
                formatted.push(swap.as_str());
            }
            if formatted.is_empty() {
                format!("INSTALL {root}")
            } else {
                format!("ERASE {}", formatted.join(" "))
            }
        }
        DiskLayout::Mounted { target } => format!("INSTALL {}", target.display()),
        DiskLayout::InPlace => "INSTALL IN PLACE /".into(),
    }
}

fn linux() -> Result<(), String> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err("Installation requires a Linux environment with the NixOS installer tools.".into())
    }
}

fn validate_settings(request: &InstallRequest) -> Result<(), String> {
    crate::config::validate_hostname(&request.hostname)?;
    crate::config::validate_username(&request.username, false)?;
    if request.timezone.is_empty()
        || !request.timezone.split('/').all(safe_path_component)
        || !plain_setting(&request.locale)
        || !plain_setting(&request.keymap)
    {
        return Err("Timezone, locale and keymap must be valid, non-empty identifiers.".into());
    }
    desktop_lines(request.desktop.as_deref())?;
    if request.boot_mode == BootMode::Uefi && !Path::new("/sys/firmware/efi").is_dir() {
        return Err("UEFI was selected but the installer was not booted in UEFI mode.".into());
    }
    if request.boot_mode == BootMode::Bios && !matches!(std::env::consts::ARCH, "x86_64" | "x86") {
        return Err("BIOS bootloader installation requires an x86 host.".into());
    }
    if request.swap_gib > 512 {
        return Err("Swap must be no larger than 512 GiB.".into());
    }
    if request.hibernate {
        if !request.partition_swap() {
            return Err(
                "Hibernation requires a dedicated swap partition without root encryption.".into(),
            );
        }
        let minimum = hibernation_swap_gib()?;
        if matches!(request.layout, DiskLayout::Erase { .. }) && request.swap_gib < minimum {
            return Err(format!(
                "Hibernation requires at least {minimum} GiB of swap on this machine."
            ));
        }
    }
    Ok(())
}

fn plain_setting(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.@+-".contains(&c))
}

pub(crate) fn safe_path_component(value: &str) -> bool {
    plain_setting(value) && value != "." && value != ".."
}

fn desktop_lines(values: Option<&str>) -> Result<&'static str, String> {
    match values {
        None | Some("none" | "minimal" | "server") => Ok(""),
        Some("gnome") => Ok(
            "  services.displayManager.gdm.enable = true;\n  services.desktopManager.gnome.enable = true;\n",
        ),
        Some("plasma" | "kde" | "plasma6") => Ok(
            "  services.displayManager.sddm.enable = true;\n  services.desktopManager.plasma6.enable = true;\n",
        ),
        Some("xfce") => Ok(
            "  services.xserver.enable = true;\n  services.xserver.displayManager.lightdm.enable = true;\n  services.xserver.desktopManager.xfce.enable = true;\n",
        ),
        Some("niri") => {
            Ok("  services.displayManager.gdm.enable = true;\n  programs.niri.enable = true;\n")
        }
        Some("hyprland" | "hypr") => {
            Ok("  services.displayManager.gdm.enable = true;\n  programs.hyprland.enable = true;\n")
        }
        Some(value) => Err(format!("Unsupported desktop: {value}")),
    }
}

fn inspect_target(
    request: &InstallRequest,
    devices: &[BlockDevice],
    current_mounts: &[Mount],
) -> Result<CheckedTarget, String> {
    let mut checked = CheckedTarget {
        target: PathBuf::from(TARGET),
        boot_disk: None,
        root: None,
        esp: None,
        swap: None,
        mounts: Vec::new(),
        alongside: None,
        in_place: None,
        bios_entry: None,
    };
    let required_root_size = ROOT_MINIMUM
        + u64::from(if request.file_swap() {
            request.swap_gib
        } else {
            0
        }) * GIB;

    match &request.layout {
        DiskLayout::InPlace => inplace::inspect(request, devices, current_mounts, &mut checked)?,
        DiskLayout::Erase { disk, data } => {
            if disk.is_empty() {
                return Err("Choose a target disk on the Installation storage step.".into());
            }
            let device = lookup(devices, disk)?;
            require_kind(device, "disk")?;
            require_idle_tree(device)?;
            partition::validate_fresh_sizes(request, data, device.size)?;
            require_unused_target(&checked.target, current_mounts)?;
            checked.boot_disk = Some(identity(device)?);
        }
        DiskLayout::Alongside { disk, esp } => {
            if request.boot_mode != BootMode::Uefi {
                return Err("Guided alongside installation requires UEFI and a GPT disk.".into());
            }
            let device = lookup(devices, disk)?;
            require_kind(device, "disk")?;
            require_idle_tree(device)?;
            require_unused_target(&checked.target, current_mounts)?;
            if let Some(esp) = esp {
                let partition = lookup(devices, esp)?;
                require_kind(partition, "part")?;
                if parent_disk(devices, partition)?.number != device.number
                    || !is_efi_partition(partition)
                    || partition.fstype.as_deref() != Some("vfat")
                    || partition.size < 256 * 1024 * 1024
                {
                    return Err(format!(
                        "{esp} must be a FAT EFI partition of at least 256 MiB on {disk}."
                    ));
                }
                checked.esp = Some(identity(partition)?);
            }
            checked.alongside = Some(plan_alongside(
                read_gpt(disk)?,
                device.size,
                required_root_size,
                esp.is_none(),
            )?);
            checked.boot_disk = Some(identity(device)?);
        }
        DiskLayout::Partitions {
            root,
            esp,
            boot_disk,
            format_root,
            format_esp,
            swap,
            format_swap,
        } => {
            if root.is_empty() {
                return Err("Choose a root partition on the Installation storage step.".into());
            }
            let root_device = lookup(devices, root)?;
            require_kind(root_device, "part")?;
            require_idle_tree(root_device)?;
            let parent = parent_disk(devices, root_device)?;
            reject_live_disk(parent, current_mounts)?;
            require_unused_target(&checked.target, current_mounts)?;
            if root_device.size < required_root_size {
                return Err(format!(
                    "{root} is too small for the requested system and swap."
                ));
            }
            if request.encrypt && !*format_root {
                return Err("Encrypting a root partition requires formatting it.".into());
            }
            if !*format_root && root_device.fstype.as_deref() != Some(request.filesystem.name()) {
                return Err(format!(
                    "{root} is not a {} filesystem. Mount it yourself to use a more complex layout.",
                    request.filesystem.name()
                ));
            }
            checked.root = Some(identity(root_device)?);
            if request.partition_swap() {
                let swap_path = swap.as_deref().ok_or("Select a swap partition.")?;
                let device = lookup(devices, swap_path)?;
                require_kind(device, "part")?;
                require_idle_tree(device)?;
                if device.number == root_device.number
                    || esp.as_deref() == Some(swap_path)
                    || parent_disk(devices, device)?.number != parent.number
                    || !device.is_swap_partition()
                {
                    return Err("Select a separate swap partition on the same disk as root.".into());
                }
                if !*format_swap && device.fstype.as_deref() != Some("swap") {
                    return Err(format!(
                        "{swap_path} must be formatted as swap. Select Format swap partition or use a swap file."
                    ));
                }
                if request.hibernate {
                    let minimum = hibernation_swap_gib()?;
                    if device.size < u64::from(minimum) * GIB {
                        return Err(format!(
                            "Hibernation requires a swap partition of at least {minimum} GiB."
                        ));
                    }
                }
                checked.swap = Some(identity(device)?);
            }

            match request.boot_mode {
                BootMode::Uefi => {
                    if boot_disk.is_some() {
                        return Err(
                            "UEFI uses an EFI system partition, not a BIOS boot disk.".into()
                        );
                    }
                    let esp = esp
                        .as_deref()
                        .ok_or("Select an EFI system partition for UEFI.")?;
                    let esp_device = lookup(devices, esp)?;
                    require_kind(esp_device, "part")?;
                    require_idle_tree(esp_device)?;
                    let esp_parent = parent_disk(devices, esp_device)?;
                    reject_live_disk(esp_parent, current_mounts)?;
                    if esp_device.number == root_device.number {
                        return Err("The root and EFI system partitions must be different.".into());
                    }
                    if !is_efi_partition(esp_device) {
                        return Err(format!("{esp} is not marked as an EFI system partition."));
                    }
                    if (!*format_esp && esp_device.fstype.as_deref() != Some("vfat"))
                        || esp_device.size < 256 * 1024 * 1024
                    {
                        return Err(format!(
                            "{esp} must be at least 256 MiB and use FAT, or be marked for formatting."
                        ));
                    }
                    checked.esp = Some(identity(esp_device)?);
                }
                BootMode::Bios => {
                    if esp.is_some() || *format_esp {
                        return Err(
                            "BIOS installations use a GRUB disk, not an EFI partition.".into()
                        );
                    }
                    let disk = boot_disk.as_deref().unwrap_or(parent.path.as_str());
                    if disk != parent.path {
                        return Err(
                            "Use the disk containing the root partition for BIOS GRUB.".into()
                        );
                    }
                    require_bios_partition(parent)?;
                    checked.boot_disk = Some(identity(parent)?);
                }
            }
        }
        DiskLayout::Mounted { target } => {
            if request.encrypt {
                return Err(
                    "An already mounted target must be encrypted before running the installer."
                        .into(),
                );
            }
            checked.target = canonical_target(target)?;
            checked.mounts = target_mounts(&checked.target, current_mounts)?;
            let root = checked
                .mounts
                .first()
                .ok_or("Mount the target root filesystem first.")?;
            if root.filesystem != request.filesystem.name() {
                return Err(format!(
                    "{} is mounted as {}, but {} was selected.",
                    checked.target.display(),
                    root.filesystem,
                    request.filesystem.name()
                ));
            }
            for mount in &checked.mounts {
                let device = mount_device(devices, mount).ok_or_else(|| {
                    format!(
                        "{} is not backed by a recognized block device.",
                        mount.destination.display()
                    )
                })?;
                if device
                    .mountpoints
                    .iter()
                    .any(|point| !Path::new(point).starts_with(&checked.target))
                    || current_mounts.iter().any(|other| {
                        mount_device(devices, other)
                            .is_some_and(|other| other.number == device.number)
                            && !other.destination.starts_with(&checked.target)
                    })
                {
                    return Err(format!(
                        "{} is also in use outside the installation target.",
                        device.path
                    ));
                }
            }
            let root_device =
                mount_device(devices, root).ok_or("Cannot identify the target disk.")?;
            checked.root = Some(identity(root_device)?);
            match request.boot_mode {
                BootMode::Uefi => {
                    let boot = checked
                        .mounts
                        .iter()
                        .find(|mount| mount.destination == checked.target.join("boot"))
                        .ok_or("Mount the EFI system partition at the target's boot directory.")?;
                    let device =
                        mount_device(devices, boot).ok_or("Cannot identify the EFI partition.")?;
                    if boot.filesystem != "vfat" || !is_efi_partition(device) {
                        return Err(
                            "The target's boot mount must be a FAT EFI system partition.".into(),
                        );
                    }
                    checked.esp = Some(identity(device)?);
                }
                BootMode::Bios => {
                    let disk = parent_disk(devices, root_device)?;
                    reject_live_disk(disk, current_mounts)?;
                    require_bios_partition(disk)?;
                    checked.boot_disk = Some(identity(disk)?);
                }
            }
            require_fresh_target(&checked.target)?;
        }
    }
    Ok(checked)
}

fn is_efi_partition(device: &BlockDevice) -> bool {
    device.parttype.as_deref().is_some_and(|value| {
        value.eq_ignore_ascii_case(EFI_GUID) || value.eq_ignore_ascii_case("0xef")
    })
}

impl BlockDevice {
    pub fn is_swap_partition(&self) -> bool {
        self.kind == "part"
            && (self.fstype.as_deref() == Some("swap")
                || self.parttype.as_deref().is_some_and(|kind| {
                    kind.eq_ignore_ascii_case(partition::SWAP_GUID) || kind == "0x82"
                }))
    }

    pub fn idle_in_inventory(&self) -> bool {
        !self.read_only
            && self.mountpoints.is_empty()
            && self.children.iter().all(Self::idle_in_inventory)
    }
}

fn read_gpt(disk: &str) -> Result<Gpt, String> {
    #[derive(Deserialize)]
    struct TableOutput {
        partitiontable: Gpt,
    }
    let json = capture("sfdisk", ["--json", "--", disk])?;
    let table: TableOutput = serde_json::from_slice(&json)
        .map_err(|error| format!("Cannot read GPT geometry for {disk}: {error}"))?;
    if table.partitiontable.device != disk || table.partitiontable.label != "gpt" {
        return Err(format!(
            "{disk} does not have a supported GPT partition table."
        ));
    }
    Ok(table.partitiontable)
}

fn plan_alongside(
    present: Gpt,
    disk_bytes: u64,
    root_bytes: u64,
    create_esp: bool,
) -> Result<AlongsidePlan, String> {
    const MIB: u64 = 1024 * 1024;
    let sector = present.sector_size;
    if present.label != "gpt"
        || present.unit != "sectors"
        || !matches!(sector, 512 | 1024 | 2048 | 4096)
        || !disk_bytes.is_multiple_of(sector)
        || present.first_sector < 2
        || present.first_sector > present.last_sector
        || present.last_sector >= disk_bytes / sector
    {
        return Err("Invalid usable sector range or sector size reported for GPT.".into());
    }
    let alignment = MIB / sector;
    let end = present.last_sector + 1;
    let mut occupied: Vec<_> = present.partitions.iter().collect();
    occupied.sort_by_key(|part| part.start);
    let mut cursor = present.first_sector;
    let mut largest = Extent { start: 0, size: 0 };
    let mut consider_gap = |from: u64, to: u64| {
        let start = from.div_ceil(alignment) * alignment;
        let end = to / alignment * alignment;
        let size = end.saturating_sub(start);
        if size > largest.size {
            largest = Extent { start, size };
        }
    };
    for partition in occupied {
        let after = partition
            .start
            .checked_add(partition.size)
            .ok_or("GPT range overflow.")?;
        if partition.size == 0 || partition.start < cursor || after > end {
            return Err("The GPT contains overlapping or out-of-bounds partitions.".into());
        }
        consider_gap(cursor, partition.start);
        cursor = after;
    }
    consider_gap(cursor, end);
    let esp_sectors = if create_esp {
        u64::from(ESP_SIZE_MIB) * MIB / sector
    } else {
        0
    };
    if largest.size < esp_sectors + root_bytes.div_ceil(sector) {
        return Err("No contiguous unallocated GPT space is large enough. Existing partitions are left unchanged.".into());
    }
    Ok(AlongsidePlan {
        present,
        new_esp: create_esp.then_some(Extent {
            start: largest.start,
            size: esp_sectors,
        }),
        new_root: Extent {
            start: largest.start + esp_sectors,
            size: largest.size - esp_sectors,
        },
    })
}

fn verify_append<'a>(
    planned: &AlongsidePlan,
    updated: &'a Gpt,
) -> Result<(&'a GptPartition, Option<&'a GptPartition>), String> {
    let old = &planned.present;
    let additions = 1 + usize::from(planned.new_esp.is_some());
    if updated.label != old.label
        || updated.id != old.id
        || updated.device != old.device
        || updated.unit != old.unit
        || updated.first_sector != old.first_sector
        || updated.last_sector != old.last_sector
        || updated.sector_size != old.sector_size
        || updated.partitions.len() != old.partitions.len() + additions
        || !old
            .partitions
            .iter()
            .all(|part| updated.partitions.contains(part))
    {
        return Err("The partition table differs from the approved additions. No new partitions will be formatted.".into());
    }
    let lookup = |extent: Extent, guid: &str| {
        updated
            .partitions
            .iter()
            .find(|part| {
                part.start == extent.start
                    && part.size == extent.size
                    && part.type_guid.eq_ignore_ascii_case(guid)
                    && !old
                        .partitions
                        .iter()
                        .any(|old| old.node == part.node || old.uuid == part.uuid)
            })
            .ok_or("A new partition did not match the approved geometry.")
    };
    let root = lookup(planned.new_root, LINUX_GUID)?;
    let esp = planned
        .new_esp
        .map(|extent| lookup(extent, EFI_GUID))
        .transpose()?;
    Ok((root, esp))
}

fn require_bios_partition(disk: &BlockDevice) -> Result<(), String> {
    let valid = match disk.pttype.as_deref() {
        Some("dos") => true,
        Some("gpt") => disk.children.iter().any(|partition| {
            partition
                .parttype
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(BIOS_GUID))
        }),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{} needs an MBR partition table, or GPT with a BIOS boot partition, for GRUB.",
            disk.path
        ))
    }
}

fn lookup<'a>(devices: &'a [BlockDevice], path: &str) -> Result<&'a BlockDevice, String> {
    if !path.starts_with("/dev/") || !path[5..].split('/').all(safe_path_component) {
        return Err(format!("Invalid block-device path: {path}"));
    }
    find_device(devices, &|device| device.path == path)
        .ok_or_else(|| format!("Block device is not present: {path}"))
}

fn lookup_number<'a>(devices: &'a [BlockDevice], number: &str) -> Option<&'a BlockDevice> {
    find_device(devices, &|device| device.number == number)
}

fn mount_device<'a>(devices: &'a [BlockDevice], mount: &Mount) -> Option<&'a BlockDevice> {
    lookup_number(devices, &mount.device_number).or_else(|| {
        // Btrfs subvolumes can have an anonymous mount device number.
        if !mount.source.starts_with("/dev") {
            return None;
        }
        let path = fs::canonicalize(&mount.source).ok()?;
        let metadata = fs::metadata(path).ok()?;
        metadata.file_type().is_block_device().then_some(())?;
        lookup_number(devices, &device_number(metadata.rdev()))
    })
}

fn find_device<'a>(
    devices: &'a [BlockDevice],
    matches: &impl Fn(&BlockDevice) -> bool,
) -> Option<&'a BlockDevice> {
    for device in devices {
        if matches(device) {
            return Some(device);
        }
        if let Some(child) = find_device(&device.children, matches) {
            return Some(child);
        }
    }
    None
}

fn parent_disk<'a>(
    devices: &'a [BlockDevice],
    mut device: &'a BlockDevice,
) -> Result<&'a BlockDevice, String> {
    for _ in 0..16 {
        if device.kind == "disk" {
            return Ok(device);
        }
        if device.kind != "part" {
            let kernel = identity(device)?.kernel_path;
            let slaves = fs::read_dir(kernel.join("slaves")).map_err(|error| {
                format!("Cannot inspect devices underlying {}: {error}", device.path)
            })?;
            if slaves.take(2).count() > 1 {
                return Err(format!(
                    "Cannot infer a single boot disk underlying {}.",
                    device.path
                ));
            }
        }
        let parent = device
            .parent
            .as_deref()
            .ok_or_else(|| format!("Cannot find the physical disk underlying {}.", device.path))?;
        device = lookup(devices, parent)?;
    }
    Err("Block device ancestry is too deep to validate.".into())
}

fn require_kind(device: &BlockDevice, kind: &str) -> Result<(), String> {
    if device.kind != kind || device.read_only {
        return Err(format!("{} is not a writable {kind} device.", device.path));
    }
    identity(device)?;
    Ok(())
}

fn identity(device: &BlockDevice) -> Result<DeviceIdentity, String> {
    if !safe_device_number(&device.number) {
        return Err(format!("Invalid kernel device number for {}.", device.path));
    }
    let metadata =
        fs::metadata(&device.path).map_err(|error| format!("{}: {error}", device.path))?;
    if !metadata.file_type().is_block_device() || device_number(metadata.rdev()) != device.number {
        return Err(format!("{} is not a block device.", device.path));
    }
    let kernel_path = fs::canonicalize(Path::new("/sys/dev/block").join(&device.number))
        .map_err(|error| format!("Cannot verify kernel device {}: {error}", device.path))?;
    Ok(DeviceIdentity {
        record: device.clone(),
        kernel_path,
        number: metadata.rdev(),
    })
}

fn device_number(number: u64) -> String {
    // Linux dev_t encodes values beyond the legacy eight-bit major/minor.
    let major = ((number >> 8) & 0xfff) | ((number >> 32) & !0xfff);
    let minor = (number & 0xff) | ((number >> 12) & !0xff);
    format!("{major}:{minor}")
}

fn safe_device_number(number: &str) -> bool {
    number.split_once(':').is_some_and(|(major, minor)| {
        !major.is_empty()
            && !minor.is_empty()
            && major
                .bytes()
                .chain(minor.bytes())
                .all(|c| c.is_ascii_digit())
    })
}

fn require_idle_tree(device: &BlockDevice) -> Result<(), String> {
    let mount_info = mounts()?;
    let swap_devices = fs::read_to_string("/proc/swaps")
        .map_err(|error| format!("Cannot inspect active swaps: {error}"))?;
    require_idle(device, &mount_info, &swap_devices)
}

fn require_idle(
    device: &BlockDevice,
    mount_info: &[Mount],
    swap_devices: &str,
) -> Result<(), String> {
    let identity = identity(device)?;
    let in_use = !device.mountpoints.is_empty()
        || mount_info
            .iter()
            .any(|mount| mount.device_number == device.number)
        || swap_devices.lines().skip(1).any(|line| {
            line.split_whitespace()
                .next()
                .and_then(|path| decode_mount_path(path).ok())
                .and_then(|path| fs::metadata(path).ok())
                .is_some_and(|metadata| {
                    metadata.file_type().is_block_device() && metadata.rdev() == identity.number
                })
        });
    if in_use || device.read_only {
        return Err(format!(
            "{} is mounted, active as swap, or read-only.",
            device.path
        ));
    }
    let mut holders = fs::read_dir(identity.kernel_path.join("holders"))
        .map_err(|error| format!("Cannot inspect holders for {}: {error}", device.path))?;
    if holders.next().is_some() {
        return Err(format!(
            "{} is in use by another block device.",
            device.path
        ));
    }
    for child in &device.children {
        require_idle(child, mount_info, swap_devices)?;
    }
    Ok(())
}

fn reject_live_disk(disk: &BlockDevice, current_mounts: &[Mount]) -> Result<(), String> {
    fn uses_host_mount(device: &BlockDevice, current_mounts: &[Mount]) -> bool {
        device
            .mountpoints
            .iter()
            .any(|point| point != "[SWAP]" && !Path::new(point).starts_with(TARGET))
            || current_mounts.iter().any(|mount| {
                mount.device_number == device.number && !mount.destination.starts_with(TARGET)
            })
            || device
                .children
                .iter()
                .any(|child| uses_host_mount(child, current_mounts))
    }
    if uses_host_mount(disk, current_mounts) {
        Err(format!("{} backs a filesystem outside /mnt.", disk.path))
    } else {
        Ok(())
    }
}

fn target_mounts(target: &Path, current_mounts: &[Mount]) -> Result<Vec<Mount>, String> {
    let top_level: Vec<_> = current_mounts
        .iter()
        .filter(|mount| mount.destination == target)
        .collect();
    let [root] = top_level.as_slice() else {
        if top_level.is_empty() {
            return Err(format!(
                "{} is not mounted. Mount the root filesystem there or choose an installation layout that prepares the disk.",
                target.display()
            ));
        }
        return Err(format!(
            "{} has multiple mounts. Leave just the target root mounted there.",
            target.display()
        ));
    };
    let mut target_mounts = vec![(*root).clone()];
    for mount in current_mounts
        .iter()
        .filter(|mount| mount.destination != target && mount.destination.starts_with(target))
    {
        target_mounts.push(mount.clone());
    }
    for mount in &target_mounts {
        if mount.read_only || !safe_absolute_path(&mount.destination) {
            return Err(format!(
                "{} is read-only or has an unsafe mount path.",
                mount.destination.display()
            ));
        }
        if target_mounts
            .iter()
            .filter(|other| other.destination == mount.destination)
            .count()
            != 1
        {
            return Err(format!(
                "{} has multiple stacked mounts.",
                mount.destination.display()
            ));
        }
    }
    Ok(target_mounts)
}

fn canonical_target(target: &Path) -> Result<PathBuf, String> {
    if !target.starts_with(TARGET) || !safe_absolute_path(target) {
        return Err("Use /mnt or a path below /mnt with ordinary directory names.".into());
    }
    let canonical =
        fs::canonicalize(target).map_err(|error| format!("{}: {error}", target.display()))?;
    if canonical != target || !canonical.is_dir() {
        return Err(format!(
            "{} must be a real directory without symbolic links.",
            target.display()
        ));
    }
    Ok(canonical)
}

fn safe_absolute_path(path: &Path) -> bool {
    path.to_str().is_some_and(|value| {
        value.starts_with('/') && value[1..].split('/').all(safe_path_component)
    })
}

fn require_unused_target(target: &Path, current_mounts: &[Mount]) -> Result<(), String> {
    if current_mounts
        .iter()
        .any(|mount| mount.destination.starts_with(target))
    {
        return Err(format!(
            "{} already has a mount. Unmount it or select the mounted layout.",
            target.display()
        ));
    }
    match fs::symlink_metadata(target) {
        Ok(_) => {
            canonical_target(target)?;
            if fs::read_dir(target)
                .map_err(|error| format!("{}: {error}", target.display()))?
                .next()
                .is_some()
            {
                return Err(format!("{} must be empty.", target.display()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", target.display())),
    }
    Ok(())
}

fn require_fresh_target(target: &Path) -> Result<(), String> {
    // Never let privileged installer writes traverse links stored on a reused volume.
    for name in ["boot", "etc", "nix", "var", "home", "root", "run", "tmp"] {
        let path = target.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!("{} must be a real directory.", path.display()));
            }
            Ok(_) if matches!(name, "etc" | "nix" | "var") => {
                if fs::read_dir(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?
                    .next()
                    .is_some()
                {
                    return Err(format!(
                        "{} is populated. Use a fresh installation target to preserve existing configuration.",
                        path.display()
                    ));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Ok(())
}

fn mounts() -> Result<Vec<Mount>, String> {
    let data = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("Cannot inspect mounted filesystems: {error}"))?;
    parse_mounts(&data)
}

fn parse_mounts(data: &str) -> Result<Vec<Mount>, String> {
    data.lines()
        .map(|line| {
            let (attributes, filesystem) = line
                .split_once(" - ")
                .ok_or("Malformed Linux mount info.")?;
            let fields: Vec<_> = attributes.split_whitespace().collect();
            if fields.len() < 6 || !safe_device_number(fields[2]) {
                return Err("Malformed Linux mount info.".into());
            }
            let mut filesystem = filesystem.split_whitespace();
            let fs_type = filesystem.next().ok_or("Missing filesystem type.")?;
            let source = filesystem.next().ok_or("Missing filesystem source.")?;
            Ok(Mount {
                id: fields[0].parse::<u64>().map_err(|_| "Invalid mount ID.")?,
                device_number: fields[2].into(),
                source: decode_mount_path(source)?,
                destination: decode_mount_path(fields[4])?,
                filesystem: fs_type.into(),
                read_only: fields[5].split(',').any(|option| option == "ro"),
            })
        })
        .collect()
}

fn decode_mount_path(value: &str) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let mut remaining = value.as_bytes();
    let mut decoded = Vec::new();
    while let Some((&first, tail)) = remaining.split_first() {
        if first == b'\\' {
            let escape = tail.get(..3).ok_or("Malformed mounted path.")?;
            let [a, b, c] = escape else {
                return Err("Malformed mounted path.".into());
            };
            if !matches!(a, b'0'..=b'3') || !matches!(b, b'0'..=b'7') || !matches!(c, b'0'..=b'7') {
                return Err("Malformed mounted path.".into());
            }
            decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
            remaining = &tail[3..];
        } else {
            decoded.push(first);
            remaining = tail;
        }
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

fn describe_steps(request: &InstallRequest, checked: &CheckedTarget) -> Vec<String> {
    let mut steps = Vec::new();
    match &request.layout {
        DiskLayout::InPlace => return inplace::describe(request, checked),
        DiskLayout::Erase { disk, data } => {
            steps.push(format!("Erase all filesystems and partitions on {disk}."));
            steps.push(match request.boot_mode {
                BootMode::Uefi => format!(
                    "Create a GPT and a {} MiB FAT32 EFI partition at /boot.",
                    ESP_SIZE_MIB
                ),
                BootMode::Bios => "Create a GPT and a 4 MiB BIOS boot partition for GRUB.".into(),
            });
            for partition in data {
                steps.push(format!(
                    "Create and format {} as {} ({} GiB).",
                    partition.mount_point(),
                    partition.filesystem.name(),
                    partition.size_gib
                ));
            }
            steps.push(format!(
                "Use the remaining space for / ({}).",
                request.filesystem.name()
            ));
        }
        DiskLayout::Alongside { disk, esp } => {
            let alongside = checked
                .alongside
                .as_ref()
                .expect("validated alongside geometry");
            let range = alongside.new_root;
            steps.push(format!(
                "Add a root partition on {disk} in unallocated GPT sectors {}–{} ({} GiB). Existing partitions will not be resized or formatted.",
                range.start,
                range.start + range.size - 1,
                range.size * alongside.present.sector_size / GIB,
            ));
            if let Some(esp) = esp {
                steps.push(format!(
                    "Keep the existing EFI partition {esp} and add a separate GRUB boot entry."
                ));
            } else if let Some(region) = alongside.new_esp {
                steps.push(format!(
                    "Add a new {} MiB EFI partition in GPT sectors {}–{}.",
                    ESP_SIZE_MIB,
                    region.start,
                    region.start + region.size - 1
                ));
            }
        }
        DiskLayout::Partitions {
            root,
            esp,
            format_root,
            format_esp,
            swap,
            format_swap,
            ..
        } => {
            steps.push(if *format_root {
                format!("Format root partition {root}.")
            } else {
                format!("Keep the filesystem on {root} and mount it as root.")
            });
            if let Some(esp) = esp {
                steps.push(if *format_esp {
                    format!("Format EFI system partition {esp} as FAT32.")
                } else {
                    format!("Keep EFI system partition {esp} and mount it at /boot.")
                });
            }
            if request.partition_swap()
                && let Some(swap) = swap
            {
                steps.push(if *format_swap {
                    format!("Format {swap} as swap.")
                } else {
                    format!("Use the existing swap filesystem on {swap}.")
                });
            }
        }
        DiskLayout::Mounted { .. } => {
            steps.push(format!(
                "Use the existing mounts beneath {}.",
                checked.target.display()
            ));
            for mount in &checked.mounts {
                steps.push(format!(
                    "  {} ({}, device {})",
                    mount.destination.display(),
                    mount.filesystem,
                    mount.device_number
                ));
            }
        }
    }
    if request.encrypt {
        steps.push("Encrypt the new root with LUKS2 and prompt for its passphrase.".into());
    }
    if request.partition_swap() && matches!(request.layout, DiskLayout::Erase { .. }) {
        steps.push(format!("Create a {} GiB swap partition.", request.swap_gib));
    } else if request.file_swap() {
        steps.push(format!(
            "Create a {} GiB swap file on the target root.",
            request.swap_gib
        ));
    }
    if request.hibernate {
        steps.push("Configure hibernation resume from the swap partition.".into());
    }
    steps.push(format!(
        "Install NixOS at {} using {} with {}.",
        checked.target.display(),
        request.filesystem.name(),
        match request.boot_mode {
            BootMode::Uefi if shared_esp(request) => "UEFI GRUB with a separate boot entry",
            BootMode::Uefi => "UEFI systemd-boot",
            BootMode::Bios => "BIOS GRUB",
        }
    ));
    steps.push(format!(
        "Set host {}, user {}, timezone {}, locale {}, keymap {}, desktop {}, network {}.",
        request.hostname,
        request.username,
        request.timezone,
        request.locale,
        request.keymap,
        request.desktop.as_deref().unwrap_or("none"),
        if request.network_manager {
            "NetworkManager"
        } else {
            "DHCP"
        }
    ));
    steps.push(format!(
        "Prompt for the password of {} after installation.",
        request.username
    ));
    steps
}

pub fn apply_plan(
    plan: &InstallPlan,
    configure: impl FnOnce(&Path, &InstallRequest) -> Result<(), String>,
) -> Result<(), String> {
    linux()?;
    require_root()?;
    require_terminal()?;
    if plan.request != plan.approved_request || plan.target != plan.checked.target {
        return Err(
            "The installation plan was edited. Review a fresh plan before applying.".into(),
        );
    }
    let request = &plan.approved_request;
    validate_settings(request)?;
    preflight(request)?;
    let state_version = "26.05";
    let checked = inspect_target(request, &probe()?, &mounts()?)?;
    if checked != plan.checked {
        return Err(
            "Devices or mounts changed after review. Review a fresh installation plan.".into(),
        );
    }

    if matches!(request.layout, DiskLayout::InPlace) {
        return inplace::apply(request, &checked, state_version, configure);
    }

    let mut session = InstallMounts::default();
    let result = install(request, &checked, state_version, &mut session, configure);
    let cleanup = session.clean();
    match (result, cleanup) {
        (Ok(()), clean) => clean,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}\nCould not clean up installer mounts: {cleanup}"
        )),
    }
}

fn preflight(request: &InstallRequest) -> Result<(), String> {
    if request.boot_mode == BootMode::Uefi
        && !mounts()?.iter().any(|mount| {
            mount.destination == Path::new("/sys/firmware/efi/efivars")
                && mount.filesystem == "efivarfs"
                && !mount.read_only
        })
    {
        return Err("The UEFI variable filesystem must be mounted and writable before installing systemd-boot.".into());
    }
    if matches!(request.layout, DiskLayout::InPlace) {
        return tools_available(&[
            "nixos-generate-config",
            "nix",
            "nix-env",
            "lsblk",
            "getent",
            "df",
            "mkpasswd",
        ]);
    }
    let mut tools = vec![
        "nixos-generate-config",
        "nixos-install",
        "nixos-enter",
        "lsblk",
    ];
    match &request.layout {
        DiskLayout::Erase { data, .. } => {
            tools.extend([
                "sfdisk",
                "udevadm",
                "mount",
                "umount",
                request.filesystem.formatter(),
            ]);
            if request.boot_mode == BootMode::Uefi {
                tools.push("mkfs.fat");
            }
            tools.extend(
                data.iter()
                    .map(|partition| partition.filesystem.formatter()),
            );
            if request.partition_swap() {
                tools.push("mkswap");
            }
        }
        DiskLayout::Alongside { esp, .. } => {
            tools.extend([
                "sfdisk",
                "udevadm",
                "mount",
                "umount",
                request.filesystem.formatter(),
            ]);
            if esp.is_none() {
                tools.push("mkfs.fat");
            }
        }
        DiskLayout::Partitions {
            format_root,
            format_esp,
            format_swap,
            ..
        } => {
            tools.extend(["mount", "umount"]);
            if *format_root {
                tools.push(request.filesystem.formatter());
            }
            if *format_esp {
                tools.push("mkfs.fat");
            }
            if *format_swap && request.partition_swap() {
                tools.push("mkswap");
            }
        }
        DiskLayout::Mounted { .. } => {}
        DiskLayout::InPlace => unreachable!("in-place preflight handled separately"),
    }
    if request.encrypt {
        tools.push("cryptsetup");
    }
    if request.file_swap() {
        tools.push("df");
        match request.filesystem {
            FileSystem::Btrfs => tools.push("btrfs"),
            _ => tools.extend(["fallocate", "mkswap"]),
        }
    }
    tools_available(&tools)
}

fn install(
    request: &InstallRequest,
    checked: &CheckedTarget,
    state_version: &str,
    session: &mut InstallMounts,
    configure: impl FnOnce(&Path, &InstallRequest) -> Result<(), String>,
) -> Result<(), String> {
    let target = checked.target.as_path();
    let boot_disk = checked.boot_disk.as_ref().map(stable_disk_path);
    let mut root = checked.root.clone();
    let mut esp = checked.esp.clone();
    let mut swap_device = None;
    let mut new_data = Vec::new();
    let (format_root, format_esp) = match &request.layout {
        DiskLayout::Erase { data, .. } => {
            let approved_disk = checked.boot_disk.as_ref().ok_or("Missing approved disk.")?;
            let partitions = partition::create(approved_disk, request, data)?;
            root = Some(partitions.root);
            esp = partitions.esp;
            swap_device = partitions
                .swap
                .as_ref()
                .map(|device| partition::prepare_swap(device, true))
                .transpose()?;
            new_data = partitions.data;
            (true, request.boot_mode == BootMode::Uefi)
        }
        DiskLayout::Alongside { .. } => {
            let approved_disk = checked.boot_disk.as_ref().ok_or("Missing approved disk.")?;
            let planned = checked
                .alongside
                .as_ref()
                .ok_or("Missing approved GPT geometry.")?;
            let (new_root, new_esp) = append_partitions(approved_disk, planned)?;
            root = Some(new_root);
            let format_esp = new_esp.is_some();
            if let Some(new_esp) = new_esp {
                esp = Some(new_esp);
            }
            (true, format_esp)
        }
        DiskLayout::Partitions {
            format_root,
            format_esp,
            format_swap,
            ..
        } => {
            swap_device = checked
                .swap
                .as_ref()
                .map(|device| partition::prepare_swap(device, *format_swap))
                .transpose()?;
            (*format_root, *format_esp)
        }
        DiskLayout::Mounted { .. } => (false, false),
        DiskLayout::InPlace => unreachable!("in-place apply handled separately"),
    };

    if !matches!(request.layout, DiskLayout::Mounted { .. }) {
        let root = root.as_ref().ok_or("Missing root partition.")?;
        ensure_same_idle(root)?;
        let root_path = if request.encrypt {
            let name = format!("nixos-install-{}", std::process::id());
            let mapper = Path::new("/dev/mapper").join(&name);
            if mapper.exists() {
                return Err("The installer encryption mapping already exists.".into());
            }
            // The reviewed plan already confirmed formatting this partition.
            install_command(
                "cryptsetup",
                "Create the LUKS2 container",
                [
                    "luksFormat",
                    "--batch-mode",
                    "--type",
                    "luks2",
                    "--verify-passphrase",
                    "--",
                    root.record.path.as_str(),
                ],
            )?;
            install_command(
                "cryptsetup",
                "Unlock the LUKS2 container",
                ["open", "--", root.record.path.as_str(), name.as_str()],
            )?;
            session.mapper = Some(name);
            mapper
        } else {
            PathBuf::from(&root.record.path)
        };
        if format_root {
            partition::format_filesystem(request.filesystem, &root_path)?;
        }
        require_unused_target(target, &mounts()?)?;
        create_directory(target)?;
        session.mount(&root_path, target, request.filesystem.name())?;
        require_fresh_target(target)?;

        if let Some(esp) = esp.as_ref() {
            ensure_same_idle(esp)?;
            if format_esp {
                command("mkfs.fat", ["-F", "32", esp.record.path.as_str()])?;
            }
            let boot_dir = target.join("boot");
            create_directory(&boot_dir)?;
            session.mount(Path::new(&esp.record.path), &boot_dir, "vfat")?;
        }
        for (device, partition) in new_data {
            ensure_same_idle(&device)?;
            let source = Path::new(&device.record.path);
            partition::format_filesystem(partition.filesystem, source)?;
            let destination = target.join(partition.mount_point().trim_start_matches('/'));
            if partition.role == DataRole::Data {
                create_directory(&target.join("mnt"))?;
            }
            create_directory(&destination)?;
            session.mount(source, &destination, partition.filesystem.name())?;
        }
    }

    let accepted_mounts = target_mounts(target, &mounts()?)?;
    if matches!(request.layout, DiskLayout::Mounted { .. }) && accepted_mounts != checked.mounts {
        return Err("The target mount changed after review.".into());
    }
    require_fresh_target(target)?;
    if shared_esp(request) {
        verify_shared_esp(target, &request.hostname)?;
    }
    if request.file_swap() {
        create_swap(target, request)?;
    }
    let config_dir = target.join("etc/nixos");
    create_directory(&target.join("etc"))?;
    create_directory(&config_dir)?;

    let hardware = capture(
        "nixos-generate-config",
        [
            OsStr::new("--root"),
            target.as_os_str(),
            OsStr::new("--show-hardware-config"),
        ],
    )?;
    write_new(&config_dir.join("hardware-configuration.nix"), &hardware)?;
    let configuration = configuration(
        request,
        boot_disk.as_deref(),
        swap_device.as_deref(),
        state_version,
    )?;
    write_new(
        &config_dir.join("configuration.nix"),
        configuration.as_bytes(),
    )?;
    configure(target, request)?;

    if target_mounts(target, &mounts()?)? != accepted_mounts {
        return Err("A target mount changed before NixOS installation.".into());
    }
    if !config_dir.join("flake.nix").is_file() {
        return Err(format!(
            "The installation configuration did not create {}/flake.nix.",
            config_dir.display()
        ));
    }

    let flake = format!("{}#{}", config_dir.display(), request.hostname);
    install_command(
        "nixos-install",
        "Build and install NixOS",
        [
            OsStr::new("--root"),
            target.as_os_str(),
            OsStr::new("--no-root-passwd"),
            OsStr::new("--no-channel-copy"),
            OsStr::new("--flake"),
            OsStr::new(&flake),
            OsStr::new("--impure"),
        ],
    )
    .map_err(|error| {
        format!("{error}\nThe target configuration remains at /etc/nixos for recovery.")
    })?;
    // nixos-enter takes a command string. Username validation limits the argument to
    // shell-safe account name characters. passwd reads the password from the TTY.
    let password_command = format!(
        "/nix/var/nix/profiles/system/sw/bin/passwd -- {}",
        request.username
    );
    command(
        "nixos-enter",
        [
            OsStr::new("--root"),
            target.as_os_str(),
            OsStr::new("-c"),
            OsStr::new(&password_command),
        ],
    )
    .map_err(|error| format!("{error}\nNixOS was installed, but the account password was not set."))
}

fn ensure_same_idle(approved: &DeviceIdentity) -> Result<(), String> {
    let devices = probe()?;
    let current = lookup(&devices, &approved.record.path)?;
    if identity(current)? != *approved {
        return Err(format!(
            "Block device {} changed after review.",
            approved.record.path
        ));
    }
    require_idle_tree(current)?;
    check_exclusive(current)
}

fn check_exclusive(device: &BlockDevice) -> Result<(), String> {
    // On Linux an exclusive block-device open refuses devices claimed by the
    // kernel, including by mounts in another process's mount namespace.
    const LINUX_O_EXCL: i32 = 0o200;
    OpenOptions::new()
        .read(true)
        .custom_flags(LINUX_O_EXCL)
        .open(&device.path)
        .map(|_| ())
        .map_err(|error| format!("Cannot exclusively access {}: {error}", device.path))
}

fn stable_disk_path(device: &DeviceIdentity) -> String {
    let Ok(entries) = fs::read_dir("/dev/disk/by-id") else {
        return device.record.path.clone();
    };
    let mut aliases: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !safe_absolute_path(&path) {
                return None;
            }
            let metadata = fs::metadata(&path).ok()?;
            (metadata.file_type().is_block_device() && metadata.rdev() == device.number)
                .then(|| path.to_string_lossy().into_owned())
        })
        .collect();
    aliases.sort();
    aliases
        .into_iter()
        .next()
        .unwrap_or_else(|| device.record.path.clone())
}

fn partition_disk(disk: &DeviceIdentity, layout: &str) -> Result<(), String> {
    let path = &disk.record.path;
    ensure_same_idle(disk)?;
    write_partition_table(
        path,
        &["--no-act", "--wipe=always", "--wipe-partitions=always"],
        layout,
    )?;
    ensure_same_idle(disk)?;
    write_partition_table(path, &["--wipe=always", "--wipe-partitions=always"], layout)?;
    command("udevadm", ["settle"])
}

fn write_partition_table(disk: &str, args: &[&str], layout: &str) -> Result<(), String> {
    let mut child = Command::new("sfdisk")
        .arg("--lock=yes")
        .args(args)
        .args(["--", disk])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Cannot start sfdisk: {error}"))?;
    let write_result = child
        .stdin
        .take()
        .ok_or("Cannot write the partition table to sfdisk.")?
        .write_all(layout.as_bytes());
    let status = child
        .wait()
        .map_err(|error| format!("Cannot wait for sfdisk: {error}"))?;
    write_result.map_err(|error| format!("Cannot send partition table to sfdisk: {error}"))?;
    if !status.success() {
        return Err(format!("sfdisk failed on {disk} ({status})."));
    }
    Ok(())
}

fn append_partitions(
    disk: &DeviceIdentity,
    plan: &AlongsidePlan,
) -> Result<(DeviceIdentity, Option<DeviceIdentity>), String> {
    let path = disk.record.path.as_str();
    let line = |range: Extent, guid: &str| {
        format!("start={}, size={}, type={guid}\n", range.start, range.size)
    };
    let mut layout = String::new();
    if let Some(esp) = plan.new_esp {
        layout.push_str(&line(esp, EFI_GUID));
    }
    layout.push_str(&line(plan.new_root, LINUX_GUID));
    let args = ["--append", "--wipe=never", "--wipe-partitions=never"];
    let mut dry_run = args.to_vec();
    dry_run.push("--no-act");
    ensure_same_idle(disk)?;
    write_partition_table(path, &dry_run, &layout)?;
    ensure_same_idle(disk)?;
    if read_gpt(path)? != plan.present {
        return Err("The GPT changed after review. Review a fresh plan.".into());
    }
    write_partition_table(path, &args, &layout)?;
    command("udevadm", ["settle"])?;

    let updated = read_gpt(path)?;
    let (root, esp) = verify_append(plan, &updated)?;
    let devices = probe()?;
    let current = identity(lookup(&devices, path)?)?;
    if !same_physical_disk(&current, disk) {
        return Err("The disk changed while adding the new partitions.".into());
    }
    let identify = |part| identify_gpt_partition(&devices, &current, updated.sector_size, part);
    Ok((identify(root)?, esp.map(identify).transpose()?))
}

fn same_physical_disk(current: &DeviceIdentity, approved: &DeviceIdentity) -> bool {
    current.number == approved.number
        && current.kernel_path == approved.kernel_path
        && current.record.size == approved.record.size
        && current.record.serial == approved.record.serial
        && current.record.wwn == approved.record.wwn
}

fn identify_gpt_partition(
    devices: &[BlockDevice],
    disk: &DeviceIdentity,
    sector_size: u64,
    partition: &GptPartition,
) -> Result<DeviceIdentity, String> {
    let record = lookup(devices, &partition.node)?;
    require_kind(record, "part")?;
    let identity = identity(record)?;
    let start = fs::read_to_string(identity.kernel_path.join("start"))
        .map_err(|error| format!("Cannot read kernel partition start: {error}"))?;
    if start.trim().parse::<u64>().ok().filter(|_| sector_size > 0)
        != partition
            .start
            .checked_mul(sector_size)
            .map(|offset| offset / 512)
        || partition.size.checked_mul(sector_size) != Some(record.size)
        || !matches_gpt_part(record, partition)
        || parent_disk(devices, record)?.number != disk.record.number
    {
        return Err(format!(
            "Kernel partition {} does not match the approved GPT entry.",
            partition.node
        ));
    }
    require_idle_tree(record)?;
    Ok(identity)
}

fn matches_gpt_part(device: &BlockDevice, partition: &GptPartition) -> bool {
    device.path == partition.node
        && device
            .partuuid
            .as_deref()
            .is_some_and(|uuid| uuid.eq_ignore_ascii_case(&partition.uuid))
}

#[derive(Default)]
struct InstallMounts {
    mounted: Vec<Mount>,
    mapper: Option<String>,
}

impl InstallMounts {
    fn mount(&mut self, source: &Path, target: &Path, filesystem: &str) -> Result<(), String> {
        command(
            "mount",
            [
                OsStr::new("-t"),
                OsStr::new(filesystem),
                OsStr::new("--"),
                source.as_os_str(),
                target.as_os_str(),
            ],
        )?;
        let mount = mounts()?
            .into_iter()
            .find(|mount| mount.destination == target)
            .ok_or_else(|| format!("Cannot verify the new mount at {}.", target.display()))?;
        self.mounted.push(mount);
        Ok(())
    }

    fn clean(&mut self) -> Result<(), String> {
        while let Some(expected) = self.mounted.pop() {
            let now: Vec<_> = mounts()?
                .into_iter()
                .filter(|mount| mount.destination == expected.destination)
                .collect();
            if now != [expected.clone()] {
                return Err(format!(
                    "Mount at {} changed before cleanup.",
                    expected.destination.display()
                ));
            }
            command(
                "umount",
                [OsStr::new("--"), expected.destination.as_os_str()],
            )?;
        }
        if let Some(mapper) = self.mapper.take() {
            command("cryptsetup", ["close", "--", mapper.as_str()])?;
        }
        Ok(())
    }
}

fn create_directory(path: &Path) -> Result<(), String> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(format!("{} is not a real directory.", path.display()))
            }
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn write_new(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn create_swap(target: &Path, request: &InstallRequest) -> Result<(), String> {
    let raw = capture(
        "df",
        [OsStr::new("-Pk"), OsStr::new("--"), target.as_os_str()],
    )?;
    let df = std::str::from_utf8(&raw).map_err(|_| "Invalid df output.")?;
    let available = df
        .lines()
        .last()
        .and_then(|line| line.split_whitespace().nth(3))
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or("Cannot determine available space for the target.")?;
    if available < (ROOT_MINIMUM + u64::from(request.swap_gib) * GIB) / 1024 {
        return Err(
            "The target does not have enough free space for the requested system and swap.".into(),
        );
    }
    let path = target.join("swapfile");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", path.display())),
        Ok(_) => return Err(format!("{} already exists.", path.display())),
    }
    let size = format!("{}G", request.swap_gib);
    match request.filesystem {
        FileSystem::Btrfs => command(
            "btrfs",
            [
                OsStr::new("filesystem"),
                OsStr::new("mkswapfile"),
                OsStr::new("--size"),
                OsStr::new(&size),
                path.as_os_str(),
            ],
        ),
        _ => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            command(
                "fallocate",
                [
                    OsStr::new("-l"),
                    OsStr::new(&size),
                    OsStr::new("--"),
                    path.as_os_str(),
                ],
            )?;
            command("mkswap", [OsStr::new("--"), path.as_os_str()])
        }
    }
}

fn shared_esp(request: &InstallRequest) -> bool {
    request.boot_mode == BootMode::Uefi
        && matches!(
            request.layout,
            DiskLayout::Alongside { esp: Some(_), .. }
                | DiskLayout::Partitions {
                    format_esp: false,
                    ..
                }
                | DiskLayout::Mounted { .. }
        )
}

fn efi_id(hostname: &str) -> String {
    format!("nixos-{}", hostname.to_ascii_lowercase())
}

fn verify_shared_esp(target: &Path, hostname: &str) -> Result<(), String> {
    let boot = target.join("boot");
    for path in [
        boot.join("grub"),
        boot.join("kernels"),
        boot.join("EFI").join(efi_id(hostname)),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(format!(
                    "{} already exists on the shared EFI partition. Use a dedicated EFI partition to preserve the existing bootloader.",
                    path.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Cannot inspect {}: {error}", path.display())),
        }
    }
    Ok(())
}

fn configuration(
    request: &InstallRequest,
    boot_disk: Option<&str>,
    swap_device: Option<&str>,
    state_version: &str,
) -> Result<String, String> {
    let boot = if matches!(request.layout, DiskLayout::InPlace) {
        String::new()
    } else {
        match request.boot_mode {
        BootMode::Uefi if shared_esp(request) => {
            let bootloader_id = efi_id(&request.hostname);
            format!("  boot.loader.systemd-boot.enable = lib.mkForce false;\n  boot.loader.grub.enable = lib.mkForce true;\n  boot.loader.grub.efiSupport = lib.mkForce true;\n  boot.loader.grub.efiInstallAsRemovable = lib.mkForce false;\n  boot.loader.grub.devices = lib.mkForce [];\n  boot.loader.grub.mirroredBoots = lib.mkForce [ {{ path = \"/boot\"; devices = [ \"nodev\" ]; efiBootloaderId = \"{bootloader_id}\"; }} ];\n  boot.loader.efi.canTouchEfiVariables = true;\n")
        }
        BootMode::Uefi => "  boot.loader.systemd-boot.enable = lib.mkForce true;\n  boot.loader.grub.enable = lib.mkForce false;\n  boot.loader.efi.canTouchEfiVariables = true;\n".into(),
        BootMode::Bios => {
            let disk = boot_disk.ok_or("The BIOS boot disk was not validated.")?;
            format!("  boot.loader.systemd-boot.enable = lib.mkForce false;\n  boot.loader.grub.enable = lib.mkForce true;\n  boot.loader.grub.efiSupport = lib.mkForce false;\n  boot.loader.grub.devices = lib.mkForce [ \"{disk}\" ];\n")
        }
    }
    };
    let desktop = desktop_lines(request.desktop.as_deref())?;
    let swap = partition::swap_configuration(request, swap_device)?;
    let groups = if request.network_manager {
        "\"wheel\" \"networkmanager\""
    } else {
        "\"wheel\""
    };
    Ok(format!(
        "{{ config, lib, pkgs, ... }}:\n{{\n  imports = [ ./hardware-configuration.nix ];\n\n{boot}\n  networking.hostName = \"{}\";\n  networking.networkmanager.enable = lib.mkForce {};\n  networking.useDHCP = lib.mkOverride 90 {};\n  time.timeZone = lib.mkOverride 90 \"{}\";\n  i18n.defaultLocale = lib.mkOverride 90 \"{}\";\n  console.keyMap = lib.mkOverride 90 \"{}\";\n\n{desktop}  users.mutableUsers = true;\n  users.users.\"{}\" = {{\n    isNormalUser = true;\n    extraGroups = [ {groups} ];\n  }};\n\n{swap}  nix.settings.experimental-features = [ \"nix-command\" \"flakes\" ];\n  environment.systemPackages = [ pkgs.nh pkgs.git ];\n  system.stateVersion = \"{state_version}\";\n}}\n",
        request.hostname,
        request.network_manager,
        !request.network_manager,
        request.timezone,
        request.locale,
        request.keymap,
        request.username
    ))
}

fn require_root() -> Result<(), String> {
    if capture("id", ["-u"])? == b"0\n" {
        Ok(())
    } else {
        Err("Run the installer as root (for example, sudo nixos-installer) to change disks or install.".into())
    }
}

fn require_terminal() -> Result<(), String> {
    if io::stdin().is_terminal() {
        Ok(())
    } else {
        Err("Run this action from an interactive terminal.".into())
    }
}

fn tools_available(tools: &[&str]) -> Result<(), String> {
    let path = std::env::var_os("PATH").ok_or("PATH is not set.")?;
    let dirs: Vec<_> = std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .collect();
    let unavailable: Vec<_> = tools
        .iter()
        .filter(|tool| {
            !dirs.iter().any(|dir| {
                fs::metadata(dir.join(tool)).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
        })
        .copied()
        .collect();
    if unavailable.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Required live-system tools are missing: {}.",
            unavailable.join(", ")
        ))
    }
}

fn capture<T: AsRef<OsStr>>(
    program: &str,
    args: impl IntoIterator<Item = T>,
) -> Result<Vec<u8>, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("Cannot run {program}: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn command<T: AsRef<OsStr>>(
    program: &str,
    args: impl IntoIterator<Item = T>,
) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    run_command(program, cmd)
}

fn run_command(program: &str, mut cmd: Command) -> Result<(), String> {
    let status = cmd
        .env_remove("NIXOS_BASE_CONFIG")
        .status()
        .map_err(|error| format!("Cannot run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed ({status})."))
    }
}

fn install_command<T: AsRef<OsStr>>(
    program: &str,
    step: &str,
    args: impl IntoIterator<Item = T>,
) -> Result<(), String> {
    let log_path = Path::new("/tmp").join(format!(
        "nixos-installer-{program}-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    ));
    let mut log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|error| format!("Cannot create {}: {error}", log_path.display()))?;
    writeln!(log, "{step} ({program})").map_err(|error| error.to_string())?;

    // Keep stdin and stdout on the terminal for passphrase prompts. Read stderr
    // in chunks so prompts can be displayed immediately even without a newline.
    let mut child = Command::new(program)
        .args(args)
        .env_remove("NIXOS_BASE_CONFIG")
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{step}: Cannot run {program}: {error}"))?;
    let mut stderr = child.stderr.take().expect("child stderr was piped");
    let mut terminal = io::stderr().lock();
    let mut recent = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut logging_error = None;
    let mut hash_error = None;
    let stream = loop {
        let read = match stderr.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(error),
        };
        let bytes = &chunk[..read];
        let _ = terminal.write_all(bytes);
        let _ = terminal.flush();
        if logging_error.is_none() {
            logging_error = log.write_all(bytes).err();
        }
        recent.extend_from_slice(bytes);
        if hash_error.is_none()
            && let Some(end) = recent.iter().rposition(|byte| *byte == b'\n')
        {
            hash_error = hash_mismatch(&String::from_utf8_lossy(&recent[..end]));
        }
        if recent.len() > 16 * 1024 {
            recent.drain(..recent.len() - 16 * 1024);
        }
    };
    drop(stderr);
    let status = child
        .wait()
        .map_err(|error| format!("{step}: Cannot wait for {program}: {error}"))?;
    if status.success() {
        let _ = fs::remove_file(&log_path);
        return Ok(());
    }
    let report = String::from_utf8_lossy(&recent);
    let details = hash_error
        .or_else(|| hash_mismatch(&report))
        .unwrap_or_else(|| {
            let lines: Vec<_> = report
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            lines[lines.len().saturating_sub(9)..].join("\n")
        });
    let log_note = match logging_error.or_else(|| stream.err()) {
        Some(error) => format!("Partial stderr log: {} ({error})", log_path.display()),
        None => format!("Stderr log: {}", log_path.display()),
    };
    Err(format!(
        "{step}: {program} failed ({status}).\n{details}\n{log_note}"
    ))
}

fn hash_mismatch(output: &str) -> Option<String> {
    let mut lines = output.lines();
    let error = lines.find(|line| line.contains("hash mismatch"))?;
    let hashes: Vec<_> = lines.take(3).collect();
    if !hashes.iter().any(|line| line.contains("got:")) {
        return None;
    }
    Some(
        std::iter::once(error)
            .chain(hashes)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn request(layout: DiskLayout) -> InstallRequest {
        InstallRequest {
            layout,
            boot_mode: BootMode::Uefi,
            filesystem: FileSystem::Ext4,
            encrypt: false,
            swap_gib: 0,
            hibernate: false,
            hostname: "workstation".into(),
            username: "owner".into(),
            timezone: "Europe/Moscow".into(),
            locale: "en_US.UTF-8".into(),
            keymap: "us".into(),
            desktop: None,
            network_manager: true,
        }
    }

    fn simulated_plan(layout: DiskLayout) -> InstallPlan {
        let request = request(layout);
        let checked = CheckedTarget {
            target: PathBuf::from(TARGET),
            boot_disk: None,
            root: None,
            esp: None,
            swap: None,
            mounts: Vec::new(),
            alongside: None,
            in_place: None,
            bios_entry: None,
        };
        InstallPlan {
            approved_request: request.clone(),
            target: checked.target.clone(),
            steps: describe_steps(&request, &checked),
            request,
            checked,
        }
    }

    #[test]
    fn review_names_exact_devices_to_be_formatted() {
        let plan = simulated_plan(DiskLayout::Partitions {
            root: "/dev/nvme0n1p2".into(),
            esp: Some("/dev/nvme0n1p1".into()),
            boot_disk: None,
            format_root: true,
            format_esp: false,
            swap: None,
            format_swap: false,
        });
        assert_eq!(confirmation_token(&plan), "ERASE /dev/nvme0n1p2");
        assert!(
            plan.steps
                .iter()
                .any(|step| step.contains("Keep EFI system partition /dev/nvme0n1p1"))
        );
    }

    #[test]
    fn partition_and_non_destructive_modes_have_distinct_confirmation() {
        let erase = simulated_plan(DiskLayout::Erase {
            disk: "/dev/sda".into(),
            data: Vec::new(),
        });
        let mounted = simulated_plan(DiskLayout::Mounted {
            target: "/mnt/nixos".into(),
        });
        assert_eq!(confirmation_token(&erase), "ERASE /dev/sda");
        assert_eq!(confirmation_token(&mounted), "INSTALL /mnt/nixos");
    }

    #[test]
    fn formatting_a_selected_swap_partition_requires_its_device_in_confirmation() {
        let mut plan = simulated_plan(DiskLayout::Partitions {
            root: "/dev/sda3".into(),
            esp: None,
            boot_disk: None,
            format_root: true,
            format_esp: false,
            swap: Some("/dev/sda2".into()),
            format_swap: true,
        });
        plan.request.swap_gib = 4;
        plan.approved_request = plan.request.clone();
        assert_eq!(confirmation_token(&plan), "ERASE /dev/sda3 /dev/sda2");
    }

    #[test]
    fn missing_mounted_target_explains_how_to_continue() {
        let error = target_mounts(Path::new("/mnt"), &[]).unwrap_err();
        assert!(error.starts_with("/mnt is not mounted."));
        assert!(error.contains("choose an installation layout"));
    }

    #[test]
    fn install_failure_reports_hash_mismatch_and_keeps_stderr_log() {
        let error = install_command("sh", "Build test system", [
            "-c",
            "printf 'error: hash mismatch in fixture\n  specified: sha256-A\n  got: sha256-B\n' >&2; exit 102",
        ]).unwrap_err();
        let note = error.lines().last().unwrap();
        let log_path = note.strip_prefix("Stderr log: ").unwrap();
        let log = fs::read_to_string(log_path).unwrap();
        fs::remove_file(log_path).unwrap();
        assert!(error.contains("exit status: 102"));
        assert!(error.contains("error: hash mismatch in fixture"));
        assert!(error.contains("  specified: sha256-A\n  got: sha256-B"));
        assert!(log.contains("Build test system"));
        assert!(log.contains("error: hash mismatch in fixture"));
    }

    fn gpt_fixture(sector_size: u64) -> Gpt {
        Gpt {
            label: "gpt".into(),
            id: "3E517A10-ED75-4424-9FFF-71DE67FB7D25".into(),
            device: "/dev/sda".into(),
            unit: "sectors".into(),
            first_sector: 34,
            last_sector: 64 * GIB / sector_size - 34,
            sector_size,
            partitions: vec![GptPartition {
                node: "/dev/sda1".into(),
                start: 1024 * 1024 / sector_size,
                size: 16 * GIB / sector_size,
                type_guid: EFI_GUID.into(),
                uuid: "OLD".into(),
                attributes: [("name".into(), "System".into())].into(),
            }],
        }
    }

    #[test]
    fn alongside_uses_aligned_gpt_space_without_resizing_existing_partitions() {
        for sector in [512, 4096] {
            let gpt = gpt_fixture(sector);
            let original = gpt.partitions[0].clone();
            let plan = plan_alongside(gpt.clone(), 64 * GIB, ROOT_MINIMUM, true).unwrap();
            let efi = plan.new_esp.unwrap();
            let root = plan.new_root;
            assert_eq!(efi.start, original.start + original.size);
            assert_eq!(efi.size * sector, GIB);
            assert_eq!(root.start, efi.start + efi.size);
            assert_eq!(root.start * sector % (1024 * 1024), 0);
            assert!(root.start + root.size <= gpt.last_sector + 1);

            let mut after = gpt.clone();
            let new = |node: &str, extent: Extent, guid: &str| GptPartition {
                node: node.into(),
                start: extent.start,
                size: extent.size,
                type_guid: guid.into(),
                uuid: node.into(),
                attributes: Default::default(),
            };
            after.partitions.push(new("/dev/sda2", efi, EFI_GUID));
            after.partitions.push(new("/dev/sda3", root, LINUX_GUID));
            assert_eq!(verify_append(&plan, &after).unwrap().0.node, "/dev/sda3");
            after.partitions[0].size -= 1;
            assert!(verify_append(&plan, &after).is_err());
        }
    }

    #[test]
    fn alongside_rejects_invalid_geometry_and_insufficient_free_space() {
        let gpt = gpt_fixture(512);
        assert!(plan_alongside(gpt.clone(), 64 * GIB, 48 * GIB, false).is_err());
        let mut overlapping = gpt.clone();
        overlapping.partitions.push(gpt.partitions[0].clone());
        assert!(plan_alongside(overlapping, 64 * GIB, ROOT_MINIMUM, false).is_err());
        let mut outside = gpt;
        outside.partitions[0].size = u64::MAX;
        assert!(plan_alongside(outside, 64 * GIB, ROOT_MINIMUM, false).is_err());
    }

    #[test]
    fn mount_inventory_keeps_mount_ids_sources_and_escapes() {
        let fixture = "20 7 8:2 / / rw,relatime - ext4 /dev/sda2 rw\n                       38 20 0:57 /@ /mnt rw,relatime - btrfs /dev/mapper/store rw\n                       39 38 8:4 / /mnt/efi\\040directory ro,nosuid - vfat /dev/sda4 ro\n";
        let records = parse_mounts(fixture).unwrap();
        assert_eq!(records[1].id, 38);
        assert_eq!(records[1].source, PathBuf::from("/dev/mapper/store"));
        assert_eq!(records[2].destination, PathBuf::from("/mnt/efi directory"));
        assert!(records[2].read_only);
        assert!(target_mounts(Path::new("/mnt"), &records).is_err());
        assert!(require_unused_target(Path::new("/mnt"), &records).is_err());
    }

    #[test]
    fn device_list_handles_null_mount_points_and_swap() {
        let fixture = r#"{"blockdevices":[
          {"name":"/dev/sda","size":20000000000,"type":"disk","mountpoints":[null],"maj:min":"8:0","ro":false,
            "children":[{"name":"/dev/sda1","size":10000000000,"type":"part","mountpoints":["[SWAP]",null],"maj:min":"8:1","ro":false}]}
        ]}"#;
        let devices: DeviceList = serde_json::from_str(fixture).unwrap();
        let disk = &devices.blockdevices[0];
        assert!(disk.mountpoints.is_empty());
        assert_eq!(disk.children[0].mountpoints, ["[SWAP]"]);
        assert_eq!(device_number(8 << 8 | 1), "8:1");
        assert_eq!(device_number(259 << 8 | 1 << 20), "259:256");
    }

    #[test]
    fn existing_configuration_and_symlinked_directory_are_preserved() {
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tag = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("installer-fixture-{}-{tag}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let etc = directory.join("etc");
        let external = directory.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("flake.nix"), "existing").unwrap();
        symlink(&external, &etc).unwrap();
        let link_rejected = require_fresh_target(&directory).is_err();
        fs::remove_file(&etc).unwrap();
        fs::create_dir(&etc).unwrap();
        fs::write(etc.join("configuration.nix"), "existing").unwrap();
        let config_rejected = require_fresh_target(&directory).is_err();
        let contents = fs::read_to_string(external.join("flake.nix")).unwrap();
        fs::remove_dir_all(&directory).unwrap();
        assert!(link_rejected);
        assert!(config_rejected);
        assert_eq!(contents, "existing");
    }

    #[test]
    fn configuration_encodes_storage_boot_and_network_choices() {
        let mut request = request(DiskLayout::Erase {
            disk: "/dev/sda".into(),
            data: Vec::new(),
        });
        request.swap_gib = 4;
        request.hibernate = true;
        request.desktop = Some("plasma".into());
        let swap = "/dev/disk/by-partuuid/aabbccdd-01";
        let efi = configuration(&request, None, Some(swap), "26.05").unwrap();
        assert!(efi.contains("boot.loader.systemd-boot.enable = lib.mkForce true"));
        assert!(efi.contains("services.desktopManager.plasma6.enable = true"));
        assert!(efi.contains("networking.useDHCP = lib.mkOverride 90 false"));
        assert!(efi.contains(&format!("device = \"{swap}\"")));
        assert!(efi.contains(&format!("boot.resumeDevice = \"{swap}\"")));
        request.boot_mode = BootMode::Bios;
        request.encrypt = true;
        request.hibernate = false;
        let bios =
            configuration(&request, Some("/dev/disk/by-id/ata-fixture"), None, "26.05").unwrap();
        assert!(bios.contains(
            "boot.loader.grub.devices = lib.mkForce [ \"/dev/disk/by-id/ata-fixture\" ]"
        ));
        assert!(bios.contains("system.stateVersion = \"26.05\""));
        assert!(bios.contains("device = \"/swapfile\""));
        assert!(!bios.contains("boot.resumeDevice"));
    }

    #[test]
    fn shared_efi_uses_distinct_loader_with_no_removable_fallback_overwrite() {
        let request = request(DiskLayout::Alongside {
            disk: "/dev/sda".into(),
            esp: Some("/dev/sda1".into()),
        });
        let output = configuration(&request, None, None, "26.05").unwrap();
        assert!(output.contains("efiBootloaderId = \"nixos-workstation\""));
        assert!(output.contains("efiInstallAsRemovable = lib.mkForce false"));
        assert!(output.contains("systemd-boot.enable = lib.mkForce false"));
    }

    #[test]
    fn user_and_setting_validation_rejects_shell_and_nix_syntax() {
        let mut request = request(DiskLayout::Erase {
            disk: "/dev/sda".into(),
            data: Vec::new(),
        });
        request.username = "owner; echo no".into();
        assert!(
            validate_settings(&request)
                .unwrap_err()
                .starts_with("Choose a normal")
        );
        request.username = "owner".into();
        request.timezone = "../etc".into();
        assert!(
            validate_settings(&request)
                .unwrap_err()
                .starts_with("Timezone")
        );
        request.timezone = "Etc/UTC".into();
        request.locale = "${untrusted}".into();
        assert!(
            validate_settings(&request)
                .unwrap_err()
                .starts_with("Timezone")
        );
    }
}
