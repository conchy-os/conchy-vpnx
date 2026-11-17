# conchy-vpnx

VPN/proxy client built on the [mihomo](https://github.com/MetaCubeX/mihomo) core, written in Rust.
Part of Conchy OS, but it does not depend on the Conchy shell: the daemon and CLI work in any
Linux session with D-Bus (systemd or runit).

## Components

| Crate | Binary | Role |
|---|---|---|
| `vpnx-core` | — | config merging, subscription, mihomo API, process supervision |
| `conchy-vpnxd` | `conchy-vpnxd` | daemon: runs mihomo, exposes `org.conchy.Vpnx` on the session bus |
| `conchy-vpnx` | `conchy-vpnx` | CLI |
| `vpnx-client` | — | typed D-Bus client for frontends (conchy-shell uses it) |

## Usage

```sh
conchy-vpnx subscription set https://provider.example/sub   # download a Clash/mihomo profile
conchy-vpnx up
conchy-vpnx status
conchy-vpnx groups                 # proxy groups, `*` marks the selected proxy
conchy-vpnx select Proxy node-1
conchy-vpnx delay node-1
conchy-vpnx mode auto|tun|proxy
conchy-vpnx tun enable|disable     # allow TUN mode (asks for the admin password)
conchy-vpnx reload                 # after editing overlay.yaml
conchy-vpnx down
```

The daemon starts on first use through D-Bus activation, so nothing has to be enabled.

## Modes

- **tun** — all traffic goes through a virtual interface. mihomo needs `CAP_NET_ADMIN`, which
  is opt-in: `conchy-vpnx tun enable` (or the "Allow" button in conchy-shell) runs the
  `conchy-vpnx-tun` helper through pkexec. It sets the capability and leaves a marker in
  `/etc/conchy-vpnx/tun-enabled`, so the package hooks restore it after mihomo upgrades.
  Note that the capability applies to the mihomo binary, i.e. to every user of the machine.
  Don't run another mihomo-based client in TUN mode at the same time (e.g. FlClash): both use
  routing table 2022 and the same `ip rule`s.
- **proxy** — mihomo listens on `127.0.0.1:<mixed-port>`; the daemon sets `http_proxy`/`https_proxy`/
  `all_proxy` for newly started programs (D-Bus activation environment and the systemd user
  manager) and the GNOME proxy settings. Programs that are already running are not affected.
- **auto** (default) — TUN when mihomo has the capability, proxy otherwise.

## Files

```text
~/.config/conchy/vpnx.kdl               settings
~/.config/conchy/vpnx/subscription.url
~/.config/conchy/vpnx/overlay.yaml      your rules, merged on top of the subscription
~/.local/share/conchy/vpnx/             downloaded profile, mihomo home, generated config
$XDG_RUNTIME_DIR/conchy-vpnx/mihomo.sock  mihomo API (never exposed over TCP)
```

`vpnx.kdl`:

```kdl
mode "auto"          // auto | tun | proxy
mixed-port 7890
autoconnect false    // bring the VPN up when the daemon starts
// mihomo "/opt/mihomo/mihomo"   // default: looked up in PATH
```

### Merging rules (`overlay.yaml`)

```yaml
prepend-rules:                 # checked before the subscription's rules
  - DOMAIN-SUFFIX,work.corp,DIRECT
append-rules:                  # after the subscription's rules, before MATCH
  - DOMAIN-KEYWORD,ads,REJECT
prepend-proxies: []            # same name as a subscription proxy replaces it
append-proxies: []
prepend-proxy-groups: []
append-proxy-groups: []
rule-providers: {}             # merged by key
# any other key is deep-merged over the subscription (dns, sniffer, ...)
```

- A `MATCH` rule from the overlay replaces the subscription's; `MATCH` is always last.
- Rules pointing at a group that no longer exists go to `DIRECT`, with a warning
  (`conchy-vpnx status`), instead of breaking the whole config.
- Every new config is checked with `mihomo -t` first; if mihomo rejects it, the previous one keeps
  running.

## Installation

```sh
make && sudo make install PREFIX=/usr
```

Installs the binaries, the D-Bus activation file, a systemd user unit and a runit run script.
Packaging recipes:

- `packaging/arch/` — PKGBUILD (`conchy-vpnx-git`) and a pacman hook that restores the TUN
  capability after mihomo upgrades, if it was granted (pacman drops file capabilities when it
  replaces a file).
- `packaging/void/` — xbps-src templates for `conchy-vpnx` and `mihomo` (which Void does not ship).

## Requirements

- `mihomo` in `PATH`
- a D-Bus session bus

## License

GPL-3.0-or-later
