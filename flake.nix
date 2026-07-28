{
  description = "Topomind: semantic FreeCAD context and safe MCP editing";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {self, nixpkgs, flake-utils, crane, ...}:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {inherit system;};
      craneLib = crane.mkLib pkgs;
      src = pkgs.lib.cleanSourceWith {
        src = ./.;
        filter = path: type:
          pkgs.lib.cleanSourceFilter path type
          && !(type == "directory" && pkgs.lib.hasSuffix "/target" (toString path))
          && !(type == "directory" && pkgs.lib.hasSuffix "/dist" (toString path));
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

      devShells.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          clippy
          gcc
          git
          jq
          pkg-config
          python3
          rust-analyzer
          rustfmt
        ];
        shellHook = ''
          export TOPOMIND_SCHEMA_ROOT="${toString ./.}/schemas"
          export TOPOMIND_FIXTURE_ROOT="${toString ./.}/fixtures"
        '';
      };
    });
}
