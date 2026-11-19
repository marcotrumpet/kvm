#!/bin/sh
# Installs KVM for this user: ~/.local/bin/kvm, an app menu entry, and start at sign-in.
# Run from the unpacked release folder:  ./install.sh
set -eu
cd "$(dirname "$0")"
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications" "$HOME/.config/autostart"
pkill -x kvm 2>/dev/null || true
install -m 755 kvm "$HOME/.local/bin/kvm"
sed "s|^Exec=kvm|Exec=$HOME/.local/bin/kvm|" kvm.desktop > "$HOME/.local/share/applications/kvm.desktop"
cp "$HOME/.local/share/applications/kvm.desktop" "$HOME/.config/autostart/kvm.desktop"
echo "Installed. Starting KVM (it lives in the system tray; needs an X11 session)."
nohup "$HOME/.local/bin/kvm" >/dev/null 2>&1 &
