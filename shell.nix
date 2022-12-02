  { pkgs ? import <nixpkgs> {}}:
  pkgs.mkShell {
    nativeBuildInputs = [
      pkgs.rustup
      pkgs.sccache
      pkgs.pkg-config
      pkgs.dbus
    ];
    shellHook = ''
      export RUSTC_WRAPPER=sccache
      # export RUST_LOG=info
      rustup override set stable
    '';
  }  
