{
  # BeetOS reproducible dev shell.
  #
  # Motivation: the QEMU test harness (smokes, bench, screenshots) depends
  # on an external toolchain — qemu-system-aarch64 *and its option ROMs*
  # (efi-virtio.rom &c.), socat, ImageMagick, the LLVM objcopy/strip
  # tools, and a Rust toolchain with the aarch64-unknown-none target.
  # Pinning `qemu` here is the key win: in nixpkgs the qemu package bundles
  # its pc-bios blobs, so `nix develop` can never land in the state we hit
  # once (a recycled container with qemu gone, then a minimal reinstall
  # missing efi-virtio.rom → every smoke booting to zero markers).
  #
  # Scope: a dev *shell* only. The project builds with plain `cargo` /
  # `cargo xtask` exactly as before — this just guarantees the tools those
  # commands shell out to are present and pinned. The M9 std work uses a
  # separate custom Rust fork (beetos/rust), built outside; it is NOT
  # managed here.
  #
  # Usage:
  #   nix develop            # drops you in the shell with the toolchain
  #   cargo xtask qemu-smoke # …then run anything as usual
  # Or with direnv: `direnv allow` (see .envrc).

  description = "BeetOS — secure minimal AArch64 microkernel: dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # Single source of truth for the Rust version: read the same
        # rust-toolchain.toml cargo/rustup already use. Pins the
        # aarch64-unknown-none target for the kernel/loader/services.
        rustToolchain =
          pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain

            # QEMU virt target — bundles its option ROMs, so virtio
            # devices work out of the box (the breakage this flake exists
            # to prevent).
            qemu

            # TCP remote-console plumbing used by the net smoke.
            socat

            # `convert` / `magick` for `cargo xtask qemu-screenshot`
            # (framebuffer PPM → PNG).
            imagemagick

            # `tar` for the virtio-blk test disk image.
            gnutar

            # llvm-objcopy / llvm-strip — xtask's ELF→flat and strip
            # steps try these first (rust-objcopy is only the fallback).
            llvm
          ];

          shellHook = ''
            echo "BeetOS dev shell"
            echo "  $(qemu-system-aarch64 --version | head -1)"
            echo "  $(rustc --version)"
            echo "run: cargo xtask qemu-smoke  |  cargo xtask qemu-bench  |  cargo test --workspace"
          '';
        };
      });
}
