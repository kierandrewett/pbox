
printf '%s\n' '[pbox-image] Checking image compatibility (init, agent runtime and service startup)'
fail_image() {
    printf 'Image compatibility check failed: %s\n' "$1" >&2
    exit 1
}
for tool in systemctl sshd sudo python3 ip infocmp; do
    command -v "$tool" >/dev/null 2>&1 || fail_image "Missing $tool; install it in your Dockerfile."
done
infocmp xterm-256color >/dev/null 2>&1 || fail_image 'Missing xterm-256color terminfo; install your distribution terminal database.'
[ -x /bin/bash ] || fail_image 'Missing /bin/bash; install Bash for the pbox login shell.'
init_binary="$(readlink -f /sbin/init)"
"$init_binary" --version 2>/dev/null | head -n 1 | grep -q '^systemd ' || fail_image '/sbin/init must run systemd; choose a systemd-compatible base image.'
agent_error=$(/usr/local/bin/pbox-agent --help 2>&1 >/dev/null) || fail_image "pbox-agent cannot run in this image: ${agent_error:-the executable returned an error}. Check its CPU architecture, libc and shared libraries. Use a compatible image or configure agent.binary with a build for this image."
systemctl --root=/ preset pbox-agent.service >/dev/null || fail_image 'Cannot apply the pbox-agent service preset; check systemd masks and presets in your image.'
systemctl --root=/ is-enabled --quiet pbox-agent.service || fail_image 'Image policy disables pbox-agent.service. Remove its mask or add an earlier systemd preset that enables pbox-agent.service.'
printf '%s\n' '[pbox-image] Image checks passed; guest boot and relay connectivity will be checked after creation'
