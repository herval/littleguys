# littleguys: everyday commands. Run `make` to list them.

.DEFAULT_GOAL := help
.PHONY: help install run mock build release release-secrets install-app check test clean

# One universal binary: runs on Apple silicon and Intel Macs.
TARGET := universal-apple-darwin
APP := src-tauri/target/$(TARGET)/release/bundle/macos/littleguys.app
UPDATER_KEY := $(HOME)/.tauri/littleguys-updater.key

# install-app only needs to run on this Mac, so it builds just this Mac's architecture and
# skips the release profile's fat LTO + single codegen unit, which spend minutes on one core
# shrinking the binary.
NATIVE := $(shell [ "$$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ] && echo aarch64 || echo x86_64)-apple-darwin
LOCAL_PROFILE := CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
APPS ?= /Applications

help: ## List the commands
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-16s %s\n", $$1, $$2}'

install: ## Install dependencies (needs Node 22+, pnpm and Rust)
	@command -v pnpm >/dev/null || { echo "pnpm not found: run 'corepack enable' or 'npm i -g pnpm'"; exit 1; }
	@command -v cargo >/dev/null || { echo "Rust not found: install it from https://rustup.rs"; exit 1; }
	rustup target add aarch64-apple-darwin x86_64-apple-darwin
	pnpm install
	cd src-tauri && cargo fetch

run: ## Run the app, with a hot-reloading UI
	pnpm tauri dev

mock: ## Run only the UI in a browser, against a mock gateway
	@echo "Open http://localhost:1420/main.html"
	pnpm dev

build: ## Build littleguys.app and a .dmg (universal: Apple silicon + Intel)
	pnpm tauri build --target $(TARGET)

release: ## Same as build, plus the signed update bundle (.app.tar.gz + .sig) the updater installs
	@test -f $(UPDATER_KEY) || { echo "No updater key at $(UPDATER_KEY)"; exit 1; }
	TAURI_SIGNING_PRIVATE_KEY=$(UPDATER_KEY) TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$$(cat $(UPDATER_KEY).password)" \
		pnpm tauri build --target $(TARGET) --config src-tauri/tauri.release.conf.json

release-secrets: ## One-time: put the signing, notarization and tap credentials in GitHub secrets
	scripts/release-secrets.sh

install-app: ## Build littleguys.app for this Mac and put it in /Applications, quitting a running copy first
	$(LOCAL_PROFILE) pnpm tauri build --target $(NATIVE) --bundles app
	@if pgrep -qx littleguys; then osascript -e 'tell application id "ai.littleguys.app" to quit' >/dev/null 2>&1; sleep 2; pkill -x littleguys || true; fi
	rm -rf "$(APPS)/littleguys.app"
	cp -R src-tauri/target/$(NATIVE)/release/bundle/macos/littleguys.app "$(APPS)/"
	@echo "Installed $(APPS)/littleguys.app"

check: ## Typecheck the UI and the Rust core
	pnpm typecheck
	cd src-tauri && cargo check

test: ## Run the Rust tests
	cd src-tauri && cargo test

clean: ## Remove build output
	rm -rf dist
	cd src-tauri && cargo clean
