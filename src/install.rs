//! NixOS live-system installation. Planning inspects disks and never modifies them.
use serde::Deserialize;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;
const ROOT_MINIMUM: u64 = 8 * GIB;
const ESP_SIZE_MIB: u32 = 1024;
const TARGET: &str = "/mnt";

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
    },
    Partitions {
        root: String,
        esp: Option<String>,
        boot_disk: Option<String>,
        format_root: bool,
        format_esp: bool,
    },
    Mounted {
        target: PathBuf,
    },
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
    fn name(self) -> &'static str {
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
    pub hostname: String,
    pub username: String,
    pub timezone: String,
    pub locale: String,
    pub keymap: String,
    pub desktop: Option<String>,
    pub network_manager: bool,
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
    mounts: Vec<Mount>,
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
    command("cfdisk", ["--lock=yes", "--", disk])?;
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
        DiskLayout::Erase { disk } => format!("ERASE {disk}"),
        DiskLayout::Partitions {
            root,
            esp,
            format_root,
            format_esp,
            ..
        } => {
            let mut formatted = Vec::new();
            if *format_root {
                formatted.push(root.as_str());
            }
            if *format_esp {
                if let Some(esp) = esp {
                    formatted.push(esp.as_str());
                }
            }
            if formatted.is_empty() {
                format!("INSTALL {root}")
            } else {
                format!("ERASE {}", formatted.join(" "))
            }
        }
        DiskLayout::Mounted { target } => format!("INSTALL {}", target.display()),
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
    let hostname_valid = !request.hostname.is_empty()
        && request.hostname.len() <= 63
        && request.hostname.as_bytes()[0].is_ascii_alphanumeric()
        && request
            .hostname
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && request
            .hostname
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-');
    if !hostname_valid {
        return Err(
            "Hostname must be a DNS label of 1–63 ASCII letters, digits or hyphens.".into(),
        );
    }
    let username_valid = !request.username.is_empty()
        && request.username.len() <= 32
        && (request.username.as_bytes()[0].is_ascii_lowercase()
            || request.username.starts_with('_'))
        && request
            .username
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        && !matches!(request.username.as_str(), "root" | "nobody" | "nixbld");
    if !username_valid {
        return Err("Choose a normal account name using lowercase ASCII letters, digits, underscores or hyphens.".into());
    }
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
    Ok(())
}

fn plain_setting(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.@+-".contains(&c))
}

fn safe_path_component(value: &str) -> bool {
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
        mounts: Vec::new(),
    };
    let required_root_size = ROOT_MINIMUM + u64::from(request.swap_gib) * GIB;

    match &request.layout {
        DiskLayout::Erase { disk } => {
            let device = lookup(devices, disk)?;
            require_kind(device, "disk")?;
            require_idle_tree(device)?;
            if device.size < required_root_size + 2 * GIB {
                return Err(format!(
                    "{disk} is too small for the requested system and swap."
                ));
            }
            require_unused_target(&checked.target, current_mounts)?;
            checked.boot_disk = Some(identity(device)?);
        }
        DiskLayout::Partitions {
            root,
            esp,
            boot_disk,
            format_root,
            format_esp,
        } => {
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
        value.eq_ignore_ascii_case("c12a7328-f81f-11d2-ba4b-00a0c93ec93b")
            || value.eq_ignore_ascii_case("0xef")
    })
}

fn require_bios_partition(disk: &BlockDevice) -> Result<(), String> {
    let valid = match disk.pttype.as_deref() {
        Some("dos") => true,
        Some("gpt") => disk.children.iter().any(|partition| {
            partition.parttype.as_deref().is_some_and(|value| {
                value.eq_ignore_ascii_case("21686148-6449-6e6f-744e-656564454649")
            })
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
        metadata.file_type().is_block_device().then(|| ())?;
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
        return Err(format!(
            "{} must be mounted exactly once.",
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
        DiskLayout::Erase { disk } => {
            steps.push(format!("Erase all filesystems and partitions on {disk}."));
            steps.push(match request.boot_mode {
                BootMode::Uefi => format!(
                    "Create a GPT, a {} MiB FAT EFI partition, and a root partition.",
                    ESP_SIZE_MIB
                ),
                BootMode::Bios => {
                    "Create a GPT, a 4 MiB BIOS boot partition, and a root partition.".into()
                }
            });
        }
        DiskLayout::Partitions {
            root,
            esp,
            format_root,
            format_esp,
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
    if request.swap_gib > 0 {
        steps.push(format!(
            "Create a {} GiB swap file on the target root.",
            request.swap_gib
        ));
    }
    steps.push(format!(
        "Install NixOS at {} using {} with {}.",
        checked.target.display(),
        request.filesystem.name(),
        match request.boot_mode {
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
    let state_version = state_version()?;
    let checked = inspect_target(request, &probe()?, &mounts()?)?;
    if checked != plan.checked {
        return Err(
            "Devices or mounts changed after review. Review a fresh installation plan.".into(),
        );
    }

    let mut session = InstallMounts::default();
    let result = install(request, &checked, &state_version, &mut session, configure);
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
    let mut tools = vec![
        "nixos-generate-config",
        "nixos-install",
        "nixos-enter",
        "nixos-version",
        "lsblk",
    ];
    match &request.layout {
        DiskLayout::Erase { .. } => {
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
        }
        DiskLayout::Partitions {
            format_root,
            format_esp,
            ..
        } => {
            tools.extend(["mount", "umount"]);
            if *format_root {
                tools.push(request.filesystem.formatter());
            }
            if *format_esp {
                tools.push("mkfs.fat");
            }
        }
        DiskLayout::Mounted { .. } => {}
    }
    if request.encrypt {
        tools.push("cryptsetup");
    }
    if request.swap_gib > 0 {
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
    let (format_root, format_esp) = match request.layout {
        DiskLayout::Erase { .. } => {
            let approved_disk = checked.boot_disk.as_ref().ok_or("Missing approved disk.")?;
            ensure_same_idle(approved_disk)?;
            partition_disk(&approved_disk.record.path, request.boot_mode)?;
            (root, esp) = wait_partitions(approved_disk, request.boot_mode)?;
            (true, request.boot_mode == BootMode::Uefi)
        }
        DiskLayout::Partitions {
            format_root,
            format_esp,
            ..
        } => (format_root, format_esp),
        DiskLayout::Mounted { .. } => (false, false),
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
            command(
                "cryptsetup",
                [
                    "luksFormat",
                    "--type",
                    "luks2",
                    "--verify-passphrase",
                    "--",
                    root.record.path.as_str(),
                ],
            )?;
            command(
                "cryptsetup",
                ["open", "--", root.record.path.as_str(), name.as_str()],
            )?;
            session.mapper = Some(name);
            mapper
        } else {
            PathBuf::from(&root.record.path)
        };
        if format_root {
            let force = match request.filesystem {
                FileSystem::Ext4 => "-F",
                _ => "-f",
            };
            command(
                request.filesystem.formatter(),
                [OsStr::new(force), root_path.as_os_str()],
            )?;
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
    }

    let accepted_mounts = target_mounts(target, &mounts()?)?;
    if matches!(request.layout, DiskLayout::Mounted { .. }) && accepted_mounts != checked.mounts {
        return Err("The target mount changed after review.".into());
    }
    require_fresh_target(target)?;
    if request.swap_gib > 0 {
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
    let configuration = configuration(request, boot_disk.as_deref(), state_version)?;
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
    command(
        "nixos-install",
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

fn partition_disk(disk: &str, boot: BootMode) -> Result<(), String> {
    let boot_partition = match boot {
        BootMode::Uefi => format!("size={}MiB, type=U", ESP_SIZE_MIB),
        BootMode::Bios => "size=4MiB, type=21686148-6449-6E6F-744E-656564454649".into(),
    };
    let layout = format!("label: gpt\nunit: sectors\n\n{boot_partition}\ntype=L\n");
    let mut child = Command::new("sfdisk")
        .args([
            "--lock=yes",
            "--wipe=always",
            "--wipe-partitions=always",
            "--",
            disk,
        ])
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
    command("udevadm", ["settle"])?;
    Ok(())
}

type PartitionDevices = (Option<DeviceIdentity>, Option<DeviceIdentity>);

fn wait_partitions(disk: &DeviceIdentity, boot: BootMode) -> Result<PartitionDevices, String> {
    for _ in 0..20 {
        let devices = probe()?;
        let current = lookup(&devices, &disk.record.path)?;
        let current_identity = identity(current)?;
        // The partition table and children were just changed. The physical device
        // and its immutable identifiers must still be the approved ones.
        if current_identity.number != disk.number
            || current_identity.kernel_path != disk.kernel_path
            || current.size != disk.record.size
            || current.serial != disk.record.serial
            || current.wwn != disk.record.wwn
        {
            return Err("The selected disk changed while partitioning.".into());
        }
        if current.pttype.as_deref() == Some("gpt") && current.children.len() == 2 {
            let mut root = None;
            let mut esp = None;
            for child in &current.children {
                require_kind(child, "part")?;
                require_idle_tree(child)?;
                let child_identity = identity(child)?;
                let number = fs::read_to_string(child_identity.kernel_path.join("partition"))
                    .map_err(|error| {
                        format!("Cannot verify new partition {}: {error}", child.path)
                    })?;
                match number.trim() {
                    "2" if child.size >= ROOT_MINIMUM => root = Some(child_identity),
                    "1" if boot == BootMode::Uefi && is_efi_partition(child) => {
                        esp = Some(child_identity)
                    }
                    _ => {}
                }
            }
            if root.is_some() && (boot == BootMode::Bios || esp.is_some()) {
                return Ok((root, esp));
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("The new partitions were not visible to the kernel.".into())
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

fn configuration(
    request: &InstallRequest,
    boot_disk: Option<&str>,
    state_version: &str,
) -> Result<String, String> {
    let boot = match request.boot_mode {
        BootMode::Uefi => "  boot.loader.systemd-boot.enable = lib.mkForce true;\n  boot.loader.grub.enable = lib.mkForce false;\n  boot.loader.efi.canTouchEfiVariables = true;\n".into(),
        BootMode::Bios => {
            let disk = boot_disk.ok_or("The BIOS boot disk was not validated.")?;
            format!("  boot.loader.systemd-boot.enable = lib.mkForce false;\n  boot.loader.grub.enable = lib.mkForce true;\n  boot.loader.grub.efiSupport = lib.mkForce false;\n  boot.loader.grub.devices = lib.mkForce [ \"{disk}\" ];\n")
        }
    };
    let desktop = desktop_lines(request.desktop.as_deref())?;
    let swap = if request.swap_gib > 0 {
        "  swapDevices = lib.mkForce [ { device = \"/swapfile\"; } ];\n"
    } else {
        "  swapDevices = lib.mkForce [];\n"
    };
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

fn state_version() -> Result<String, String> {
    let raw = capture("nixos-version", std::iter::empty::<&str>())?;
    let output = std::str::from_utf8(&raw).map_err(|_| "Invalid nixos-version output.")?;
    let mut parts = output.trim().split('.');
    let major = parts.next().ok_or("Missing live NixOS version.")?;
    let minor = parts.next().ok_or("Missing live NixOS minor version.")?;
    if [major, minor]
        .into_iter()
        .any(|part| part.len() != 2 || !part.bytes().all(|c| c.is_ascii_digit()))
    {
        return Err(format!(
            "Unrecognized live NixOS version: {}",
            output.trim()
        ));
    }
    Ok(format!("{major}.{minor}"))
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
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("Cannot run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed ({status})."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(layout: DiskLayout) -> InstallRequest {
        InstallRequest {
            layout,
            boot_mode: BootMode::Uefi,
            filesystem: FileSystem::Ext4,
            encrypt: false,
            swap_gib: 0,
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
            mounts: Vec::new(),
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
        });
        let mounted = simulated_plan(DiskLayout::Mounted {
            target: "/mnt/nixos".into(),
        });
        assert_eq!(confirmation_token(&erase), "ERASE /dev/sda");
        assert_eq!(confirmation_token(&mounted), "INSTALL /mnt/nixos");
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
        });
        request.swap_gib = 4;
        request.desktop = Some("plasma".into());
        let efi = configuration(&request, None, "26.05").unwrap();
        assert!(efi.contains("boot.loader.systemd-boot.enable = lib.mkForce true"));
        assert!(efi.contains("services.desktopManager.plasma6.enable = true"));
        assert!(efi.contains("networking.useDHCP = lib.mkOverride 90 false"));
        assert!(efi.contains("device = \"/swapfile\""));
        request.boot_mode = BootMode::Bios;
        let bios = configuration(&request, Some("/dev/disk/by-id/ata-fixture"), "26.05").unwrap();
        assert!(bios.contains(
            "boot.loader.grub.devices = lib.mkForce [ \"/dev/disk/by-id/ata-fixture\" ]"
        ));
        assert!(bios.contains("system.stateVersion = \"26.05\""));
    }

    #[test]
    fn user_and_setting_validation_rejects_shell_and_nix_syntax() {
        let mut request = request(DiskLayout::Erase {
            disk: "/dev/sda".into(),
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
