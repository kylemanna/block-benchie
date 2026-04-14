{ rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "block-benchie";
  version = "0.1.0";

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };
}
