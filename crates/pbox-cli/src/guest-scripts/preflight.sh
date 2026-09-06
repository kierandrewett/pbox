
printf '%s\n' '[pbox-image] Checking image compatibility (init, agent runtime and service startup)'
fail_image() {
    printf 'Image compatibility check failed: %s\n' "$1" >&2
    exit 1
}
for tool in sshd sudo python3 ip infocmp; do
    command -v "$tool" >/dev/null 2>&1 || fail_image "Missing $tool; install it in your Dockerfile."
done
infocmp xterm-256color >/dev/null 2>&1 || fail_image 'Missing xterm-256color terminfo; install your distribution terminal database.'
[ -x /bin/bash ] || fail_image 'Missing /bin/bash; install Bash for the pbox login shell.'
init_binary="$(readlink -f /sbin/init)"
systemd=false
if command -v systemctl >/dev/null 2>&1 && "$init_binary" --version 2>/dev/null | head -n 1 | grep -q '^systemd '; then
    systemd=true
elif command -v openrc-run >/dev/null 2>&1 && [ -d /etc/init.d ]; then
    : # OpenRC launch script is installed by the image preparation step.
elif command -v runit >/dev/null 2>&1 || command -v runsvdir >/dev/null 2>&1; then
    : # runit supervises /etc/service/pbox-agent/run.
else
    fail_image 'Unsupported init system; pbox needs systemd, OpenRC or runit to start its guest agent.'
fi
agent_error=$(/usr/local/bin/pbox-agent --help 2>&1 >/dev/null) || fail_image "pbox-agent cannot run in this image: ${agent_error:-the executable returned an error}. Check its CPU architecture, libc and shared libraries. Use a compatible image or configure agent.binary with a build for this image."
if [ "$systemd" = true ]; then
    systemctl --root=/ preset pbox-agent.service >/dev/null || fail_image 'Cannot apply the pbox-agent service preset; check systemd masks and presets in your image.'
    systemctl --root=/ is-enabled --quiet pbox-agent.service || fail_image 'Image policy disables pbox-agent.service. Remove its mask or add an earlier systemd preset that enables pbox-agent.service.'
elif [ -x /etc/init.d/pbox-agent ]; then
    [ -x /etc/init.d/pbox-agent ] || fail_image 'OpenRC pbox-agent service is not executable.'
elif [ -x /etc/service/pbox-agent/run ]; then
    [ -x /etc/service/pbox-agent/run ] || fail_image 'runit pbox-agent service is not executable.'
else
    fail_image 'The image init system is present but the pbox-agent launcher was not installed.'
fi
printf '%s\n' '[pbox-image] Image checks passed; guest boot and relay connectivity will be checked after creation'
