{ pkgs ? import <nixpkgs> { } }:

let
  # Include directly linked libraries and the libraries loaded dynamically by
  # the desktop WebView/tray integration. Build inputs alone do not make these
  # discoverable by Cargo-built executables at runtime on NixOS.
  desktopLibraries = with pkgs; [
    gtk3
    webkitgtk_4_1
    libsoup_3
    openssl
    xdotool
    glib
    cairo
    gdk-pixbuf
    libappindicator-gtk3
    libxkbcommon
    libglvnd
  ];

  schemaPackages = with pkgs; [
    gtk3
    gsettings-desktop-schemas
  ];
  schemaDataDirs = pkgs.lib.concatMapStringsSep ":"
    (package: "${package}/share/gsettings-schemas/${package.name}")
    schemaPackages;
in
pkgs.mkShell {
  name = "psa-desktop";

  # Use the developer's existing rustup toolchain, as for the rest of the
  # workspace, rather than switching Rust versions when entering this shell.
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = desktopLibraries ++ [ pkgs.gsettings-desktop-schemas ];

  shellHook = ''
    export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath desktopLibraries}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export XDG_DATA_DIRS="${schemaDataDirs}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
  '';
}
