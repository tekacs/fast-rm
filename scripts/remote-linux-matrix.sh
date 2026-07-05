#!/usr/bin/env bash
set -Eeuo pipefail

tag="fast-delete-matrix-$(date +%s)-$$"
dataset="work/${tag}-zfs"
zvol_ext4="work/${tag}-ext4"
zvol_xfs="work/${tag}-xfs"
mnt_zfs="/mnt/${tag}-zfs"
mnt_ext4="/mnt/${tag}-ext4"
mnt_xfs="/mnt/${tag}-xfs"
uid_gid="$(id -u):$(id -g)"
bin="target/debug/fast-delete"

cleanup() {
    set +e
    sudo umount "$mnt_ext4" >/dev/null 2>&1
    sudo umount "$mnt_xfs" >/dev/null 2>&1
    sudo zfs destroy -f "$dataset" >/dev/null 2>&1
    sudo zfs destroy -f "$zvol_ext4" >/dev/null 2>&1
    sudo zfs destroy -f "$zvol_xfs" >/dev/null 2>&1
    sudo rm -rf "$mnt_zfs" "$mnt_ext4" "$mnt_xfs"
}
trap cleanup EXIT

need() {
    command -v "$1" >/dev/null || {
        echo "missing required command: $1" >&2
        exit 127
    }
}

need cargo
need mkfs.ext4
need mkfs.xfs

cargo build

echo "creating $dataset at $mnt_zfs"
sudo zfs create -o "mountpoint=$mnt_zfs" "$dataset"
sudo chown "$uid_gid" "$mnt_zfs"

echo "creating $zvol_ext4 at $mnt_ext4"
sudo zfs create -V 1G -o volmode=dev "$zvol_ext4"
sudo mkdir -p "$mnt_ext4"
sudo mkfs.ext4 -F -q "/dev/zvol/$zvol_ext4"
sudo mount -t ext4 "/dev/zvol/$zvol_ext4" "$mnt_ext4"
sudo chown "$uid_gid" "$mnt_ext4"

echo "creating $zvol_xfs at $mnt_xfs"
sudo zfs create -V 1G -o volmode=dev "$zvol_xfs"
sudo mkdir -p "$mnt_xfs"
sudo mkfs.xfs -f "/dev/zvol/$zvol_xfs" >/dev/null
sudo mount -t xfs "/dev/zvol/$zvol_xfs" "$mnt_xfs"
sudo chown "$uid_gid" "$mnt_xfs"

make_tree() {
    local target="$1"
    local outside="$2"

    mkdir -p "$target/a/b/c" "$target/wide"
    printf 'alpha\n' > "$target/a/file-a"
    printf 'beta\n' > "$target/a/b/file-b"
    printf 'gamma\n' > "$target/a/b/c/file-c"

    for i in $(seq 1 64); do
        printf '%s\n' "$i" > "$target/wide/file-$i"
    done

    mkdir -p "$outside"
    printf 'keep\n' > "$outside/kept"
    ln -s "$outside" "$target/link-to-outside"
}

assert_missing() {
    local path="$1"
    if [[ -e "$path" || -L "$path" ]]; then
        echo "expected missing: $path" >&2
        exit 1
    fi
}

assert_present() {
    local path="$1"
    if [[ ! -e "$path" && ! -L "$path" ]]; then
        echo "expected present: $path" >&2
        exit 1
    fi
}

info_for_staged() {
    local staged="$1"
    local trash_root
    trash_root="$(dirname "$(dirname "$staged")")"
    printf '%s/info/%s.trashinfo\n' "$trash_root" "$(basename "$staged")"
}

run_surface() {
    local label="$1"
    local root="$2"
    local direct_target="$root/direct-target"
    local default_target="$root/default-target"
    local trash_only_target="$root/trash-only-target"
    local outside="$root/outside"

    echo "== $label: direct purge =="
    make_tree "$direct_target" "$outside/direct"
    "$bin" --direct "$direct_target"
    assert_missing "$direct_target"
    assert_present "$outside/direct/kept"

    echo "== $label: trash then purge =="
    make_tree "$default_target" "$outside/default"
    "$bin" "$default_target"
    assert_missing "$default_target"
    assert_present "$outside/default/kept"

    echo "== $label: trash-only leaves desktop-visible trash item =="
    make_tree "$trash_only_target" "$outside/trash-only"
    local output staged info
    output="$("$bin" --trash-only "$trash_only_target")"
    echo "$output"
    staged="$(printf '%s\n' "$output" | awk '/^trashed / { print $NF }')"
    [[ -n "$staged" ]] || {
        echo "failed to parse staged path for $label" >&2
        exit 1
    }
    info="$(info_for_staged "$staged")"
    assert_missing "$trash_only_target"
    assert_present "$staged"
    assert_present "$info"
    grep -q '^\[Trash Info\]$' "$info"
    grep -q '^Path=' "$info"
    grep -q '^DeletionDate=' "$info"
    "$bin" --direct "$staged"
    rm -f "$info"
    assert_present "$outside/trash-only/kept"
}

run_surface zfs "$mnt_zfs"
run_surface ext4 "$mnt_ext4"
run_surface xfs "$mnt_xfs"

echo "matrix complete: $tag"
