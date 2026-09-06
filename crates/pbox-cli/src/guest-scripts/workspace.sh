set -eu
fail() { printf 'Image compatibility check failed: %s\n' "$1" >&2; exit 1; }
/usr/local/bin/pbox-agent --help >/dev/null || fail 'The agent binary is incompatible with this image; configure agent.binary for its libc and architecture.'
# Prepare networking and the development account; the image supplies its shell/tools.
if ! command -v dhcpcd >/dev/null 2>&1 || ! command -v sudo >/dev/null 2>&1; then
    if command -v pacman >/dev/null 2>&1; then
        pacman -Syu --noconfirm --needed dhcpcd sudo
        pacman -Scc --noconfirm
    elif command -v apt-get >/dev/null 2>&1; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        apt-get install -y --no-install-recommends dhcpcd-base sudo
        rm -rf /var/lib/apt/lists/*
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y dhcpcd sudo
        dnf clean all
    elif command -v apk >/dev/null 2>&1; then
        apk add --no-cache dhcpcd sudo ca-certificates
    elif command -v zypper >/dev/null 2>&1; then
        zypper --non-interactive install --no-recommends dhcpcd sudo
        zypper clean --all
    else
        fail 'Install dhcpcd and sudo in your Dockerfile for workspace networking.'
    fi
fi
command -v dhcpcd >/dev/null 2>&1 || fail 'dhcpcd is unavailable after package preparation.'
if command -v systemd-sysusers >/dev/null 2>&1 && [ -f /usr/lib/sysusers.d/dhcpcd.conf ]; then
    systemd-sysusers /usr/lib/sysusers.d/dhcpcd.conf
fi
# Image layers must not carry another container's DHCP identity or leases.
rm -f /var/lib/dhcpcd/duid /var/lib/dhcpcd/secret /var/lib/dhcpcd/*.lease /var/lib/dhcpcd/*.lease6
mkdir -p /run /tmp
chmod 1777 /tmp

# A root-default image opens as a development account instead of root.
if ! id -u pbox >/dev/null 2>&1; then
    shell=/bin/sh
    [ ! -x /bin/bash ] || shell=/bin/bash
    if command -v useradd >/dev/null 2>&1; then
        useradd --create-home --shell "$shell" pbox
    elif command -v adduser >/dev/null 2>&1; then
        adduser -D -h /home/pbox -s "$shell" pbox
    else
        fail 'Install useradd or adduser so pbox can create its development account.'
    fi
fi
mkdir -p /home/pbox /etc/sudoers.d
chown "pbox:$(id -gn pbox)" /home/pbox
printf '%s\n' 'pbox ALL=(ALL:ALL) NOPASSWD: ALL' > /etc/sudoers.d/99-pbox
chmod 0440 /etc/sudoers.d/99-pbox
visudo -cf /etc/sudoers.d/99-pbox
printf '%s\n' pbox > /etc/pbox/default-user
printf '%s\n' unmanaged > /etc/pbox-image-ostype
printf '%s\n' '[pbox-image] Workspace ready: networking and passwordless development account; no init system required'
