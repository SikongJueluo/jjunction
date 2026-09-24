{
  lib,
  rustPlatform,
  makeWrapper,
  git,
  direnv,
  root,
}:

# The `jjn` CLI. Wraps git (and direnv, used for workspace `allow`) into PATH
# so the tool works in pure shells; both are invoked as subprocesses.
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "jjunction";
  version = "0.1.0";

  src = root;

  cargoLock.lockFile = root + "/Cargo.lock";

  nativeBuildInputs = [ makeWrapper ];

  # The test suite shells out to git against local-path fixtures.
  nativeCheckInputs = [ git ];

  postInstall = ''
    wrapProgram $out/bin/jjn --prefix PATH : ${
      lib.makeBinPath [
        git
        direnv
      ]
    }

    # Standard completion dirs: NixOS/Home Manager users get these for free;
    # everyone else can `jjn completions <shell>` (starship-style).
    $out/bin/jjn completions bash > jjn.bash
    $out/bin/jjn completions fish > jjn.fish
    $out/bin/jjn completions zsh > _jjn
    install -Dm644 jjn.bash $out/share/bash-completion/completions/jjn
    install -Dm644 jjn.fish $out/share/fish/vendor_completions.d/jjn.fish
    install -Dm644 _jjn $out/share/zsh/site-functions/_jjn
  '';

  meta = {
    description = "A collection of tools for the Jujutsu (jj) version control system";
    homepage = "https://github.com/sikongjueluo/jjunction";
    mainProgram = "jjn";
    platforms = lib.platforms.linux;
    license = lib.licenses.gpl3Plus;
  };
})
