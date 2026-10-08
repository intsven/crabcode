# Local Agent Instructions

**Mirrored into the repo** at `crabcode\src\AGENTS.md`, which is a plain copy of
this file — it cannot be a symlink, since this token has no
`SeCreateSymbolicLinkPrivilege` and git would record the link target as file
content. Nothing enforces the sync, so after editing this file you must
re-copy it, or the two will silently diverge:

```powershell
Copy-Item 'D:\Programming\CrabCodeRoot\AGENTS.md' 'D:\Programming\CrabCodeRoot\crabcode\src\AGENTS.md' -Force
```

`t\publish_build.ps1` compares both hashes on every publish and warns when they
differ. Because the mirror sits at `src\`, it is the *nearer* `AGENTS.md` for
everything under `src/` and so takes precedence there; keeping the two
byte-identical is what makes that harmless.

Scope of the rules below: this workspace. They apply when the working directory
is `D:\Programming\CrabCodeRoot` or a subdirectory with no nearer `AGENTS.md`.

## Running crabcode: prefer `crabnew`, else `crabc`

`crabnew` runs the newest published build and is immune to the exe-locking rule
below. `crabc` is a copy of `target\release\crabcode.exe` and **goes stale after
every rebuild**. `crabcode` itself resolves to the separate npm/bun global
install — don't use it for local work.

### Prefer `crabnew` when a session is already running

Use `crabnew` when you are about to rebuild, or when a `crabc` session may be live:

```powershell
crabnew                      # run the newest published build
crabnew --build              # build + publish, then run
```

`crabnew` is `C:\Users\PC\.bun\bin\crabnew.cmd` (already on PATH). It runs a
timestamped snapshot from `D:\Programming\CrabCodeRoot\versions\`, never
`target\release\crabcode.exe`. That indirection is deliberate and is the whole
point — see the locking rule below.

## Building: Windows locks a running exe

**Do not assume a build can succeed while an instance is running.** Cargo
relinks `target\release\crabcode.exe` in place, and Windows refuses to replace
a running image:

```text
error: failed to remove file ...\target\release\crabcode.exe
  Caused by: Access is denied. (os error 5)
```

This is not a permissions problem to work around; it is expected. The same wall
defeats `install_crabc.ps1` (hence a stale `crabc`) and any self-overwriting copy.

### So: build through the publish scripts

Cargo has no post-build hook, so these wrappers provide one. Both call
`t\publish_build.ps1`, which snapshots the exe as
`versions\crabcode-<yyyyMMdd-HHmm>.exe` and rewrites `versions\current.txt`.
Snapshots are never locked, so publishing works even while sessions run.

The snapshot name is derived from the stamp `build.rs` actually **embedded**
(read back from `target\release\build-stamp.txt`), not from wall-clock time at
publish moment. That is deliberate: `CRABCODE_BUILD_DATE` is read when the build
starts, but publishing happens after it finishes, so naming from "now" drifts by
the build duration (~1–2 min) and `crabnew --version` could never be matched
against the snapshot `crabnew` points at. Note the format is minute-precision
(`-HHmm`, no seconds) because the embedded stamp is minute-precision.

**The two must always agree.** This is the cheapest way to confirm you are
running the build you think you are:

```powershell
$stamp = (Get-Content .\versions\current.txt).Trim()
$v = (crabnew --version) -replace '.*build ','' -replace '\)$',''
if ($v -match '^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})Z$') {
  $expect = '{0}{1}{2}-{3}{4}' -f $Matches[1],$Matches[2],$Matches[3],$Matches[4],$Matches[5]
  "current.txt=$stamp  version=$expect  match=$($expect -eq $stamp)"
}
```

If they disagree, a publish was bypassed — see the warnings below. Do **not**
"fix" this by hand-editing `current.txt` or renaming a snapshot; rebuild.

```powershell
# from this workspace root — builds, then publishes automatically
.\t\build-and-publish.ps1

# existing MSVC-aware wrapper; now publishes after a release build
.\build-release.ps1

# snapshot an already-built binary without rebuilding
powershell -NoProfile -ExecutionPolicy Bypass -File .\t\publish_build.ps1 -SkipBuild
```

- Keeps the newest 10 snapshots; a locked old one is skipped, never forced.
- A bare `cargo build --release` in some other terminal **will not** publish and may leave `target\release\crabcode.exe` with a stale embedded `CRABCODE_BUILD_STAMP` (the binary may show an old `--version` timestamp even after compile). Always use the publish wrappers (`.\t\build-and-publish.ps1`, `.\build-release.ps1`, or `crabnew --build`), which set `CRABCODE_BUILD_DATE` and call `t\publish_build.ps1`. Do **not** rely on `.\t\publish_build.ps1 -SkipBuild` after a partial/failed build — it will snapshot the stale `target\release\crabcode.exe` (as happened when `crabnew --version` showed `2026-10-06` despite `current.txt` pointing to a newer snapshot). Verify with: `(Get-FileHash ...).Hash -eq ...` or the stamp check above.
- `crabnew --build` calls `cargo` directly, so it needs `cl.exe` on PATH. On this
  machine prefer `.\build-release.ps1`, which imports the VS Build Tools env.
- The patch version alone (`0.0.14`) does **not** identify a build — many
  different binaries share it. Never conclude "this is stale" from an unchanged
  version number; compare stamps as shown above.
- A stale embedded stamp (e.g. `crabnew --version` shows `2026-10-06` while
  `current.txt` points at `20261007-...`) means the binary predates the change
  you are testing. The snapshot itself is immutable, so you cannot patch it —
  rebuild and republish:

```powershell
$env:CRABCODE_BUILD_DATE = ([DateTime]::UtcNow).ToString('yyyy-MM-ddTHH:mmZ')
cargo build --release --manifest-path .\crabcode\Cargo.toml
.\t\publish_build.ps1
```

Then confirm `crabnew --version` matches `versions\current.txt` before trusting
behavior changes. Remember any **running** session is still on the old binary:
`crabnew` only affects *new* invocations, so restart the session you are testing
in (ask the user first if it holds unsaved work).

### `crabc`: the pinned copy

```powershell
crabc            # run the copied build from anywhere on PATH
crabc --version  # crabcode 0.0.13
```

- Installed at `C:\Users\PC\.bun\bin\crabc.exe` (that dir is already on PATH).
- It is a **copy** of `D:\Programming\CrabCodeRoot\crabcode\target\release\crabcode.exe`,
  not a symlink: this token has no `SeCreateSymbolicLinkPrivilege`, and a hardlink
  would break whenever cargo replaces the exe. So it does **not** self-update.

### After every rebuild, refresh `crabc`

A `cargo build --release` leaves `crabc` stale. To refresh it:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File D:\Programming\CrabCodeRoot\t\install_crabc.ps1
```

This copy **fails if a `crabc` session is running** (same os error 5). Never run
it against live sessions without asking the user first; prefer `crabnew`, which
has no such constraint.

Verify the copy is current before trusting behavior changes:

```powershell
(Get-FileHash 'D:\Programming\CrabCodeRoot\crabcode\target\release\crabcode.exe').Hash -eq `
(Get-FileHash 'C:\Users\PC\.bun\bin\crabc.exe').Hash
```

A `false` means you are running the old binary. Do not report a build as
"working" until this is true — `cargo test` compiles only the test harness and
proves nothing about the shipped exe.

## Themes

- Global themes: `C:\Users\PC\.config\crabcode\themes\`
- A crabcode theme JSON must contain **both** `defs` and `theme` at top level;
  `src/theme.rs` rejects the file otherwise and `discover_themes` skips it
  silently. opencode exports that omit `defs` need `"defs": {}` added.
- Select with `/themes` → transparent also needs the ToggleTransparent switch;
  a theme cannot express it.
- For diff backgrounds, `"none"` means transparent (see `resolve_diff_background`
  in `src/theme.rs`). The literal string `"transparent"` is a *different* token
  and still falls back to an opaque color — that is deliberate, so existing
  themes such as `lucent-orng` keep their current appearance.

## Global instructions

- crabcode reads exactly **one** global `AGENTS.md`, from `%APPDATA%\crabcode\`
  (note: `dirs::config_dir()` is Roaming on Windows, *not* `~/.config`). It is a
  hardlink to `C:\Users\PC\.config\opencode\AGENTS.md`, so shared opencode rules
  apply here too. It silently detaches if a tool replaces the opencode file by
  rename (git checkout, some editors) — re-link if rules stop updating.
- Crabcode-specific rules belong in a **separate** file pulled in via the
  `instructions` array in `C:\Users\PC\.config\crabcode\crabcode.jsonc`, since a
  second global `AGENTS.md` would not stack. Current value:
  `AGENTS.crabcode.md` in that same directory.

## Testing on Windows

`cargo test --release` has 14 pre-existing failures in this environment (PTY
spawn, permission prompts, hyperlink wrapping, notifications). Diff them against
a clean `git stash` baseline before attributing any failure to your change.