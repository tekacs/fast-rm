# fast-rm

`fast-rm` is a Rust deleter for large directory trees.

By default it moves each top-level target into the platform Trash first, then
purges the staged Trash path with a parallel fd-relative walker. That gives the
interactive win of making the original path disappear immediately, while still
leaving a recoverable OS Trash item if the process dies after staging and before
purge completes.

## Install

From this checkout:

```sh
cargo install --locked --path .
```

## Usage

```sh
fast-rm path [path ...]
```

Default mode:

1. Move each top-level target into the OS Trash.
2. Print the staged Trash destination.
3. Purge those staged paths in parallel.
4. Remove Linux `.trashinfo` metadata for successfully purged staged items.

Useful modes:

```sh
fast-rm path [path ...]          # Trash-then-purge
fast-rm --direct path [path ...] # Delete directly, without Trash staging
fast-rm --trash-only path        # Move to Trash and stop
fast-rm --detach path            # Move to Trash, then purge in the background
fast-rm --jobs 24 path           # Choose purge worker threads
fast-rm --cross-device path      # Traverse nested device/mount boundaries
```

`--purge-only` is accepted as an alias for `--direct`.

## Modes

### Trash-then-purge

This is the default. It is meant for "make this huge tree disappear now" usage.

On a successful Trash move, the original path is gone immediately. The purge
phase then deletes the staged Trash path. If `fast-rm` exits or crashes after
the Trash move but before the purge completes, the remaining item is still in the
platform Trash location.

### Direct

```sh
fast-rm --direct target*
```

Direct mode skips Trash staging and purges the paths exactly where they are.
It is the fastest path when you do not want the recoverable staging step.

### Trash-only

```sh
fast-rm --trash-only path
```

Trash-only mode performs only the platform Trash move. It is useful when you want
the instant top-level disappearance and intend to let the desktop environment or
OS empty Trash operation handle deletion later.

### Detached

```sh
fast-rm --detach path
```

Detached mode stages into Trash, writes a job manifest, spawns a background
worker, and returns. The worker purges the staged paths and records status,
counts, and errors in the manifest.

Job manifests live under:

```text
macOS: $HOME/Library/Application Support/fast-rm/jobs/
Linux: $XDG_STATE_HOME/fast-rm/jobs/
Linux fallback: $HOME/.local/state/fast-rm/jobs/
```

Set `FAST_RM_STATE_DIR` to override that root.

Detached workers write logs next to the manifest using the same path with a
`.log` extension.

## Platform Trash Behavior

### macOS

macOS staging uses Foundation's `NSFileManager` Trash API:

```text
trashItemAtURL:resultingItemURL:error:
```

`fast-rm` purges the returned Trash destination. If Foundation does not
return a destination, the command fails rather than guessing where the item went.

### Linux

Linux staging uses the FreeDesktop Trash layout:

```text
Trash/
  files/<name>
  info/<name>.trashinfo
```

For paths on the same filesystem as `$XDG_DATA_HOME`, it uses:

```text
$XDG_DATA_HOME/Trash
```

with `$HOME/.local/share/Trash` as the normal fallback root.

For paths on another filesystem, it stages under that filesystem's Trash area:

```text
<mount>/.Trash/<uid>/
```

when a shared `.Trash` directory exists, otherwise:

```text
<mount>/.Trash-<uid>/
```

The target is renamed into `files/`, and its `.trashinfo` record is published in
`info/`. The move is checked to ensure staging stayed on the same device.

## Purge Semantics

The purge walker is Unix-only today.

It:

- Deletes files and symlinks as entries.
- Does not follow symlinks.
- Refuses an empty path and filesystem root.
- Walks directories bottom-up.
- Uses `openat`, `unlinkat`, `fdopendir`, and fd-relative traversal.
- Uses `dirent.d_type` when available, falling back to `fstatat(..., AT_SYMLINK_NOFOLLOW)` only when the directory entry type is unknown.
- Skips nested directories on another device by default.
- Crosses nested device boundaries only with `--cross-device`.

Errors are collected and printed after the purge report. A run exits non-zero if
any staging or purge errors were recorded.

## Progress

Foreground purge modes show a two-line progress display:

```text
\ deleting directly | roots 24 | scanned dirs 2841 | removed 50260 files + 2752 dirs | known 72% | skipped 0 | errors 0 | jobs 24
  [########################################----------------]
```

The bar is adaptive. `fast-rm` does not pre-scan the tree, because that would
double the traversal work. Instead, the known-work denominator grows as traversal
discovers roots, child directories, and file batches. The completed position only
moves forward as entries are actually removed or skipped.

Detached workers do not render progress. They report through the job manifest
and worker log.

## Development

```sh
just fmt
just clippy
just test
```

The Linux matrix smoke script exercises direct delete, Trash-then-purge, and
Trash-only behavior on disposable ZFS, ext4-on-zvol, and XFS test trees:

```sh
scripts/remote-linux-matrix.sh
```
