{
  description = "Baywatch - Lazy-loading reverse proxy daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "baywatch";
          version = "0.1.1";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          postInstall = ''
            install -Dm644 baywatch.service $out/lib/systemd/user/baywatch.service
            install -Dm644 config.example.yaml $out/share/doc/baywatch/config.example.yaml
          '';

          meta = with pkgs.lib; {
            description = "Lazy-loading reverse proxy daemon that spins up services on demand";
            homepage = "https://github.com/bjesus/baywatch";
            license = licenses.mit;
            maintainers = [ ];
            mainProgram = "baywatch";
          };
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      }
    ) // {
      # NixOS module for declarative configuration
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.baywatch;
          settingsFormat = pkgs.formats.yaml { };
        in
        {
          options.services.baywatch = {
            enable = lib.mkEnableOption "baywatch lazy-loading reverse proxy";

            package = lib.mkPackageOption self.packages.${pkgs.system} "default" { };

            settings = lib.mkOption {
              type = settingsFormat.type;
              default = {
                bind = "0.0.0.0";
                port = 80;
                domain = "localhost";
                services = { };
              };
              description = "Baywatch configuration (serialized to YAML)";
              example = lib.literalExpression ''
                {
                  bind = "0.0.0.0";
                  port = 80;
                  domain = "localhost";
                  idle_timeout = 900;
                  services.myapp = {
                    port = 3000;
                    command = "npm start";
                    pwd = "~/projects/myapp";
                    idle_timeout = 300;
                  };
                }
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];

            # Allow binding to privileged ports
            security.wrappers.baywatch = {
              source = "${cfg.package}/bin/baywatch";
              capabilities = "cap_net_bind_service=+ep";
              owner = "root";
              group = "root";
            };

            # Generate config file
            environment.etc."baywatch/config.yaml".source =
              settingsFormat.generate "config.yaml" cfg.settings;
          };
        };
    };
}
