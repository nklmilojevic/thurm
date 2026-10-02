# Home Manager module: `programs.thurm`. The flake exports it as `homeManagerModules.default`.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.thurm;
  toml = pkgs.formats.toml { };
  system = pkgs.stdenv.hostPlatform.system;
  # The app ships its own `thurm` and `thurmd` and updates them with itself, and the CLI must
  # speak its daemon's protocol: on macOS the CLI comes from the app unless asked for.
  defaultPackage =
    if pkgs.stdenv.hostPlatform.isDarwin then null else self.packages.${system}.thurm or null;
  # Where to find `thurm` at activation, whose PATH has neither ~/.local/bin nor the app.
  thurmCandidates =
    if cfg.package != null then
      [ (lib.getExe cfg.package) ]
    else
      [
        "thurm"
        "${config.home.homeDirectory}/.local/bin/thurm"
        "/Applications/Thurm.app/Contents/Helpers/thurm"
        "${config.home.homeDirectory}/Applications/Thurm.app/Contents/Helpers/thurm"
      ];

  configFile = pkgs.concatText "thurm-config.toml" (
    lib.optional (cfg.settings != { }) (toml.generate "thurm-settings.toml" cfg.settings)
    ++ lib.optional (cfg.extraConfig != "") (pkgs.writeText "thurm-extra.toml" "\n${cfg.extraConfig}")
  );

  themeFile =
    name: theme:
    if lib.isAttrs theme && !lib.isDerivation theme then
      toml.generate "thurm-theme-${name}.toml" theme
    else if lib.isString theme && !lib.hasPrefix "/" theme then
      pkgs.writeText "thurm-theme-${name}" theme
    else
      theme;
in
{
  options.programs.thurm = {
    enable = lib.mkEnableOption "Thurm, a terminal for coding agents";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = defaultPackage;
      defaultText = lib.literalMD ''
        `null` on macOS, where the app provides `thurm`; the flake's `thurm` elsewhere
      '';
      description = ''
        The `thurm` and `thurmd` package to install. On macOS the app bundles both and keeps
        them at its own version (use Thurm > Integrations > Install Command-Line Tool); a
        different CLI build may not talk to the app's daemon. On a remote host, pin the
        flake to the app's version.
      '';
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      example = lib.literalExpression ''
        {
          font = {
            family = "JetBrains Mono";
            size = 15;
          };
          colors.theme = "light:catppuccin-latte,dark:catppuccin-mocha";
          window.option_as_alt = "left";
          keybindings."cmd+shift+d" = "split_down";
          remote = [
            {
              name = "devbox";
              host = "devbox";
            }
          ];
        }
      '';
      description = ''
        Configuration written to {file}`$XDG_CONFIG_HOME/thurm/config.toml`. See
        <https://docs.thurm.rs/configuration-reference/> for the options.

        The file is read-only, so changes from the app's settings, `thurm set`,
        `thurm theme NAME` and `thurm remote add` fail: make them here.
      '';
    };

    extraConfig = lib.mkOption {
      type = lib.types.lines;
      default = "";
      description = "TOML appended to {file}`config.toml` after {option}`programs.thurm.settings`.";
    };

    themes = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.oneOf [
          lib.types.path
          toml.type
          lib.types.lines
        ]
      );
      default = { };
      example = lib.literalExpression ''
        {
          my-theme = {
            foreground = "#cdd6f4";
            background = "#1e1e2e";
            palette = [ "#45475a" "#f38ba8" "#a6e3a1" "#f9e2af" "#89b4fa" "#f5c2e7" "#94e2d5" "#bac2de"
                        "#585b70" "#f38ba8" "#a6e3a1" "#f9e2af" "#89b4fa" "#f5c2e7" "#94e2d5" "#a6adc8" ];
          };
        }
      '';
      description = ''
        Theme files written to {file}`$XDG_CONFIG_HOME/thurm/themes/<name>.toml`: Thurm's
        TOML as an attribute set, or a file or text in either supported format. Select one
        with `colors.theme = "~/.config/thurm/themes/<name>.toml"`.
      '';
    };

    reloadOnChange = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run `thurm reload` on activation when the config changes and the daemon is running.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = lib.optional (cfg.package != null) cfg.package;

    xdg.configFile = {
      "thurm/config.toml" = lib.mkIf (cfg.settings != { } || cfg.extraConfig != "") {
        source = configFile;
        onChange = lib.mkIf cfg.reloadOnChange ''
          for thurm in ${lib.escapeShellArgs thurmCandidates}; do
            if command -v "$thurm" >/dev/null 2>&1; then
              sock="$("$thurm" socket-path 2>/dev/null || true)"
              if [ -n "$sock" ] && [ -S "$sock" ]; then
                run "$thurm" reload >/dev/null 2>&1 || true
              fi
              break
            fi
          done
        '';
      };
    }
    // lib.mapAttrs' (
      name: theme: lib.nameValuePair "thurm/themes/${name}.toml" { source = themeFile name theme; }
    ) cfg.themes;
  };
}
