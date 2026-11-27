{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager.url = "github:nix-community/home-manager";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
    nixos-base.url = "github:acup1/nixos-base/main";
    nixos-base.inputs.nixpkgs.follows = "nixpkgs";
    nixos-base.inputs.home-manager.follows = "home-manager";
  };
  outputs = { self, nixos-base, home-manager, ... } @ inputs: let
    selector = builtins.getEnv "NIXOS_BASE_CONFIG";
    cfg = import (if selector == "" then ./config.nix else builtins.toPath selector) { inherit inputs; };
    inherit (cfg) system username flakeDir;
    specialArgs = { inherit self inputs system username flakeDir; };
  in {
    nixosConfigurations.${cfg.hostName or @HOST@} = nixos-base.lib.mkNixosSystem {
      config = cfg // { nixosHostConfiguration = ./configuration.nix; };
      extraModules = [
        home-manager.nixosModules.home-manager
        {
          programs.nh.enable = true;
          nix.settings.experimental-features = [ "nix-command" "flakes" ];
          home-manager.useGlobalPkgs = true;
          home-manager.useUserPackages = true;
          home-manager.extraSpecialArgs = specialArgs;
          home-manager.users.${username} = {
            imports = map (bundle: bundle.home or {}) cfg.modules;
            home.stateVersion = "26.05";
          };
        }
      ];
    };
  };
}
