{
  description = "Topomind: semantic FreeCAD context and safe MCP editing";

  inputs = {
    rs-harbor.url = "git+ssh://git@codeberg.org/caniko/rs-harbor.git?ref=trunk&rev=f209ddbca3fdbb0dc31fa3886ccc2ff7369c18ac";

    nixpkgs.follows = "rs-harbor/nixpkgs";
    rust-overlay.follows = "rs-harbor/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {self, nixpkgs, rs-harbor, rust-overlay, flake-utils, ...}:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      toolchain = rs-harbor.lib.mkToolchain {
        inherit pkgs;
        toolchainProfile = "nightly";
        extensions = ["rustfmt"];
        crossTargets = [];
      };
      inherit (toolchain) craneLib;
      cross = rs-harbor.lib.mkCross {
        inherit pkgs system;
        enableOsxcross = false;
      };
      cargoConfig = toolchain.cargoConfig;
      src = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = pkgs.lib.fileset.unions [
          (craneLib.fileset.commonCargoSources ./.)
          ./fixtures
        ];
      };
      commonArgs = {
        inherit src;
        pname = "topomind";
        version = "0.1.0";
        strictDeps = true;
        nativeBuildInputs = [pkgs.pkg-config];
      };
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      package = craneLib.buildPackage (commonArgs // {
        inherit cargoArtifacts;
        cargoExtraArgs = "-p topomind";
      });
      extension = craneLib.buildPackage (commonArgs // {
        inherit cargoArtifacts;
        pname = "topomind-freecad-extension";
        cargoExtraArgs = "-p topomind-freecad-extension";
        nativeBuildInputs = [pkgs.pkg-config pkgs.python3];
        installPhase = ''
          mkdir -p $out/lib
          native=$(find target/release -maxdepth 1 -type f \( -name 'libSemanticMCP_native.so' -o -name 'libSemanticMCP_native.dylib' -o -name 'SemanticMCP_native.dll' \) -print -quit)
          test -n "$native"
          cp "$native" $out/lib/SemanticMCP_native.so
        '';
      });
      cargoTest = craneLib.cargoTest (commonArgs // {
        inherit cargoArtifacts;
      });
      schemaCheck = pkgs.runCommand "topomind-schema-check" {
        nativeBuildInputs = [package];
      } ''
        cp -r ${./schemas} schemas
        ${package}/bin/topomind --validate-schemas --check --root .
        touch $out
      '';
      addon = pkgs.runCommand "topomind-freecad-addon" {
        nativeBuildInputs = [package];
      } ''
        mkdir -p $out
        ${package}/bin/topomind --package-addon --source ${./freecad-addon}/SemanticMCP --native ${extension}/lib/SemanticMCP_native.so --output $out/topomind-freecad-addon.zip
      '';
    in {
      packages.default = package;
      packages.extension = extension;
      packages.addon = addon;

      checks = {
        default = package;
        test = cargoTest;
        clippy = craneLib.cargoClippy (commonArgs // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });
        fmt = craneLib.cargoFmt {inherit src;};
        schemas = schemaCheck;
        extension = extension;
      };

      apps.default = {
        type = "app";
        program = "${package}/bin/topomind";
        meta.description = "Topomind semantic FreeCAD context MCP server";
      };
      apps.addon = {
        type = "app";
        program = "${pkgs.writeShellScript "topomind-package-addon" ''
          exec ${package}/bin/topomind --package-addon --source ${./freecad-addon}/SemanticMCP --native ${extension}/lib/SemanticMCP_native.so --output "''${1:-dist/topomind-freecad-addon.zip}"
        ''}";
        meta.description = "Package the Topomind FreeCAD addon";
      };

      devShells.default = rs-harbor.lib.mkDevShell {
        inherit pkgs cross cargoConfig;
        inherit (toolchain) craneLib;
        enableWindowsEnv = false;
        enableOsxcrossEnv = false;
        checks = self.checks.${system};
        packages = with pkgs; [
          git
          jq
          python3
        ];
        extraShellHook = ''
          export TOPOMIND_SCHEMA_ROOT="${toString ./.}/schemas"
          export TOPOMIND_FIXTURE_ROOT="${toString ./.}/fixtures"
        '';
      };
    });
}
