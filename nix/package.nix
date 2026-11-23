{
  lib,
  rustPlatform,
  makeWrapper,
  pkg-config,
  pipewire,
  wireplumber,
  flac,
  curl,
}:

rustPlatform.buildRustPackage {
  pname = "pipewire-recorder";
  version = "0.1.0";

  src = ./..;

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    makeWrapper
    pkg-config
  ];

  nativeCheckInputs = [
    flac
  ];

  postFixup = ''
    wrapProgram $out/bin/pipewire-recorder --prefix PATH : ${
      lib.makeBinPath [
        pipewire
        wireplumber
        flac
        curl
      ]
    }
  '';

  meta = with lib; {
    description = "Background PipeWire stream recorder with MPRIS metadata, cover art, completeness validation, and deduplication";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "pipewire-recorder";
  };
}
