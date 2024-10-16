# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.5.0 - 07-17-2024

### Changed

- Update to version 0.6.0 of `wdl` crate.

## 0.4.0 - 07-01-2024

### Added

- `--except` arg to `check --lint` and `lint` subcommands.

### Changed

- Update to version 0.5.0 of `wdl` crate. This enables lint directive comments (AKA `#@` comments) among other new features.

## 0.3.0 - 06-18-2024

### Added

- `check` subcommand with `--lint` parameter

### Changed

- Update to version 0.4.0 of `wdl` crate. This features a new parser implementation

## 0.2.1 - 06-05-2024

### Fixed

- exit code `2` if there are no parse errors or validation failures, but there are lint warnings.
  - exit code `1` if there are parse errors or validation failures; exit code `0` means there were no concerns found at all.

## 0.2.0 - 06-03-2024

### Added

- `explain` command

### Changed

- Update to version 0.3.0 of `wdl` crate. This pulls in new lint rules.
