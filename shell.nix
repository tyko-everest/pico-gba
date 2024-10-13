let
  pkgs = import (fetchTarball("channel:nixpkgs-unstable")) {};
in pkgs.mkShell {
  buildInputs = with pkgs; [
    cargo
    rustc
    cmake
    dbus
    fontconfig
    lldb
    libxkbcommon
    wayland
  ];
  nativeBuildInputs = with pkgs; [
      pkg-config
      fontconfig
  ];
  dbus = pkgs.dbus;
  shellHook = 
    ''
      export RUST_BACKTRACE=1
      export LD_LIBRARY_PATH=${pkgs.wayland}/lib:$LD_LIBRARY_PATH
      export LD_LIBRARY_PATH=${pkgs.libxkbcommon}/lib:$LD_LIBRARY_PATH
    '';
}
