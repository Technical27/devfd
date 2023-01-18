{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    sqlx-cli
    file
  ];

  DATABASE_URL = "sqlite:test.db";
}
