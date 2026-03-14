#!/usr/bin/env bash
# Build the custom initramfs on NixOS.
# Uses Nix for static binaries (busybox, dropbear), assembles cpio directly.
# Run on the server: ./initramfs/build.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ALPINE_INITRAMFS="$HOME/.vm-rs/initramfs"
WORK_DIR=$(mktemp -d)
ROOTFS="$WORK_DIR/rootfs"

cleanup() { rm -rf "$WORK_DIR"; }
trap cleanup EXIT

# ── Get static binaries from Nix ────────────────────────────────────────────

echo "Getting static busybox from Nix..."
BUSYBOX=$(nix-build '<nixpkgs>' -A pkgsStatic.busybox --no-out-link 2>/dev/null)
echo "  busybox: $BUSYBOX/bin/busybox"

echo "Getting static dropbear from Nix..."
DROPBEAR=$(nix-build '<nixpkgs>' -A pkgsStatic.dropbear --no-out-link 2>/dev/null)
echo "  dropbear: $DROPBEAR"

# ── Create rootfs directory structure ───────────────────────────────────────

mkdir -p "$ROOTFS"/{bin,sbin,etc/dropbear,dev,proc,sys,tmp,run}
mkdir -p "$ROOTFS"/mnt/{oci-layers,rootfs,overlay-work}
mkdir -p "$ROOTFS"/{home,root,lib}

# ── Install binaries ────────────────────────────────────────────────────────

cp "$BUSYBOX/bin/busybox" "$ROOTFS/bin/busybox"
chmod 755 "$ROOTFS/bin/busybox"

# Dropbear puts binaries in bin/ or sbin/ depending on version
cp "$DROPBEAR/bin/dropbear" "$ROOTFS/bin/dropbear" 2>/dev/null || \
    cp "$DROPBEAR/sbin/dropbear" "$ROOTFS/bin/dropbear"
cp "$DROPBEAR/bin/dropbearkey" "$ROOTFS/sbin/dropbearkey" 2>/dev/null || \
    cp "$DROPBEAR/sbin/dropbearkey" "$ROOTFS/sbin/dropbearkey"
chmod 755 "$ROOTFS/bin/dropbear" "$ROOTFS/sbin/dropbearkey"

# ── Install init script ────────────────────────────────────────────────────

cp "$SCRIPT_DIR/init" "$ROOTFS/init"
chmod 755 "$ROOTFS/init"

# ── Extract kernel modules from Alpine initramfs ───────────────────────────

if [ -f "$ALPINE_INITRAMFS" ]; then
    echo "Extracting kernel modules from Alpine initramfs..."
    KVER=$(zcat "$ALPINE_INITRAMFS" | cpio -t 2>/dev/null | grep 'modules.dep$' | head -1 | sed 's|lib/modules/\([^/]*\)/.*|\1|')
    echo "  Kernel version: $KVER"

    cd "$WORK_DIR"
    zcat "$ALPINE_INITRAMFS" | cpio -id \
        "lib/modules/$KVER/kernel/drivers/net/virtio_net.ko.gz" \
        "lib/modules/$KVER/kernel/drivers/net/net_failover.ko.gz" \
        "lib/modules/$KVER/kernel/net/core/failover.ko.gz" \
        "lib/modules/$KVER/kernel/fs/fuse/virtiofs.ko.gz" \
        "lib/modules/$KVER/kernel/fs/fuse/fuse.ko.gz" \
        "lib/modules/$KVER/kernel/fs/overlayfs/overlay.ko.gz" \
        "lib/modules/$KVER/modules.dep" \
        "lib/modules/$KVER/modules.alias" \
        2>/dev/null || true

    if [ -d "$WORK_DIR/lib" ]; then
        cp -r "$WORK_DIR/lib" "$ROOTFS/"
        echo "  Modules included:"
        find "$ROOTFS/lib" -name '*.ko.gz' | while read f; do echo "    $(basename $f)"; done
    fi
    cd "$SCRIPT_DIR"
else
    echo "WARNING: No Alpine initramfs at $ALPINE_INITRAMFS — skipping kernel modules"
fi

# ── Build cpio.gz ───────────────────────────────────────────────────────────

echo ""
echo "=== Initramfs contents ==="
(cd "$ROOTFS" && find . -type f -exec ls -lh {} \;)

OUTPUT="$WORK_DIR/initramfs.cpio.gz"
(cd "$ROOTFS" && find . | sort | cpio -o -H newc --quiet) | gzip -9 > "$OUTPUT"

echo ""
echo "=== Result ==="
ls -lh "$OUTPUT"

# ── Install ─────────────────────────────────────────────────────────────────

mkdir -p "$HOME/.vm-rs"
cp "$OUTPUT" "$HOME/.vm-rs/initramfs-custom.cpio.gz"
echo "Installed to: $HOME/.vm-rs/initramfs-custom.cpio.gz"
