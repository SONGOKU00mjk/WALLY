{
  description = "wally — a minimal Wayland wallpaper setter with first-class GIF and animated WebP support";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          wally = pkgs.rustPlatform.buildRustPackage {
            pname = "wally";
            version = "1.0.0";

            src = self;

            cargoLock.lockFile = ./Cargo.lock;

            # Default feature set, matching install.sh and the release
            # artifacts: AVIF decoding ON (pure-Rust rav1d — needs nasm on
            # x86_64; aarch64 takes the pure-Rust path), JXL OFF.
            nativeBuildInputs = with pkgs; [ nasm ];

            doCheck = true;

            meta = with pkgs.lib; {
              description = "Minimal Wayland wallpaper setter with first-class GIF and animated WebP support";
              homepage = "https://github.com/SONGOKU00mjk/WALLY";
              license = licenses.gpl3Plus;
              platforms = platforms.linux;
              mainProgram = "wally";
            };
          };
        in
        {
          wally = wally;
          default = wally;
        }
      );

      apps = forAllSystems (
        pkgs: {
          default = {
            type = "app";
            program = "${self.packages.${pkgs.system}.wally}/bin/wally";
          };
        }
      );
    };
}
