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
conchy-vpnx subscription show      # title, traffic, expiry, next update
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
  Two TUN clients break each other's routing (mihomo-based ones share routing table 2022 and
  the same `ip rule`s), so if another VPN already sends all traffic into its own TUN device
  (FlClash, sing-box, OpenVPN, a Tailscale exit node, ...), `auto` falls back to proxy mode with
  a warning and `tun` refuses to start.
- **proxy** — mihomo listens on `127.0.0.1:<mixed-port>`; the daemon sets `http_proxy`/`https_proxy`/
  `all_proxy` for newly started programs (D-Bus activation environment and the systemd user
  manager) and the GNOME proxy settings; your own GNOME proxy settings come back on `down`.
  Programs that are already running are not affected.
- **auto** (default) — TUN when mihomo has the capability, proxy otherwise.

## Subscriptions

The profile must be Clash/mihomo YAML; the daemon asks for it with a `clash.meta` User-Agent,
which most panels answer with YAML. Panels that bind subscriptions to devices (Remnawave and
compatible) also get `x-hwid`, `x-device-os`, `x-ver-os` and `x-device-model`; the HWID is a
hash of `/etc/machine-id`, so this machine counts as one device. If the server sends only
placeholder servers (`0.0.0.0:1`, e.g. "device limit reached"), the daemon reports their names
as the error instead of connecting to nothing.

The subscription is updated in the background as often as the server asks
(`profile-update-interval`, otherwise every 24 hours); a failed update keeps the previous
profile and is retried after 30 minutes. Title, traffic and expiry come from the `profile-title`
and `subscription-userinfo` headers.

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
// hwid "..."        // device id for the subscription server (default: derived from machine-id)
// subscription-update-hours 12   // default: what the server asks for, else 24; 0 = off
// overlay "https://..."          // remote overlays, see below; repeatable
// mihomo "/opt/mihomo/mihomo"   // default: looked up in PATH
```

### Merging rules (overlays)

The config is built in layers: the subscription, then remote overlays in the order they were
added, then your `~/.config/conchy/vpnx/overlay.yaml`. Each overlay applies to everything before
it, so your local file can also undo what a remote overlay did.

```yaml
# Changes to what earlier layers built (applied first):
delete-rules:                  # rule fields, or /regex/ over the whole rule
  - DOMAIN-SUFFIX,example.com  #   matches DOMAIN-SUFFIX,example.com,<any target>[,...]
  - /^GEOIP,/
replace-rule-targets:          # rules sent to one group now go to another
  "Old group": "New group"
delete-proxies: [some-node, /^Expired/]   # names or /regex/; removed from groups too
delete-proxy-groups: [Ads]                # rules that used it fall back to DIRECT
modify-proxy-groups:
  "Proxy":
    prepend-proxies: [my-node] # put first (moved if already there)
    append-proxies: []
    remove-proxies: [/RU/]     # names or /regex/
    interval: 300              # any other group key is overridden

# Additions:
prepend-rules:                 # before every earlier rule
  - DOMAIN-SUFFIX,work.corp,DIRECT
append-rules:                  # after them, before MATCH
  - DOMAIN-KEYWORD,ads,REJECT
prepend-proxies: []            # same name as an existing proxy replaces it
append-proxies: []
prepend-proxy-groups: []
append-proxy-groups: []
rule-providers: {}             # merged by key
# any other key is deep-merged (dns, sniffer, ...)
```

- The `MATCH` of the latest layer wins and is always last.
- Rules pointing at a group that no longer exists go to `DIRECT`, with a warning
  (`conchy-vpnx status`), instead of breaking the whole config. So do delete/replace entries that
  matched nothing, e.g. after the provider renamed a group.
- A group left without proxies gets `DIRECT`, so the config stays valid.
- Every new config is checked with `mihomo -t` first; if mihomo rejects it, the previous one keeps
  running.

### Remote overlays

Keep an overlay in a git repository (or a gist, any web server) and share it between machines:

```sh
conchy-vpnx overlay add https://raw.githubusercontent.com/me/vpn-rules/main/overlay.yaml
conchy-vpnx overlay list
conchy-vpnx overlay update         # also happens with every subscription update
conchy-vpnx overlay remove 1       # by number or URL
```

They are listed as `overlay "url"` in `vpnx.kdl`, downloaded before being added (a broken link is
refused) and cached, so the VPN starts offline; a failed update keeps the previous copy. A remote
overlay can change anything except the settings the daemon forces (controller, ports, TUN), so
only add sources you trust. URLs never appear in error messages, so private links with a token
in the query string stay private.

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
