{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    sqlx-cli
  ];

  DATABASE_URL = "sqlite:test.db";
}
