use super::*;

const ROOT: &str = "/";
const CONFIG: &str = "/etc/nixos";
const SECRETS: &str = "/etc/nixos-inplace";
const MARKER: &str = "/.nixos-installer-migrate";
const PROFILE: &str = "/nix/var/nix/profiles/system";
const BOOT: &str = "/boot/nixos-inplace";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Account {
    uid: u32,
    gid: u32,
    home: PathBuf,
    existing: bool,
    ssh_key: bool,
    host_keys: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BiosEntry {
    custom: PathBuf,
    uuid: String,
    menu: String,
}

fn parse_account(record: &str, username: &str) -> Result<Account, String> {
    let fields = record.trim_end().split(':').collect::<Vec<_>>();
    let [name, _, uid, gid, _, home, _] = fields.as_slice() else {
        return Err(format!("Cannot read the existing account {username}."));
    };
    if *name != username {
        return Err("Account name changed during inspection.".into());
    }
    let uid = uid.parse::<u32>().map_err(|_| "Invalid account UID.")?;
    let gid = gid.parse::<u32>().map_err(|_| "Invalid account GID.")?;
    let home = PathBuf::from(home);
    if !(1000..65534).contains(&uid)
        || (gid != 100 && !(1000..65534).contains(&gid))
        || !home.starts_with("/home")
        || home == Path::new("/home")
        || !home.components().all(|part| {
            matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        return Err("In-place installation requires an existing normal account with a home below /home and a compatible UID/GID.".into());
    }
    let ssh_key = fs::symlink_metadata(home.join(".ssh/authorized_keys")).is_ok_and(|file| {
        file.is_file() && file.uid() == uid && file.mode() & 0o022 == 0 && file.len() > 0
    });
    Ok(Account {
        uid,
        gid,
        home,
        existing: true,
        ssh_key,
        host_keys: Vec::new(),
    })
}

fn account(username: &str) -> Result<Account, String> {
    let output = Command::new("getent")
        .args(["passwd", username])
        .output()
        .map_err(|error| format!("Looking up the account: {error}"))?;
    if output.status.success() {
        return parse_account(
            std::str::from_utf8(&output.stdout).map_err(|_| "Invalid passwd record.")?,
            username,
        );
    }
    if output.status.code() != Some(2) {
        return Err("Could not inspect the account database.".into());
    }
    let home = Path::new("/home").join(username);
    absent(&home)?;
    let mut used = std::collections::BTreeSet::new();
    for database in ["passwd", "group"] {
        let inventory = capture("getent", [database])?;
        let text = std::str::from_utf8(&inventory).map_err(|_| "Invalid account inventory.")?;
        used.extend(
            text.lines()
                .filter_map(|entry| entry.split(':').nth(2)?.parse::<u32>().ok()),
        );
    }
    let id = (1000..60000)
        .find(|id| !used.contains(id))
        .ok_or("No free local account ID is available.")?;
    Ok(Account {
        uid: id,
        gid: id,
        home,
        existing: false,
        ssh_key: false,
        host_keys: Vec::new(),
    })
}

pub(super) fn suggested_account() -> Option<String> {
    ["SUDO_USER", "USER"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|username| {
            username.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                && username.len() <= 32
                && username
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
                && account(username).is_ok_and(|user| user.existing)
        })
}

fn password_hash<'a>(raw: &'a [u8], username: &str) -> Result<&'a [u8], String> {
    let name = username.as_bytes();
    let fields = raw.split(|byte| *byte == b':').collect::<Vec<_>>();
    match fields.as_slice() {
        [found, hash, ..] if *found == name && hash.starts_with(b"$")
            && hash.iter().all(|byte| byte.is_ascii_graphic()) => Ok(hash),
        _ => Err("The account has no usable local password. Set a local password on the current system before migrating.".into()),
    }
}

fn shadow(username: &str) -> Result<Vec<u8>, String> {
    // Do not include stderr or stdout in errors from a password database query.
    let output = Command::new("getent")
        .args(["shadow", username])
        .output()
        .map_err(|_| "Cannot query the current account password database.")?;
    if !output.status.success() {
        return Err("Cannot read the selected account's password as root.".into());
    }
    let mut hash = password_hash(&output.stdout, username)?.to_vec();
    hash.push(b'\n');
    Ok(hash)
}

fn prompt_password(username: &str) -> Result<Vec<u8>, String> {
    eprintln!("Set the local NixOS password for {username}.");
    let output = Command::new("mkpasswd")
        .arg("--method=yescrypt")
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|_| "Could not start the protected password prompt.")?;
    let hash = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    if !output.status.success()
        || !hash.starts_with(b"$y$")
        || !hash.iter().all(|byte| byte.is_ascii_graphic())
    {
        return Err("The protected password prompt did not return a password hash.".into());
    }
    Ok([hash, b"\n"].concat())
}

fn absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(format!(
            "{} already exists. Move it to a separate backup before an in-place installation.",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot inspect {}: {error}", path.display())),
    }
}

fn secure_boot_is_off() -> Result<(), String> {
    let efivars = Path::new("/sys/firmware/efi/efivars");
    let variable = efivars.join("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c");
    match fs::read(variable) {
        Ok(bytes) if bytes.get(4) == Some(&0) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound && efivars.is_dir() => Ok(()),
        Ok(_) => {
            Err("Disable Secure Boot before preparing the unsigned NixOS GRUB boot entry.".into())
        }
        Err(error) => Err(format!("Could not verify Secure Boot state: {error}")),
    }
}

fn esp_mount(mounts: &[Mount]) -> Result<&Mount, String> {
    let mut candidates = mounts.iter().filter(|mount| {
        ["/boot", "/boot/efi", "/efi"]
            .iter()
            .any(|path| mount.destination == Path::new(path))
            && mount.filesystem == "vfat"
            && !mount.read_only
    });
    let esp = candidates
        .next()
        .ok_or("Mount the writable FAT EFI partition at /boot, /boot/efi, or /efi first.")?;
    if candidates.next().is_some() {
        return Err("Multiple EFI mounts are present. Unmount the extra EFI partition before reviewing the plan.".into());
    }
    Ok(esp)
}

fn bios_entry() -> Result<BiosEntry, String> {
    let grub = ["/boot/grub", "/boot/grub2"]
        .into_iter()
        .find(|path| {
            let path = Path::new(path);
            let config = path.join("grub.cfg");
            fs::symlink_metadata(path)
                .is_ok_and(|dir| dir.is_dir() && dir.uid() == 0 && dir.mode() & 0o022 == 0)
                && fs::symlink_metadata(&config).is_ok_and(|file| {
                    file.is_file()
                        && file.uid() == 0
                        && file.mode() & 0o022 == 0
                        && (1..2 * 1024 * 1024).contains(&file.len())
                })
                && fs::read_to_string(config).is_ok_and(|text| {
                    text.lines().any(|line| {
                        line.trim_start().starts_with("source ") && line.contains("/custom.cfg")
                    })
                })
        })
        .ok_or(
            "BIOS in-place installation requires an existing GRUB menu that loads custom.cfg.",
        )?;
    let custom = Path::new(grub).join("custom.cfg");
    absent(&custom)?;
    let uuid = capture(
        "findmnt",
        ["--noheadings", "--output", "UUID", "--target", "/boot"],
    )?;
    let uuid = std::str::from_utf8(&uuid)
        .map_err(|_| "Invalid GRUB boot filesystem UUID.")?
        .trim();
    if uuid.is_empty()
        || !uuid
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err("Cannot identify the original GRUB boot filesystem UUID.".into());
    }
    let relative = capture("grub-mkrelpath", ["/boot"])
        .or_else(|_| capture("grub2-mkrelpath", ["/boot"]))
        .map_err(
            |_| "BIOS in-place installation needs the running distribution's grub-mkrelpath tool.",
        )?;
    let relative = std::str::from_utf8(&relative)
        .map_err(|_| "Invalid GRUB boot path.")?
        .trim();
    if !relative.starts_with('/')
        || !relative
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.@".contains(&byte))
    {
        return Err("The running GRUB boot directory uses an unsupported path.".into());
    }
    Ok(BiosEntry {
        custom,
        uuid: uuid.to_owned(),
        menu: format!(
            "{}/nixos-inplace/grub/grub.cfg",
            relative.trim_end_matches('/')
        ),
    })
}

fn host_key(type_name: &str) -> PathBuf {
    PathBuf::from(format!("/etc/ssh/ssh_host_{type_name}_key"))
}

fn secure_key(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|file| {
        file.is_file()
            && file.uid() == 0
            && file.mode() & 0o077 == 0
            && (1..=65_536).contains(&file.len())
    })
}

fn block_root<'a>(devices: &'a [BlockDevice], mounts: &[Mount]) -> Option<&'a BlockDevice> {
    let mount = mounts
        .iter()
        .find(|entry| entry.destination == Path::new(ROOT))?;
    if mount.read_only || !matches!(mount.filesystem.as_str(), "ext4" | "xfs" | "btrfs") {
        return None;
    }
    let device = mount_device(devices, mount)?;
    (device.kind == "part" && !device.read_only).then_some(device)
}

pub(super) fn is_installed_linux_root() -> bool {
    cfg!(target_os = "linux")
        && !Path::new("/etc/NIXOS").exists()
        && probe()
            .ok()
            .zip(mounts().ok())
            .is_some_and(|(devices, mounts)| block_root(&devices, &mounts).is_some())
}

pub(super) fn inspect(
    request: &InstallRequest,
    devices: &[BlockDevice],
    mounts: &[Mount],
    checked: &mut CheckedTarget,
) -> Result<(), String> {
    if request.encrypt || request.swap_gib != 0 {
        return Err(
            "The in-place layout reuses the filesystem. Clear encryption and new swap settings."
                .into(),
        );
    }
    for path in [
        "/etc/NIXOS",
        CONFIG,
        SECRETS,
        MARKER,
        "/.nixos-installer-migrated",
        PROFILE,
        BOOT,
        "/old-root",
    ] {
        absent(Path::new(path))?;
    }
    for path in ["/etc", "/boot", "/home", "/nix"] {
        let path = Path::new(path);
        let meta =
            fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} must be a real directory owned by root.",
                path.display()
            ));
        }
    }
    let root = mounts
        .iter()
        .find(|mount| mount.destination == Path::new(ROOT))
        .ok_or("Cannot identify the running system root.")?;
    if root.read_only || !matches!(root.filesystem.as_str(), "ext4" | "xfs" | "btrfs") {
        return Err(
            "In-place installation requires a writable ext4, XFS, or Btrfs root filesystem.".into(),
        );
    }
    let device =
        mount_device(devices, root).ok_or("The existing root is not a recognized block device.")?;
    require_kind(device, "part")?;
    if device.read_only {
        return Err("The existing root device is read-only.".into());
    }
    // Mounts under directories moved to old-root need a separate migration plan.
    for mount in mounts
        .iter()
        .filter(|mount| mount_device(devices, mount).is_some())
    {
        if !["/", "/boot", "/boot/efi", "/efi", "/home", "/nix"]
            .iter()
            .any(|allowed| mount.destination == Path::new(allowed))
            && !["/dev", "/proc", "/sys", "/run", "/snap"]
                .iter()
                .any(|root| mount.destination.starts_with(root))
        {
            return Err(format!(
                "The separate mount at {} requires a manual migration.",
                mount.destination.display()
            ));
        }
    }
    if request.boot_mode == BootMode::Uefi {
        secure_boot_is_off()?;
        let mount = esp_mount(mounts)?;
        let device =
            mount_device(devices, mount).ok_or("Cannot identify the existing EFI partition.")?;
        if !is_efi_partition(device) || device.read_only {
            return Err(
                "The mounted FAT filesystem is not a writable EFI system partition.".into(),
            );
        }
        absent(
            &mount
                .destination
                .join("EFI")
                .join(efi_id(&request.hostname)),
        )?;
        checked.esp = Some(identity(device)?);
    } else {
        if Path::new("/sys/firmware/efi").is_dir() {
            return Err(
                "The running system booted in UEFI mode. Use UEFI for the in-place boot entry."
                    .into(),
            );
        }
        let disk = parent_disk(devices, device)?;
        require_bios_partition(disk)?;
        checked.boot_disk = Some(identity(disk)?);
        checked.bios_entry = Some(bios_entry()?);
    }
    checked.target = PathBuf::from(ROOT);
    checked.root = Some(identity(device)?);
    checked.mounts = mounts
        .iter()
        .filter(|mount| {
            ["/", "/boot", "/boot/efi", "/efi", "/home", "/nix"]
                .iter()
                .any(|path| mount.destination == Path::new(path))
        })
        .cloned()
        .collect();
    let mut owner = account(&request.username)?;
    if owner.existing {
        let canonical = owner
            .home
            .canonicalize()
            .map_err(|error| format!("Opening existing home: {error}"))?;
        if canonical != owner.home || !canonical.is_dir() {
            return Err("The account's home must be a real directory within /home.".into());
        }
    }
    if owner.ssh_key
        && Path::new("/etc/ssh")
            .canonicalize()
            .is_ok_and(|dir| dir == Path::new("/etc/ssh"))
        && fs::metadata("/etc/ssh")
            .is_ok_and(|dir| dir.is_dir() && dir.uid() == 0 && dir.mode() & 0o022 == 0)
    {
        owner.host_keys = ["ed25519", "ecdsa", "rsa"]
            .into_iter()
            .filter(|kind| secure_key(&host_key(kind)))
            .collect();
    }
    checked.in_place = Some(owner);
    Ok(())
}

pub(super) fn describe(request: &InstallRequest, checked: &CheckedTarget) -> Vec<String> {
    let root = checked
        .root
        .as_ref()
        .map_or("/", |root| root.record.path.as_str());
    let mut steps = vec![
        format!("Build NixOS for the currently mounted root on {root}. No partitions or filesystems will be recreated."),
        "Keep /home, /boot and /nix.".into(),
        "On the first NixOS boot, move the previous system tree to the root-only /old-root directory.".into(),
    ];
    if let Some(account) = &checked.in_place {
        if account.existing {
            steps.push(format!("Retain {} and its usable local password. A protected prompt sets a password when the account was locked.", request.username));
        } else {
            steps.push(format!(
                "Create a new administrator {} and prompt for a local password.",
                request.username
            ));
        }
        if account.ssh_key {
            steps.push(
                "Keep the administrator's home authorized_keys available through OpenSSH.".into(),
            );
        }
    }
    if request.boot_mode == BootMode::Uefi {
        if let Ok(esp) = esp_mount(&checked.mounts) {
            steps.push(format!(
                "Install a separate GRUB EFI entry at {}.",
                esp.destination.display()
            ));
        }
    } else if let Some(entry) = &checked.bios_entry {
        steps.push(format!("Add 'NixOS in-place' to the original GRUB menu using {}. Choose it manually at boot; the BIOS boot sector is preserved.", entry.custom.display()));
    }
    steps.push("The reviewed NixOS network settings use DHCP. Static IPs, custom SSH ports and firewall rules require separate configuration. Keep a local console or recovery medium available.".into());
    steps.push("Stage a boot configuration. Return to the running system without activating NixOS or rebooting.".into());
    steps
}

fn migration_module(request: &InstallRequest, checked: &CheckedTarget) -> Result<String, String> {
    let account = checked
        .in_place
        .as_ref()
        .ok_or("Missing reviewed account.")?;
    let quote = crate::config::quote;
    let user = quote(&request.username);
    let home = quote(account.home.to_str().ok_or("Home path must be UTF-8.")?);
    let (device, efi): (String, String) = match request.boot_mode {
        BootMode::Uefi => {
            let esp = esp_mount(&checked.mounts)?
                .destination
                .to_str()
                .ok_or("EFI path must be UTF-8.")?;
            (
                "nodev".into(),
                format!(
                    "efiSysMountPoint = {}; efiBootloaderId = {};",
                    quote(esp),
                    quote(&efi_id(&request.hostname))
                ),
            )
        }
        BootMode::Bios => ("nodev".into(), String::new()),
    };
    let group = if account.gid == 100 {
        "\"users\"".into()
    } else {
        user.clone()
    };
    let group_declaration = if account.gid == 100 {
        String::new()
    } else {
        format!("users.groups.{group}.gid = {};", account.gid)
    };
    let efi_enabled = request.boot_mode == BootMode::Uefi;
    let ssh = if account.ssh_key {
        let known_keys = account
            .host_keys
            .iter()
            .map(|kind| {
                format!(r#"{{ type = "{kind}"; path = "{SECRETS}/ssh_host_{kind}_key"; }}"#)
            })
            .collect::<Vec<_>>()
            .join(" ");
        let keys = if known_keys.is_empty() {
            String::new()
        } else {
            format!("  services.openssh.hostKeys = lib.mkDefault [ {known_keys} ];\n")
        };
        format!(
            "services.openssh.enable = lib.mkDefault true;\n  services.openssh.settings = {{ PasswordAuthentication = lib.mkDefault false; PermitRootLogin = lib.mkDefault \"no\"; }};\n{keys}"
        )
    } else {
        String::new()
    };
    Ok(format!(
        r#"{{ lib, pkgs, ... }}: {{
  boot.loader.systemd-boot.enable = lib.mkForce false;
  boot.loader.grub.enable = lib.mkForce true;
  boot.loader.grub.efiSupport = lib.mkForce {efi_enabled};
  boot.loader.grub.efiInstallAsRemovable = lib.mkForce false;
  boot.loader.grub.devices = lib.mkForce [];
  boot.loader.grub.mirroredBoots = lib.mkForce [ {{ path = "{BOOT}"; devices = [ {} ]; {efi} }} ];
  boot.loader.efi.canTouchEfiVariables = {efi_enabled};
  boot.initrd.systemd.services.nixos-installer-migrate = {{
    description = "Preserve the previous system root before the first NixOS boot";
    after = [ "sysroot.mount" ];
    before = [ "initrd-root-fs.target" "initrd-nixos-activation.service" "initrd-switch-root.service" ];
    requiredBy = [ "initrd-nixos-activation.service" "initrd-switch-root.service" ];
    onFailure = [ "emergency.target" ];
    unitConfig.DefaultDependencies = false;
    serviceConfig.Type = "oneshot";
    path = [ pkgs.coreutils pkgs.util-linux ];
    script = builtins.readFile ./migrate-root.sh;
  }};
  {group_declaration}
  {ssh}
  users.users.{user} = {{
    uid = {};
    home = {home};
    group = {group};
    hashedPasswordFile = "{SECRETS}/account.hash";
  }};
}}"#,
        quote(&device),
        account.uid
    ))
}

fn ensure_free_space() -> Result<(), String> {
    let raw = capture("df", ["-Pk", "--", ROOT])?;
    let available = std::str::from_utf8(&raw)
        .ok()
        .and_then(|text| text.lines().last())
        .and_then(|line| line.split_whitespace().nth(3))
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or("Cannot determine free space on the running root.")?;
    if available < ROOT_MINIMUM / 1024 {
        return Err("The running root filesystem needs at least 8 GiB of free space.".into());
    }
    Ok(())
}

fn hardware_wrapper(reviewed: &CheckedTarget) -> String {
    let mounts = reviewed
        .mounts
        .iter()
        .map(|mount| crate::config::quote(&mount.destination.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"args@{{ config, lib, pkgs, modulesPath, ... }}:
let detected = import ./detected-hardware-configuration.nix args;
    preserve = [ {mounts} ];
in assert lib.all (mount: builtins.hasAttr mount detected.fileSystems) preserve;
detected // {{ fileSystems = lib.getAttrs preserve detected.fileSystems; }}
"#
    )
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn add_bios_menu(entry: &BiosEntry) -> Result<(), String> {
    if bios_entry()? != *entry {
        return Err("The original GRUB menu changed after review.".into());
    }
    let temporary = entry
        .custom
        .with_file_name(format!(".nixos-installer-custom-{}", std::process::id()));
    let menu = format!(
        "menuentry 'NixOS in-place' {{\n  search --no-floppy --fs-uuid --set=root {}\n  configfile {}\n}}\n",
        entry.uuid, entry.menu
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temporary)
        .map_err(|error| format!("Preparing GRUB menu: {error}"))?;
    let result = file
        .write_all(menu.as_bytes())
        .and_then(|_| file.sync_all())
        .and_then(|_| fs::hard_link(&temporary, &entry.custom))
        .map_err(|error| format!("Adding the original GRUB menu entry: {error}"));
    let _ = fs::remove_file(temporary);
    result
}

pub(super) fn apply(
    request: &InstallRequest,
    reviewed: &CheckedTarget,
    state_version: &str,
    configure: impl FnOnce(&Path, &InstallRequest) -> Result<(), String>,
) -> Result<(), String> {
    ensure_free_space()?;
    let account = reviewed
        .in_place
        .as_ref()
        .ok_or("Missing reviewed account.")?;
    let hash = if account.existing {
        shadow(&request.username).or_else(|_| prompt_password(&request.username))?
    } else {
        prompt_password(&request.username)?
    };
    let config_dir = Path::new(CONFIG);
    let secrets = Path::new(SECRETS);
    let marker = Path::new(MARKER);
    let hardware = capture("nixos-generate-config", ["--show-hardware-config"])?;
    let configuration = configuration(request, None, state_version)?.replace(
        "[ ./hardware-configuration.nix ]",
        "[ ./hardware-configuration.nix ./inplace.nix ]",
    );
    let migration = migration_module(request, reviewed)?;
    let mut owns_config = false;
    let mut owns_secrets = false;
    let mut owns_marker = false;
    let mut owns_profile = false;
    let mut boot_started = false;
    let result = (|| {
        fs::create_dir(config_dir).map_err(|error| format!("{CONFIG}: {error}"))?;
        owns_config = true;
        write_new(
            &config_dir.join("detected-hardware-configuration.nix"),
            &hardware,
        )?;
        write_new(
            &config_dir.join("hardware-configuration.nix"),
            hardware_wrapper(reviewed).as_bytes(),
        )?;
        write_new(
            &config_dir.join("configuration.nix"),
            configuration.as_bytes(),
        )?;
        write_new(&config_dir.join("inplace.nix"), migration.as_bytes())?;
        write_new(
            &config_dir.join("migrate-root.sh"),
            include_bytes!("../../templates/migrate-root.sh"),
        )?;
        configure(Path::new(ROOT), request)?;
        fs::create_dir(secrets).map_err(|error| format!("{SECRETS}: {error}"))?;
        owns_secrets = true;
        fs::set_permissions(secrets, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        private_file(&secrets.join("account.hash"), &hash)?;
        for kind in &account.host_keys {
            let source = host_key(kind);
            if !secure_key(&source) {
                return Err("An existing SSH host key changed after review.".into());
            }
            let bytes = fs::read(&source).map_err(|_| "Cannot read the verified SSH host key.")?;
            private_file(
                &secrets.join(source.file_name().ok_or("Missing SSH host key filename.")?),
                &bytes,
            )?;
        }

        let closure = build_output(request, "toplevel")?;
        let loader = build_output(request, "installBootLoader")?;
        if !closure.join("init").is_file() || !loader.is_file() {
            return Err("Nix did not return a bootable system closure and installer.".into());
        }
        verify_mounts(reviewed)?;
        absent(Path::new(PROFILE))?;
        command(
            "nix-env",
            [
                OsStr::new("--profile"),
                OsStr::new(PROFILE),
                OsStr::new("--set"),
                closure.as_os_str(),
            ],
        )?;
        owns_profile = true;
        // A failed boot installation must leave first-boot migration disarmed.
        let mut marker_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(marker)
            .map_err(|error| format!("{MARKER}: {error}"))?;
        owns_marker = true;
        marker_file
            .write_all(b"nixos-installer in-place v1\n")
            .and_then(|_| marker_file.sync_all())
            .map_err(|error| format!("{MARKER}: {error}"))?;
        boot_started = true;
        // switch-to-configuration refuses to run on a foreign distribution.
        // Call the same nixpkgs bootloader builder directly, without activation.
        let status = Command::new(loader)
            .arg(&closure)
            .env("NIXOS_INSTALL_BOOTLOADER", "1")
            .status()
            .map_err(|error| format!("Installing NixOS boot entry: {error}"))?;
        if !status.success() {
            return Err(format!(
                "NixOS boot entry failed: {status}. Use the original boot entry or recovery media."
            ));
        }
        if let Some(entry) = &reviewed.bios_entry {
            add_bios_menu(entry)?;
        }
        Ok(())
    })();
    if result.is_err() {
        if owns_marker {
            let _ = fs::remove_file(marker);
        }
        if !boot_started {
            if owns_profile
                && let Ok(generation) = fs::read_link(PROFILE)
                && generation.components().count() == 1
                && generation.to_str().is_some_and(|name| {
                    name.strip_prefix("system-")
                        .and_then(|name| name.strip_suffix("-link"))
                        .is_some_and(|index| {
                            !index.is_empty() && index.bytes().all(|digit| digit.is_ascii_digit())
                        })
                })
                && fs::remove_file(PROFILE).is_ok()
            {
                let _ = fs::remove_file(Path::new(PROFILE).with_file_name(generation));
            }
            if owns_config {
                let _ = fs::remove_dir_all(config_dir);
            }
            if owns_secrets {
                let _ = fs::remove_dir_all(secrets);
            }
        }
    }
    result
}

fn build_output(request: &InstallRequest, name: &str) -> Result<PathBuf, String> {
    let installable = format!(
        "path:{CONFIG}#nixosConfigurations.{}.config.system.build.{name}",
        request.hostname
    );
    let output = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            "--impure",
            "--no-link",
            "--print-out-paths",
            &installable,
        ])
        .env("NIXOS_BASE_CONFIG", Path::new(CONFIG).join("config.nix"))
        .output()
        .map_err(|error| format!("Building NixOS {name}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "NixOS {name} build failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|_| "Nix returned invalid output.")?
        .trim();
    if !Path::new(value).starts_with("/nix/store") || value.lines().count() != 1 {
        return Err(format!("Nix did not return one store path for {name}."));
    }
    Ok(PathBuf::from(value))
}

fn verify_mounts(reviewed: &CheckedTarget) -> Result<(), String> {
    let devices = probe()?;
    let current = mounts()?;
    for previous in &reviewed.mounts {
        if !current.iter().any(|mount| mount == previous) {
            return Err(format!(
                "{} changed after review.",
                previous.destination.display()
            ));
        }
    }
    for identity in [&reviewed.root, &reviewed.esp, &reviewed.boot_disk]
        .into_iter()
        .flatten()
    {
        let device = lookup(&devices, &identity.record.path)?;
        if super::identity(device)? != *identity {
            return Err(format!("{} changed after review.", device.path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "nixos-inplace-fixture-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn populate(root: &Path, bin: &Path, fail_name: &str) {
        fs::create_dir(root).unwrap();
        fs::create_dir(bin).unwrap();
        for dir in [
            "etc/nixos",
            "etc/nixos-inplace",
            "usr/bin",
            "home/user",
            "boot",
            "nix",
            "efi",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (file, text) in [
            (".nixos-installer-migrate", "nixos-installer in-place v1\n"),
            ("etc/nixos/config.nix", "config"),
            ("etc/nixos-inplace/account.hash", "hash fixture"),
            ("etc/os-release", "original"),
            ("usr/bin/tool", "tool"),
            ("home/user/data", "data"),
            ("boot/original", "boot"),
            (".old dotfile", "dotfile"),
        ] {
            fs::write(root.join(file), text).unwrap();
        }
        fs::write(bin.join("mv"), format!("#!/bin/bash\nset -eu\n[[ $1 = --no-copy && $2 = -T && $3 = -- ]]\n[[ ${{5##*/}} != {fail_name} ]] || exit 1\n[[ ! -e $5 && ! -L $5 ]]\nexec /bin/mv -- \"$4\" \"$5\"\n")).unwrap();
        fs::write(bin.join("sync"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(
            bin.join("stat"),
            if cfg!(target_os = "macos") {
                "#!/bin/sh\nexec /usr/bin/stat -f '%Lp' \"$4\"\n"
            } else {
                "#!/bin/sh\nexec /usr/bin/stat \"$@\"\n"
            },
        )
        .unwrap();
        for name in ["mv", "sync", "stat"] {
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn migrate(root: &Path, bin: &Path) -> bool {
        let path = std::env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        Command::new("/bin/bash")
            .arg("-c")
            .arg(include_str!("../../templates/migrate-root.sh"))
            .arg("migrate-root")
            .arg(root)
            .env("PATH", path)
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn stages_a_clean_root_and_preserves_original_system() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        let bin = fixture.0.join("bin");
        populate(&root, &bin, "never-fail");
        assert!(migrate(&root, &bin));
        assert!(migrate(&root, &bin));
        for file in [
            "old-root/etc/os-release",
            "old-root/usr/bin/tool",
            "old-root/.old dotfile",
            "home/user/data",
            "boot/original",
            "etc/nixos/config.nix",
            "etc/nixos-inplace/account.hash",
        ] {
            assert!(root.join(file).is_file(), "{file}");
        }
        assert_eq!(
            fs::metadata(root.join("old-root")).unwrap().mode() & 0o777,
            0o700
        );
        assert!(!root.join(".nixos-installer-migrate").exists());
        fs::remove_file(root.join("etc/nixos-inplace/account.hash")).unwrap();
        assert!(migrate(&root, &bin));
        fs::remove_dir_all(root.join("old-root")).unwrap();
        assert!(migrate(&root, &bin));
    }

    #[test]
    fn refuses_unarmed_boot_and_rolls_back_failed_move() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        let bin = fixture.0.join("bin");
        populate(&root, &bin, "usr");
        assert!(!migrate(&root, &bin));
        assert!(!root.join("old-root").exists());
        assert!(root.join("etc/os-release").is_file());
        assert!(root.join("usr/bin/tool").is_file());
        assert!(root.join("etc/nixos/config.nix").is_file());
        fs::remove_file(root.join(".nixos-installer-migrate")).unwrap();
        assert!(!migrate(&root, &bin));
        assert!(!root.join("old-root").exists());
    }

    #[test]
    fn resumes_after_an_interrupted_first_boot() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        let bin = fixture.0.join("bin");
        populate(&root, &bin, "never-fail");
        let old = root.join("old-root");
        fs::create_dir(&old).unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            old.join(".nixos-installer-in-progress"),
            "nixos-installer in-place v1\n",
        )
        .unwrap();
        fs::rename(root.join("etc"), old.join("etc")).unwrap();
        fs::rename(root.join("usr"), old.join("usr")).unwrap();
        fs::create_dir(root.join("etc")).unwrap();
        fs::rename(old.join("etc/nixos"), root.join("etc/nixos")).unwrap();
        assert!(migrate(&root, &bin));
        assert!(root.join("etc/nixos/config.nix").exists());
        assert!(root.join("etc/nixos-inplace/account.hash").exists());
        assert!(old.join("etc/os-release").exists());
        assert!(old.join("usr/bin/tool").exists());
    }

    #[test]
    fn default_inplace_detects_only_writable_partition_roots() {
        let devices: DeviceList = serde_json::from_value(serde_json::json!({
            "blockdevices": [{"name":"/dev/test1", "size":10000000000u64,
                "type":"part", "maj:min":"42:1", "fstype":"ext4", "ro":false}]
        }))
        .unwrap();
        let mut mount = Mount {
            id: 42,
            device_number: "42:1".into(),
            source: "/dev/test1".into(),
            destination: "/".into(),
            filesystem: "ext4".into(),
            read_only: false,
        };
        assert!(block_root(&devices.blockdevices, &[mount.clone()]).is_some());
        mount.read_only = true;
        assert!(block_root(&devices.blockdevices, &[mount.clone()]).is_none());
        mount.read_only = false;
        mount.filesystem = "overlay".into();
        assert!(block_root(&devices.blockdevices, &[mount]).is_none());
    }

    #[test]
    fn accepts_existing_account_identity_and_rejects_locked_credentials() {
        let user =
            parse_account("user:x:1001:1001:Example User:/home/user:/bin/bash", "user").unwrap();
        assert_eq!(
            (user.uid, user.gid, user.home),
            (1001, 1001, PathBuf::from("/home/user"))
        );
        assert!(parse_account("root:x:0:0:root:/root:/bin/bash", "root").is_err());
        assert_eq!(
            password_hash(b"user:$6$salt$hash:11000:0:99999:7:::\n", "user").unwrap(),
            b"$6$salt$hash"
        );
        assert!(password_hash(b"user:!$6$salt$hash:11000:0:99999:7:::\n", "user").is_err());
    }
}
