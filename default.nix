let
  # nixpkgs-21.05
  nixpkgsRev = "970cb0a6dd23309f81bc2629de62b22f930fb218";
  defaultNixpkgs = builtins.fetchTarball "github.com/NixOS/nixpkgs/archive/${nixpkgsRev}.tar.gz";
in
{ nixpkgs ? defaultNixpkgs }:

with (import nixpkgs {}); with rustPlatform;

buildRustPackage rec {
  name = "nix-index-${version}";
  version = "0.1.2";

  src = builtins.filterSource (name: type: !lib.hasPrefix "target" (baseNameOf name) && !lib.hasPrefix "result" (baseNameOf name) && name != ".git") ./.;
  buildInputs = [pkgconfig openssl curl] ++ lib.optionals stdenv.isDarwin [ darwin.apple_sdk.frameworks.Security ] ;
  cargoSha256 = "0jndrr1lyvbc5fjbm9hnxis3hkd120awipnqnqqnaddqhvlsx85f";

  postInstall = ''
    mkdir -p $out/etc/profile.d
    cp ${./command-not-found.sh} $out/etc/profile.d/command-not-found.sh
    substituteInPlace $out/etc/profile.d/command-not-found.sh \
      --replace "@out@" "$out"
  '';

  meta = with lib; {
    description = "A files database for nixpkgs";
    homepage = https://github.com/bennofs/nix-index;
    license = with licenses; [ bsd3 ];
    maintainers = [ maintainers.bennofs ];
    platforms = platforms.all;
  };
}
