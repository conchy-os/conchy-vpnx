# conchy-vpnx

Rust-клиент для VPN/прокси на ядре [mihomo](https://github.com/MetaCubeX/mihomo).
Он входит в Conchy OS, но от шелла не зависит.

> Статус: ранний каркас, функциональности пока нет.

## Компоненты

| Crate | Бинарник | Что делает |
|---|---|---|
| `vpnx-core` | — | мерж конфигов, подписка, бэкенды сервисов (systemd / runit / direct) |
| `conchy-vpnxd` | `conchy-vpnxd` | демон: управляет mihomo, публикует `org.conchy.Vpnx` в session bus |
| `conchy-vpnx` | `conchy-vpnx` | CLI |
| `vpnx-client` | — | типизированный D-Bus-клиент для фронтендов |

## Возможности (план)

- режим TUN, без нужных прав (`CAP_NET_ADMIN`) — автоматический откат на системный прокси
- одна активная подписка и пользовательский overlay с мержем правил (`prepend-rules`, `append-rules`, ...)
- работает с systemd, runit (Void Linux) и в режиме прямого запуска

## Зависимости

- `mihomo` в `PATH`
- D-Bus session bus

## Лицензия

GPL-3.0-or-later
