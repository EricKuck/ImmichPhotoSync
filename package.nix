{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "immich-photo-sync";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./build.rs
      ./Info.plist
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Headless macOS daemon that syncs the Apple Photos library to an Immich server";
    homepage = "https://github.com/EricKuck/ImmichPhotoSync";
    license = lib.licenses.mit;
    mainProgram = "immich-photo-sync";
    platforms = lib.platforms.darwin;
  };
}
