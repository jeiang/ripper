{
  description = "rip: move files to the freedesktop.org trash, across btrfs subvolumes and bind mounts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      linux = [ "x86_64-linux" "aarch64-linux" ];
      forSystems = systems: f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      ripper = pkgs: pkgs.rustPlatform.buildRustPackage {
        pname = "ripper";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.installShellFiles pkgs.makeWrapper ];
        # The sandbox tests need bwrap, user namespaces and btrfs; the build
        # sandbox has none of them. `just test` runs them on Linux.
        cargoTestFlags = [ "--bins" ];
        # The fish file is static. bash and zsh come from running the binary,
        # which needs a build machine that can execute it.
        postInstall = ''
          installShellCompletion --cmd rip --fish completions/rip.fish
        '' + lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
          installShellCompletion --cmd rip \
            --bash <($out/bin/rip --completions bash) \
            --zsh <($out/bin/rip --completions zsh)
        '';
        # fzf runs the picker; GNU cp makes cross-filesystem copies. Prefixed, so
        # another `cp` earlier on the user's PATH cannot change how rip copies.
        postFixup = ''
          wrapProgram $out/bin/rip --prefix PATH : ${lib.makeBinPath [ pkgs.coreutils pkgs.fzf ]}
        '';
        meta = {
          description = "Move files to the freedesktop.org trash, across btrfs subvolumes and bind mounts";
          mainProgram = "rip";
          platforms = lib.platforms.linux;
        };
      };
    in
    {
      packages = forSystems linux (pkgs: { default = ripper pkgs; ripper = ripper pkgs; });
      checks = forSystems linux (pkgs: { build = ripper pkgs; });
      # darwin: editing, `cargo fmt` and `cargo generate-lockfile` on zakkart only.
      devShells = forSystems (linux ++ [ "aarch64-darwin" ]) (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer just ]
            # Sandbox tests: bwrap, btrfs subvolumes, `script` for terminals, fish and fzf.
            # zsh and bash-completion: manually checking the generated bash/zsh
            # completions (`rip --completions bash|zsh`) in a real shell.
            ++ lib.optionals stdenv.hostPlatform.isLinux [ bubblewrap btrfs-progs util-linux fish fzf zsh bash-completion ];
        };
      });
    };
}
