# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/so1ve/wl-clipboard-watch/releases/tag/v0.1.0) - 2026-08-19

### Added

- implement functionality

### Other

- init

### Added

- Blocking clipboard selection watcher with `ext-data-control-v1` support and
  `wlr-data-control-v1` fallback.
- Bounded, lazy per-MIME transfers.
- Transfer timeouts and stale-selection detection.
