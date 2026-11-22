# Executed by the systemd initrd before switching to the reviewed root.
set -eu
shopt -s dotglob nullglob

root=${1:-/sysroot}
old=$root/old-root
marker=$root/.nixos-installer-migrate
progress=$old/.nixos-installer-in-progress
completed=$root/.nixos-installer-migrated
marker_text='nixos-installer in-place v1'

regular_marker() {
    [[ -f $1 && ! -L $1 && $(<"$1") == "$marker_text" ]]
}

if [[ ! -e $marker && ! -L $marker ]]; then
    # An unarmed, unfinished entry must never start NixOS on the old tree.
    regular_marker "$completed"
    exit
fi
regular_marker "$marker"

if [[ $root == /sysroot ]]; then
    mount -o remount,rw "$root"
fi
if [[ ! -e $old && ! -L $old ]]; then
    [[ -d $root/etc && ! -L $root/etc && -f $root/etc/nixos/config.nix \
        && -f $root/etc/nixos-inplace/account.hash ]]
    mkdir -m 0700 -- "$old"
fi
[[ -d $old && ! -L $old && $(stat -c '%a' -- "$old") == 700 ]]
if [[ ! -e $progress && ! -L $progress ]]; then
    contents=("$old"/*)
    [[ ${#contents[@]} == 0 && -f $root/etc/nixos/config.nix \
        && -f $root/etc/nixos-inplace/account.hash ]]
    (umask 077; printf '%s\n' "$marker_text" > "$progress")
    sync -f "$root"
fi
regular_marker "$progress"

move_unique() {
    [[ ! -e $2 && ! -L $2 ]] && mv --no-copy -T -- "$1" "$2"
}

rollback() {
    local status=$?
    local path name failed=0
    trap - EXIT
    if (( status == 0 )); then return; fi
    set +e
    if [[ -d $old/etc && ! -L $old/etc && -d $root/etc && ! -L $root/etc ]]; then
        for name in nixos nixos-inplace; do
            if [[ -d $root/etc/$name && ! -e $old/etc/$name ]]; then
                move_unique "$root/etc/$name" "$old/etc/$name" || failed=1
            fi
        done
        if (( !failed )); then rmdir -- "$root/etc" || failed=1; fi
    fi
    for path in "$old"/*; do
        name=${path##*/}
        [[ $path == "$progress" ]] && continue
        move_unique "$path" "$root/$name" || failed=1
    done
    if (( !failed )); then
        rm -- "$progress"
        rmdir -- "$old"
    fi
    exit "$status"
}
trap rollback EXIT
trap 'exit 1' HUP INT TERM

if [[ ! -e $old/etc && ! -L $old/etc ]]; then
    [[ -d $root/etc && ! -L $root/etc ]]
    move_unique "$root/etc" "$old/etc"
fi
[[ -d $old/etc && ! -L $old/etc ]]

for path in "$root"/*; do
    name=${path##*/}
    case $name in
        etc|old-root|.nixos-installer-migrate|.nixos-installer-migrated|nix|home|boot|efi|dev|proc|sys|run|lost+found) continue ;;
    esac
    move_unique "$path" "$old/$name"
done
if [[ ! -e $root/etc && ! -L $root/etc ]]; then
    mkdir -m 0755 -- "$root/etc"
fi
[[ -d $root/etc && ! -L $root/etc ]]
for name in nixos nixos-inplace; do
    if [[ -d $old/etc/$name && ! -L $old/etc/$name ]]; then
        move_unique "$old/etc/$name" "$root/etc/$name"
    fi
    [[ -d $root/etc/$name && ! -L $root/etc/$name ]]
done
[[ -f $root/etc/nixos/config.nix && -f $root/etc/nixos-inplace/account.hash ]]
sync -f "$root"
move_unique "$marker" "$completed"
