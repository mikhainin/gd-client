# Top-level convenience Makefile for g-client.
#
# Targets:
#   make build     - build all workspace crates in release mode
#   make test      - run the workspace test suite
#   make deb       - build .deb packages for gdrived and gdrive-ui (via cargo-deb)
#   make install   - install the built .deb packages (prints the sudo command; see note below)
#   make uninstall - remove the installed packages (prints the sudo command)
#   make clean     - remove build artifacts (cargo target dir + built .deb files)
#
# Requirements:
#   - a Rust toolchain (see AGENTS.md / README.md for the pinned rust-version)
#   - Qt6 + QML modules for gdrive-ui (see scripts/install-qt6.sh)
#   - cargo-deb (`cargo install cargo-deb`) for the `deb` target
#
# Note on `install`/`uninstall`: installing/removing .deb packages needs root
# privileges. Per repository convention (AGENTS.md), this Makefile never
# invokes sudo/apt itself - `make install`/`make uninstall` just print the
# exact command for you to run.

CARGO ?= cargo
DEB_DIR := target/debian
PACKAGES := gdrived gdrive-ui

.PHONY: all build test deb install uninstall clean help

all: build

help:
	@echo "Available targets: build, test, deb, install, uninstall, clean"

build:
	$(CARGO) build --workspace --release

test:
	$(CARGO) test --workspace

deb: build
	@for pkg in $(PACKAGES); do \
		echo "==> building .deb for $$pkg"; \
		$(CARGO) deb -p $$pkg --no-build || exit 1; \
	done
	@echo
	@echo "Built packages:"
	@ls -1 $(DEB_DIR)/*.deb

install: deb
	@echo "This target only prints the install command - it does not run it for you."
	@echo "Run the following yourself:"
	@echo
	@echo "  sudo apt install $(DEB_DIR)/*.deb"
	@echo
	@echo "(apt resolves/installs any missing dependencies, e.g. Qt6 QML modules; use dpkg -i + apt --fix-broken install -f as an alternative)"

uninstall:
	@echo "This target only prints the uninstall command - it does not run it for you."
	@echo "Run the following yourself:"
	@echo
	@echo "  sudo apt remove $(PACKAGES)"

clean:
	$(CARGO) clean
	rm -rf $(DEB_DIR)
