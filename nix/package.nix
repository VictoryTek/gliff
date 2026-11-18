# Builds the workspace: upstream's gliff, gliff-server and gliff-probe plus the
# GNOME crates (pipewire and the bindgen hook are for gnome-capture).
# Dependencies follow pkgbuild/PKGBUILD in this repository.
{ lib
, rustPlatform
, pkg-config
, nasm
, wrapGAppsHook4
, gtk4
, libadwaita
, libva
, libdrm
, libgbm
, libxkbcommon
, dbus
, pipewire
, vulkan-loader
, wayland
, openssh
}:

let
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;
in
rustPlatform.buildRustPackage {
  pname = "gliff";
  inherit version;

  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      let base = baseNameOf path; in
      !(base == "target" || base == ".git" || base == "nix" || base == "flake.nix" || base == "flake.lock");
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config nasm wrapGAppsHook4 rustPlatform.bindgenHook ];
  buildInputs = [ gtk4 libadwaita libva libdrm libgbm libxkbcommon dbus pipewire vulkan-loader wayland ];

  # Unit tests only; the hardware paths are covered by scripts/e2e.sh, which
  # needs a Hyprland session and a GPU.
  doCheck = true;

  postInstall = ''
    install -Dm644 LICENSE "$out/share/licenses/gliff/LICENSE"
    install -Dm644 NOTICE "$out/share/licenses/gliff/NOTICE"
    install -Dm644 pkgbuild/gliff.svg "$out/share/icons/hicolor/scalable/apps/gliff.svg"
    install -Dm644 pkgbuild/gliff.desktop "$out/share/applications/gliff.desktop"
    install -Dm644 docs/hardware-quirks.md "$out/share/doc/gliff/hardware-quirks.md"
  '';

  # The Vulkan loader is opened with dlopen, which the RPATH shrink in the
  # fixup phase would drop. Link it for real on the actual executables (the
  # GApps hook has already turned bin/<name> into a wrapper script).
  postFixup = ''
    for bin in gliff gliff-server gliff-probe; do
      patchelf --add-needed libvulkan.so.1 --add-rpath ${lib.makeLibraryPath [ vulkan-loader ]} "$out/bin/.$bin-wrapped"
    done
  '';

  # ssh is how the viewer reaches gliff-server.
  preFixup = ''
    gappsWrapperArgs+=(--prefix PATH : ${lib.makeBinPath [ openssh ]})
  '';

  meta = {
    description = "Remote desktop over SSH with GPU H.264 and 4:4:4 colour";
    homepage = "https://github.com/VictoryTek/gliff";
    license = with lib.licenses; [ mit bsd2 ];
    platforms = [ "x86_64-linux" "aarch64-linux" ];
    mainProgram = "gliff";
  };
}
