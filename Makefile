.PHONY: build build-all build-macos build-linux build-windows macos-app help

TARGET ?=
CARGO_TARGET_ARG := $(if $(TARGET),--target $(TARGET),)
RELEASE_DIR := target/$(if $(TARGET),$(TARGET)/)release

build:
	cargo build --release $(CARGO_TARGET_ARG)
ifneq ($(OS),Windows_NT)
	@if [ "$(TARGET)" = "aarch64-apple-darwin" ] || { [ -z "$(TARGET)" ] && [ "$$(uname -s)" = "Darwin" ]; }; then \
		$(MAKE) macos-app RELEASE_DIR="$(RELEASE_DIR)"; \
	fi
endif

build-macos:
	$(MAKE) build TARGET=aarch64-apple-darwin

build-linux:
	$(MAKE) build TARGET=x86_64-unknown-linux-gnu

build-windows:
	$(MAKE) build TARGET=x86_64-pc-windows-msvc

build-all: build-macos build-linux build-windows

macos-app:
	mkdir -p "$(RELEASE_DIR)/Harness Hat.app/Contents/MacOS" "$(RELEASE_DIR)/Harness Hat.app/Contents/Resources"
	cp "$(RELEASE_DIR)/hat-launcher" "$(RELEASE_DIR)/Harness Hat.app/Contents/MacOS/hat-launcher"
	cp "$(RELEASE_DIR)/hat" "$(RELEASE_DIR)/Harness Hat.app/Contents/MacOS/hat"
	cp "$(RELEASE_DIR)/hat-daemon" "$(RELEASE_DIR)/Harness Hat.app/Contents/MacOS/hat-daemon"
	cp packaging/macos/Info.plist "$(RELEASE_DIR)/Harness Hat.app/Contents/Info.plist"
	cp packaging/macos/HarnessHat.icns "$(RELEASE_DIR)/Harness Hat.app/Contents/Resources/HarnessHat.icns"
	@echo "Built $(RELEASE_DIR)/Harness Hat.app"

help:
	@echo "make build          Build for the current host; bundle Harness Hat.app on macOS"
	@echo "make build-macos    Build ARM64 macOS binaries and app bundle"
	@echo "make build-linux    Build x86_64 Linux binaries"
	@echo "make build-windows  Build x86_64 Windows binaries"
	@echo "make build-all      Build every target (requires all targets and cross-linkers)"
