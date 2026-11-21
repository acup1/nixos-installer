{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager.url = "github:nix-community/home-manager";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
  };
  outputs = { self, nixpkgs, home-manager, ... } @ inputs: let
    cfg = import ./config.nix { inherit inputs; };
    inherit (cfg) system username flakeDir;
    specialArgs = { inherit self inputs system username flakeDir; };
  in {
    nixosConfigurations.@HOST@ = nixpkgs.lib.nixosSystem {
      inherit system specialArgs;
      modules = [
        ./configuration.nix
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
      ] ++ map (bundle: bundle.nixos or {}) cfg.modules;
    };
  };
}
