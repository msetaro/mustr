# Flake with outputs
{
  description = "twice";
  outputs = { self, nixpkgs }: let n = 3; in { inherit n; };
}
