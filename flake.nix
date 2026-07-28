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
      cargoTest = craneLib.cargoTest (commonArgs // {
        inherit cargoArtifacts;
      });
      schemaCheck = pkgs.runCommand "topomind-schema-check" {
        nativeBuildInputs = [pkgs.python3];
      } ''
        cp -r ${./schemas} schemas
        cp -r ${./scripts} scripts
        cp -r ${./freecad-addon} freecad-addon
        chmod -R u+w schemas freecad-addon
        python scripts/validate_schemas.py
        touch $out
      '';
      pythonCheck = pkgs.runCommand "topomind-python-check" {
        nativeBuildInputs = [pkgs.python3];
      } ''
        cp -r ${./tests} tests
        cp -r ${./freecad-addon} freecad-addon
        python -m unittest discover -s tests -p 'test_*.py'
        touch $out
      '';
      addon = pkgs.runCommand "topomind-freecad-addon" {
        nativeBuildInputs = [pkgs.python3];
      } ''
        mkdir -p $out
        python ${./scripts/package_addon.py} --source ${./freecad-addon}/SemanticMCP --output $out/topomind-freecad-addon.zip
      '';
    in {
      packages.default = package;
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
        python = pythonCheck;
      };

      apps.default = {
        type = "app";
        program = "${package}/bin/topomind";
        meta.description = "Topomind semantic FreeCAD context MCP server";
      };
      apps.addon = {
        type = "app";
        program = "${pkgs.writeShellScript "topomind-package-addon" ''
          exec ${pkgs.python3}/bin/python ${./scripts/package_addon.py} --source ${./freecad-addon}/SemanticMCP --output "''${1:-dist/topomind-freecad-addon.zip}"
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
