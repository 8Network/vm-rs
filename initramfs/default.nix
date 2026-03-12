{ pkgs ? import <nixpkgs> {}
, kernelModulesDir ? ""
}:

let
  # Use pkgsStatic for fully static musl-linked binaries
  staticPkgs = pkgs.pkgsStatic;

  # Static busybox — all coreutils, networking, shell in one binary
  busybox = staticPkgs.busybox;

  # Static dropbear — lightweight SSH server (~110KB) + keygen
  dropbear = staticPkgs.dropbear;

  # The init script
  initScript = ./init;

in pkgs.stdenv.mkDerivation {
  name = "vm-initramfs";
  version = "0.1.0";

  dontUnpack = true;

  nativeBuildInputs = with pkgs; [ cpio gzip ];

  # Pass kernel modules dir as env var
  KERNEL_MODULES_DIR = kernelModulesDir;

  buildPhase = ''
    # Create the initramfs directory structure
    mkdir -p rootfs/{bin,sbin,etc/dropbear,dev,proc,sys,tmp,run}
    mkdir -p rootfs/mnt/{oci-layers,rootfs,overlay-work}
    mkdir -p rootfs/{home,root,lib}

    # Install busybox (static binary — provides sh, ip, mount, etc.)
    cp ${busybox}/bin/busybox rootfs/bin/busybox
    chmod 755 rootfs/bin/busybox

    # Install dropbear SSH server + key generator
    cp ${dropbear}/bin/dropbear    rootfs/bin/dropbear    2>/dev/null || \
    cp ${dropbear}/sbin/dropbear   rootfs/bin/dropbear
    cp ${dropbear}/bin/dropbearkey rootfs/sbin/dropbearkey 2>/dev/null || \
    cp ${dropbear}/sbin/dropbearkey rootfs/sbin/dropbearkey 2>/dev/null || \
    cp ${dropbear}/bin/dropbearkey rootfs/sbin/dropbearkey
    chmod 755 rootfs/bin/dropbear rootfs/sbin/dropbearkey

    # Install /init script (PID 1)
    cp ${initScript} rootfs/init
    chmod 755 rootfs/init

    # Install kernel modules (extracted from Alpine initramfs by build.sh)
    if [ -n "$KERNEL_MODULES_DIR" ] && [ -d "$KERNEL_MODULES_DIR/lib" ]; then
      echo "Including kernel modules from $KERNEL_MODULES_DIR"
      cp -r "$KERNEL_MODULES_DIR/lib" rootfs/
      find rootfs/lib -name '*.ko.gz' | while read f; do echo "  module: $f"; done
    fi

    # Build the cpio archive (newc format for kernel initramfs)
    (cd rootfs && find . | sort | cpio -o -H newc --quiet) | gzip -9 > initramfs.cpio.gz

    echo "=== Initramfs contents ==="
    (cd rootfs && find . -type f -exec ls -lh {} \;)
    echo "=== Initramfs size ==="
    ls -lh initramfs.cpio.gz
  '';

  installPhase = ''
    mkdir -p $out
    cp initramfs.cpio.gz $out/initramfs.cpio.gz
  '';

  meta = {
    description = "Custom VM initramfs — busybox + dropbear + init script";
    platforms = [ "x86_64-linux" "aarch64-linux" ];
  };
}
