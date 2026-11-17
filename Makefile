# Install layout shared by the Arch and Void packages.
PREFIX ?= /usr/local
DESTDIR ?=
BINDIR ?= target/release

.PHONY: build install

build:
	cargo build --release

install:
	install -Dm755 $(BINDIR)/conchy-vpnxd $(DESTDIR)$(PREFIX)/bin/conchy-vpnxd
	install -Dm755 $(BINDIR)/conchy-vpnx $(DESTDIR)$(PREFIX)/bin/conchy-vpnx
	install -Dm755 packaging/conchy-vpnx-tun $(DESTDIR)$(PREFIX)/bin/conchy-vpnx-tun
	sed 's|/usr/bin/|$(PREFIX)/bin/|' packaging/dbus/org.conchy.Vpnx.service | \
		install -Dm644 /dev/stdin $(DESTDIR)$(PREFIX)/share/dbus-1/services/org.conchy.Vpnx.service
	sed 's|/usr/bin/|$(PREFIX)/bin/|' packaging/systemd/conchy-vpnxd.service | \
		install -Dm644 /dev/stdin $(DESTDIR)$(PREFIX)/lib/systemd/user/conchy-vpnxd.service
	install -Dm755 packaging/runit/conchy-vpnxd/run $(DESTDIR)$(PREFIX)/share/conchy-vpnx/runit/conchy-vpnxd/run
