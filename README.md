![cargo-overstay — reclaim stale Rust build artifacts](assets/github-banner.webp)

# cargo-overstay

A small tool that keeps Rust `target/` directories from filling your disk. Run
it manually, or install its optional `cargo` shim for automatic cleanup.

## Install

```sh
cargo install cargo-overstay
```

## Use it manually

```sh
cargo overstay purge                              # reclaim tracked targets
cargo overstay purge --include-untracked          # also scan home and temp
cargo overstay purge --include-untracked ~/work   # scan a narrower location
cargo overstay ls                                 # show tracked projects
cargo overstay ls --include-untracked             # also list what is untracked
cargo overstay ls --include-untracked ~/work      # scan a narrower location
```

By default, both commands only consider targets recorded by the cargo shim.
Pass `--include-untracked` to also discover Cargo targets on disk — `ls`
reports exactly what `purge` would act on, so you can look before you delete.
Untracked sizes are listed separately and are not counted against the budget.

With no directory given, the scan covers your home directory **and the system
temp directories** (`/tmp` and `$TMPDIR`). Temp matters because agent tooling
puts git worktrees there, each with a full `target/` — builds that never feel
like they live on your disk, and often the largest thing a home-only scan
misses.

A scan stays on its root's filesystem. A mount underneath it — an OrbStack or
Docker VM export, a file server, an external disk — belongs to somewhere else,
where walking is slow enough to stall on a timeout and deleting a `target/`
would reclaim space on a machine you never named. Pointing the scan at such a
mount deliberately still works, since the root sets the boundary.

`purge` validates targets before deleting them, asks before removing ambiguous
matches, and skips active builds. It never follows symlinks and never descends
into a `target/`. A scanned `target/` is deleted outright only when both a
sibling `Cargo.toml` and cargo's own markers vouch for it. With just one of the
two — a manifest whose target was never built, or cargo markers with no
manifest beside them, as in a git worktree — it is listed as `(unverified)` and
gated behind a confirmation. A `target/` with neither signal (someone's JS
build output, say) is never listed and never touched.

`ls` shows `missing` for a recorded target that is no longer on disk; `purge`
prunes those rows.

Output is colored when connected to a terminal and stays plain when piped or
redirected. Set [`NO_COLOR`](https://no-color.org/) to disable colors.

## Enable automatic cleanup (optional)

Create a `cargo` symlink to overstay in a dedicated directory:

```sh
mkdir -p ~/.cargo-overstay/bin
ln -sf "$(command -v cargo-overstay)" ~/.cargo-overstay/bin/cargo
```

Put that directory first on `PATH` using your shell's startup file:

```sh
# zsh
echo 'export PATH="$HOME/.cargo-overstay/bin:$PATH"' >> ~/.zshenv

# bash (use the startup file appropriate for your environment)
echo 'export PATH="$HOME/.cargo-overstay/bin:$PATH"' >> ~/.bashrc

# fish
fish_add_path ~/.cargo-overstay/bin
```

Open a new shell. Overstay now forwards every `cargo` command to the real Cargo
and performs maintenance in the background. After Cargo exits, the worker
checks the tracked cache and immediately starts reclaiming if the build pushed
it over either configured size limit. Concurrent workers are coalesced. Keep
the shim as a symlink; wrapper scripts can recurse and copied binaries become
stale after upgrades.

Check that it is active:

```sh
command -v cargo
# ~/.cargo-overstay/bin/cargo
```

If that prints anything else, the shim is installed but shadowed, and nothing
you build is recorded. `ls` and `purge` warn when this is the case.

Check which build you are actually running:

```sh
cargo overstay --version
# cargo-overstay 0.3.0
```

Worth comparing against `cargo install` after an upgrade. The shim is a symlink
to the installed binary, so a stale install is a stale shim — and it fails
quietly rather than loudly.

### Watch out for version managers

A tool that manages your Rust toolchain — mise, asdf, rtx — prepends its own
cargo to `PATH` when it activates, which is usually *after* the line above ran.
mise in particular symlinks its rust install directly at `~/.cargo/bin`, so
activating it puts the real cargo in front of the shim. Both directories are on
`PATH`; only the order is wrong, which is why this fails silently.

Re-prepend the shim *after* the activation line rather than before it:

```sh
# ~/.zshrc, below `eval "$(mise activate zsh)"`
typeset -U path
path=("$HOME/.cargo-overstay/bin" $path)
```

```fish
# ~/.config/fish/config.fish, below `mise activate fish | source`
fish_add_path --path --move --prepend $HOME/.cargo-overstay/bin
```

Note that `~/.zshrc` only runs for *interactive* zsh shells, so a setup that
works in your terminal can still be bypassed by non-interactive shells (and
vice versa). `command -v cargo` inside the shell you actually build from is the
check that matters.

## Configure size limits

The size limits can be overridden with a TOML config file:

```toml
max_total_size = "150GiB"
max_target_size = "25GiB"
```

The default location is `~/Library/Application Support/cargo-overstay/config.toml`
on macOS. On Linux it is `$XDG_CONFIG_HOME/cargo-overstay/config.toml` when
that variable is set, otherwise `~/.config/cargo-overstay/config.toml`.
`CARGO_OVERSTAY_CONFIG` can point to a different file. Both settings are
optional and accept binary or decimal units such as `GiB`, `GB`, and `MiB`.

Invalid TOML, unknown settings, and invalid sizes are reported instead of
silently falling back to smaller limits; automatic cleanup remains disabled
until the config is fixed.

## Cleanup policy

Automatic cleanup uses these defaults:

- Unused for 30 days: remove the whole target.
- Larger than 10 GiB (`max_target_size`): trim recognized stale artifacts in place.
- More than 75 GiB (`max_total_size`) across tracked targets: remove
  least-recently-used targets.
- Less than 10 GiB free: remove least-recently-used targets until 20 GiB is free.

Overstay skips active or recently used targets and takes Cargo's build lock
before reclaiming anything. It never removes the project currently being built;
if that project's target exceeds 10 GiB, it only trims recognized artifacts.
These safety rules can leave a limit temporarily unsatisfied—for example, when
the current target alone is too large. Overstay records that condition and
retries after 15 minutes instead of waiting for the normal six-hour maintenance
interval.

## Uninstall

```sh
rm ~/.cargo-overstay/bin/cargo
# remove the PATH line from your shell config
cargo uninstall cargo-overstay
```

Optional state files live at `~/.local/share/cargo-overstay` on Linux
(`XDG_DATA_HOME` is honored) and `~/Library/Application Support/cargo-overstay`
on macOS.

## License

MIT
