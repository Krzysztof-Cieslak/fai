# Installing Fai

## Install the stable compiler

Linux and macOS:

```sh
curl -fsSL https://github.com/Krzysztof-Cieslak/fai/releases/latest/download/install.sh | sh
```

Windows PowerShell 5.1 or PowerShell 7:

```powershell
irm https://github.com/Krzysztof-Cieslak/fai/releases/latest/download/install.ps1 | iex
```

The bootstrap downloads a checksum-verified native launcher and compiler into
`~/.fai` (`%USERPROFILE%\.fai` on Windows). Installation is per-user and requires
neither administrator privileges nor Rust, Python, Node, or GitHub CLI. Unix
bootstrap requires `curl` or `wget`, `tar`, and `sha256sum` or `shasum`.

The launcher and compiler support Linux/glibc, macOS, and Windows/MSVC on x86-64
and ARM64. The initial build baselines are glibc 2.35, macOS 15, and Windows 11
or Windows Server 2022 (Windows 11 on ARM64). Each release's manifest records its
own compatibility requirement. Linux musl and 32-bit environments do not match
these artifacts.

Unix installation updates marked blocks in `.profile`, `.bashrc`, `.zshrc`, and
Fish's `conf.d/fai.fish`. Start a new shell or add `$FAI_HOME/bin` to the current
PATH. PowerShell installation updates the user environment and current PATH.

Download the script to supply options explicitly:

```sh
sh install.sh --version 0.2.0 --home "$HOME/tools/fai" --no-modify-path
```

```powershell
./install.ps1 -Version 0.2.0 -HomeDirectory "$HOME/tools/fai" -NoModifyPath
```

An exact prerelease is installed in the same way. Fresh installations select the
requested compiler; repeated bootstrap runs preserve an existing default.
Each versioned script defaults to its own release, so the `latest` one-liner
installs one coherent stable release even if a newer release appears mid-install.
For a first prerelease, use its `/releases/download/vVERSION/install.sh` or
`install.ps1` URL directly.

## Updates and side-by-side compilers

```sh
fai update                     # project dependencies
fai upgrade                    # stable compiler and global default
fai installer upgrade          # native launcher/version manager
fai installer --version

fai toolchain install 0.2.0
fai toolchain default 0.2.0
fai toolchain list
fai toolchain list --available
fai toolchain which
fai +0.2.0 check
fai toolchain pin 0.2.0
fai toolchain uninstall 0.1.0
```

`install` preserves the current default. `default` selects an installed exact
version. `upgrade` installs and selects the stable release without editing any
project. `pin` edits the active root's `[toolchain] version` in `fai.toml` while
preserving its other TOML content. `FAI_TOOLCHAIN` selects a version for a process
or shell; `+VERSION` takes precedence over it. An exact project pin is a hard
constraint, so a conflicting explicit selection fails. Without explicit selection
or a pin, the global default applies. Missing versions produce installation
instructions; ordinary compiler commands never download a compiler.

All management commands support `--message-format=json`. `which` reports the
selected executable and why it was selected. The launcher handles management
commands directly, including when the selected compiler is absent or broken.

`FAI_HOME` overrides the launcher/settings root. `FAI_TOOLCHAIN_DIR` overrides
the compiler payloads and inventory root. `fai toolchain register /path/to/fai`
registers an externally built compiler; uninstalling that registration preserves
its executable. The current default and in-use managed compilers cannot be
removed. Stop their daemons and language servers first. On Windows, an installer
upgrade can remain staged until running launchers exit.

After changing a project's pin, an older daemon can be stopped through that
version's absolute compiler path, as listed by `fai toolchain list`:
`/path/to/old/bin/fai daemon stop --project PROJECT`.

## Building native programs

The compiler embeds std, its runtime, and non-system native archives. AOT output
still requires platform development tools:

- Linux: a C toolchain and system development libraries, e.g. `build-essential`.
- macOS: Command Line Tools (`xcode-select --install`).
- Windows: Visual Studio Build Tools with C++ tools and Windows SDK; use a native
  developer environment so `link.exe` and SDK libraries can be found.

Git-backed dependencies require Git. Use `fai toolchain doctor` to inspect compiler
selection, PATH, Git, and linker prerequisites.

## Editors

Point the editor at `fai` on PATH and launch `fai lsp --project PROJECT`.
Workspace pins select the correct compiler independently in multi-root editors.
The VS Code extension checks selection after manifest changes and restarts the
affected language server when its compiler changes. Global-default changes apply
to new processes; restart an existing language server to adopt them.

## Release mirrors

`FAI_RELEASE_BASE_URL` overrides the releases root for controlled mirrors. It
must be HTTPS; loopback HTTP is supported for local qualification. Mirrors retain
`download/vVERSION/release.json`, all named assets, and
`latest/download/release.json`; available-version discovery uses `index.json`
with GitHub-shaped release records. Bootstrap scripts carry their own exact
installer release, while native commands resolve stable once and then use fixed
tag URLs.
