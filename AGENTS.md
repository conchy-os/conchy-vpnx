# AGENTS.md — conchy-vpnx

Instructions for autonomous agents and contributors working in `conchy-vpnx`.

## 1. Conchy OS Project Rules

- **Rolling release**: Conchy OS is a rolling release with no version numbers. Do not introduce versioned release schemes, version bumps, or version-gated branches.
- **Rust-only for our own code**: All Conchy OS application, daemon, CLI, and library code must be written in Rust. Do not introduce GTK, Qt, Electron, Python, or new application logic in shell scripts. (Only minimal POSIX `/bin/sh` wrappers required for packaging/privilege escalation—such as `packaging/conchy-vpnx-tun` and the runit `run` script—exist outside Rust.)
- **One utility = one task**: Keep `conchy-vpnx` strictly focused on VPN/proxy management (`mihomo` supervision, profile/overlay merging, subscriptions, D-Bus interface, CLI, and typed D-Bus client). Do not add unrelated desktop or shell functionality here.
- **Target distributions**: **Void Linux (`glibc`, `runit`, `xbps`)** is the primary target; **Arch Linux** is secondary. Keep Void packaging and runit support first-class while maintaining Arch packaging parity.
- **Git workflow**:
  - Single long-lived branch: `main`.
  - Do feature work on short-lived PR branches and delete them after merge.
  - Never commit or push unless explicitly instructed.
- **Roadmap workflow**: Project-wide phase and task tracking lives in the meta-repository (`conchy-os/ROADMAP.md` and `conchy-os/roadmap.json`). Do not edit files outside this repository when assigned only to `conchy-vpnx`.
- **Mandatory Rust verification** after every code or workspace edit:
  ```sh
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```

## 2. Safety Rules (Host & Session Isolation)

- **Never touch the live user session**: Do not send commands to the user's compositor/desktop session (`hyprctl`, etc.) and do not interact with the host session D-Bus or systemd user manager.
- **Never run host-mutating `conchy-vpnx` commands**:
  - **DO NOT run `conchy-vpnx tun enable` (or `disable`)**: it invokes `pkexec` (`conchy-vpnx-tun` / `setcap`), modifies Linux capabilities on the system `mihomo` binary, and writes/removes `/etc/conchy-vpnx/tun-enabled`.
  - **DO NOT run `conchy-vpnx up` or `conchy-vpnx down` on the host machine**: it starts/stops `mihomo`, modifies host network routes/TUN interfaces (routing table 2022 / `ip rule`s), alters the live D-Bus activation environment (`UpdateActivationEnvironment`), updates the systemd user environment, and mutates GNOME proxy settings via `gsettings`.
  - **DO NOT start `conchy-vpnxd` on the host session bus**: it claims `org.conchy.Vpnx` on the live session bus, reads/writes user configuration, and when `autoconnect true` is set it immediately alters host network/proxy state.
- **Isolate XDG directories and D-Bus (`dconf`/`gsettings` trap)**:
  - Whenever running tests or manual checks that read/write config or data directories, isolate `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_RUNTIME_DIR` inside a temporary directory created with `mktemp -d` (and clean it up afterward).
  - **dconf/gsettings trap**: Redirecting `XDG_CONFIG_HOME` and `XDG_DATA_HOME` alone does **not** isolate `gsettings`/`dconf` if `DBUS_SESSION_BUS_ADDRESS` still points to the user's live session bus—`gsettings` communicates with `dconf-service` over D-Bus and will overwrite the live user's `org.gnome.system.proxy*` keys. For any integration test that exercises `proxy_env` or D-Bus, run inside an isolated session bus (`dbus-run-session`) with `GSETTINGS_BACKEND=memory` and isolated `XDG_*` directories.
- **Never use `pkill -f` matching your own command**: `pkill -f` inspects full command lines and can match and terminate the shell/agent command executing it.
- **Never load unverified `.so` files**: Do not `dlopen`, `LD_PRELOAD`, or link against unverified shared libraries.

## 3. Repository Overview & Crates

Workspace members in `Cargo.toml`:

| Crate | Path | Kind | Role |
|---|---|---|---|
| `vpnx-core` | `crates/core` | Library | KDL settings, XDG paths, subscription download & device headers, layered YAML/overlay merge, `mihomo` API client & process supervisor, `CAP_NET_ADMIN` checks, TUN route conflict detection |
| `conchy-vpnxd` | `crates/daemon` | Binary (`conchy-vpnxd`) | Supervises `mihomo`, exposes `org.conchy.Vpnx` at `/org/conchy/Vpnx` on the D-Bus session bus, applies/restores proxy environment |
| `conchy-vpnx` | `crates/cli` | Binary (`conchy-vpnx`) | CLI frontend communicating with `conchy-vpnxd` over D-Bus |
| `vpnx-client` | `crates/client` | Library | Typed `zbus` D-Bus proxy (`VpnxProxy`) and `tun_access_command` helper for frontends |

> **Cross-repo dependency (`conchy-shell`)**: `conchy-shell` depends on `crates/client` via path `../vpnx/crates/client`. Do not rename `vpnx-client`, move `crates/client`, or break its public Rust/D-Bus API (`VpnxProxy`, `BUS_NAME`, `OBJECT_PATH`, `TUN_HELPER`, `tun_access_command`, D-Bus property/method names, or JSON payload schemas) without coordinating updates in `conchy-shell`.

### Code Style & Comments

- **Language**: All code comments, docstrings, and commit/docs prose must be in **English**.
- Follow existing conventions: concise module `//!` docs, `///` item docstrings explaining behavior and edge cases, and explicit `// SAFETY:` comments justifying every `unsafe` block.
- Keep security invariants intact:
  - Never expose subscription or remote overlay URLs (which may contain tokens) in error messages or display metadata (`label`, `name`, `source`).
  - Force `allow-lan: false`, loopback-only `listeners`, loopback `dns.listen`, and UNIX-socket-only mihomo controller unless `allow-lan true` is explicitly configured in `vpnx.kdl`.

## 4. Build, Check, and Packaging

### Build & Verification Commands

```sh
# Format check (CI enforced)
cargo fmt --all -- --check

# Apply formatting
cargo fmt --all

# Lint all targets (CI enforced)
cargo clippy --workspace --all-targets -- -D warnings

# Run all workspace tests (CI enforced)
cargo test --workspace

# Build debug / release
cargo build --workspace
make build                  # runs: cargo build --release
```

### Runtime Dependencies

- `mihomo` in `PATH` (or configured via `mihomo "..."` in `vpnx.kdl`; not in official Void repos—see `packaging/void/mihomo/`)
- `dbus` (D-Bus session bus)
- `libcap-progs` on Void / `libcap` on Arch (provides `setcap` for TUN capabilities)
- Optional session tools used when available: `ip` (`iproute2`, for TUN conflict detection), `pkexec` (`polkit`, for `conchy-vpnx tun enable|disable`), `gsettings` (GNOME proxy settings in `proxy` mode)

### Packaging & Install Layout

- `Makefile`: Shared installation layout (`PREFIX ?= /usr/local`, `DESTDIR ?=`, `BINDIR ?= target/release`). Installs:
  - `conchy-vpnxd`, `conchy-vpnx`, `packaging/conchy-vpnx-tun` -> `$(PREFIX)/bin/`
  - `packaging/dbus/org.conchy.Vpnx.service` -> `$(PREFIX)/share/dbus-1/services/`
  - `packaging/systemd/conchy-vpnxd.service` -> `$(PREFIX)/lib/systemd/user/`
  - `packaging/runit/conchy-vpnxd/run` -> `$(PREFIX)/share/conchy-vpnx/runit/conchy-vpnxd/run`
- `packaging/void/`: Draft `xbps-src` templates for `conchy-vpnx` (`packaging/void/conchy-vpnx/template`) and `mihomo` (`packaging/void/mihomo/template` + `INSTALL` hook that re-applies `cap_net_admin,cap_net_bind_service+ep` if `/etc/conchy-vpnx/tun-enabled` exists).
- `packaging/arch/`: `PKGBUILD` (`conchy-vpnx-git`) and pacman `PostTransaction` hook (`packaging/arch/conchy-vpnx-mihomo-caps.hook`) invoking `/usr/bin/conchy-vpnx-tun reapply /usr/bin/mihomo`.
- `packaging/conchy-vpnx-tun`: Privileged helper invoked via `pkexec` to `enable`, `disable`, or `reapply` capabilities on a `mihomo*` binary and toggle `/etc/conchy-vpnx/tun-enabled`.
