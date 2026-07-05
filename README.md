# fast-delete

`fast-delete` moves top-level targets into the platform Trash, then purges the
staged Trash paths with a parallel fd-relative deleter.

Default behavior:

```sh
fast-delete path [path ...]
```

Modes:

```sh
fast-delete --trash-only path [path ...]  # stage into OS Trash, stop
fast-delete --direct path [path ...]      # skip Trash staging, delete directly
fast-delete --detach path [path ...]      # stage, spawn background purge worker
fast-delete --jobs 16 path [path ...]     # choose purge worker threads
fast-delete --cross-device path           # traverse nested mount/device boundaries
```

macOS staging uses Foundation's `NSFileManager` trash API and purges the returned
Trash destination.

Linux staging uses the FreeDesktop Trash layout:

```text
Trash/
  files/<name>
  info/<name>.trashinfo
```

If the process dies after staging, the item is still in a desktop-visible Trash
location whenever the platform Trash move completed.

Foreground purge modes show live progress while they scan and unlink. Detached
workers stay quiet and record status in their job manifest.
