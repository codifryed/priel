# priel - build, test and install
#
# Three audiences, one file:
#   distro packaging   make DESTDIR=%{buildroot} PREFIX=/usr install
#   development        make check
#   install from source  make && sudo make install
#
# Follows the GNU install conventions (DESTDIR, PREFIX, BINDIR) so a packager can
# stage into a buildroot without patching anything. Every variable below can be
# overridden on the command line.

CARGO      ?= cargo
INSTALL    ?= install
DESTDIR    ?=
PREFIX     ?= /usr/local
BINDIR     ?= $(PREFIX)/bin
DATADIR    ?= $(PREFIX)/share
DOCDIR     ?= $(DATADIR)/doc/priel
LICENSEDIR ?= $(DATADIR)/licenses/priel
MANDIR     ?= $(DATADIR)/man
BASHCOMPDIR ?= $(DATADIR)/bash-completion/completions
ZSHCOMPDIR  ?= $(DATADIR)/zsh/site-functions
FISHCOMPDIR ?= $(DATADIR)/fish/vendor_completions.d

# `--locked` keeps a build reproducible from the committed lockfile. Distro
# builds usually add `--offline` on top, after `make vendor`.
CARGO_FLAGS ?= --locked

NAME    := priel
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BIN     := target/release/$(NAME)
ASSETS  := target/assets

.DEFAULT_GOAL := all
.PHONY: all help build release run test test-all lint fmt fmt-check check \
        coverage build-nolibmpv check-deps assets install uninstall dist vendor clean

all: release ## Build the release binary (default)

help: ## List the available targets
	@echo "priel $(VERSION)"
	@echo
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "Install paths honour DESTDIR, PREFIX ($(PREFIX)) and BINDIR ($(BINDIR))."

# ---- building ----

build: ## Debug build of the whole workspace
	$(CARGO) build $(CARGO_FLAGS)

release: ## Optimised build (fat LTO, one codegen unit; slower to compile)
	$(CARGO) build $(CARGO_FLAGS) --release -p priel-tui

build-nolibmpv: ## UI-only build that needs no libmpv headers; playback is a no-op
	$(CARGO) build $(CARGO_FLAGS) -p priel-tui --no-default-features

run: ## Run the debug binary; pass arguments with ARGS="--device ..."
	$(CARGO) run $(CARGO_FLAGS) -p priel-tui -- $(ARGS)

# ---- development ----

test: ## Run the test suite
	$(CARGO) test $(CARGO_FLAGS) --workspace

test-all: test ## Test both feature configurations
	$(CARGO) test $(CARGO_FLAGS) -p priel-tui --no-default-features
	$(CARGO) test $(CARGO_FLAGS) -p priel-player --no-default-features

lint: ## Clippy (pedantic) over both feature configurations
	$(CARGO) clippy $(CARGO_FLAGS) --workspace --all-targets -- -D warnings
	$(CARGO) clippy $(CARGO_FLAGS) -p priel-tui --no-default-features --all-targets -- -D warnings

fmt: ## Reformat in place
	$(CARGO) fmt --all

fmt-check: ## Fail if anything is unformatted
	$(CARGO) fmt --all --check

check: fmt-check lint test-all ## Everything CI runs; the gate before a commit

# A test binary can die on a signal rather than fail a test. cargo then reports
# no FAILED line and no assertion - just a non-zero exit - so anyone grepping
# the log for a failing test finds nothing and reads it as "could not
# reproduce". That is exactly how a use-after-free across the mpv FFI boundary
# survived for a day. This target names that case and says where the evidence
# already is.
check-signals: ## Run the suite and say plainly if a test binary was killed
	@set -o pipefail; $(CARGO) test $(CARGO_FLAGS) --workspace 2>&1 | tee /tmp/priel-check.log; \
	status=$$?; \
	if grep -qE "signal: [0-9]+|SIGSEGV|SIGABRT|SIGILL|SIGBUS" /tmp/priel-check.log; then \
	  echo ""; \
	  echo "!! A TEST BINARY DIED ON A SIGNAL. No test failed; the process was killed."; \
	  echo "!! There is no FAILED line and no assertion to find - do not read this as a"; \
	  echo "!! flake. The core dump is already captured:"; \
	  echo "!!     coredumpctl list | grep priel"; \
	  echo "!!     coredumpctl info <PID>   # the stack trace names the faulting frame"; \
	  exit 1; \
	fi; \
	exit $$status

coverage: ## Line-coverage summary (needs cargo-llvm-cov)
	@$(CARGO) llvm-cov --version >/dev/null 2>&1 \
		|| { echo "cargo-llvm-cov is not installed: cargo install cargo-llvm-cov"; exit 1; }
	$(CARGO) llvm-cov --workspace --summary-only

check-deps: ## Verify the build dependencies are present
	@command -v $(CARGO) >/dev/null \
		|| { echo "missing: cargo (install rustup, or your distro's rust toolchain)"; exit 1; }
	@pkg-config --exists mpv \
		|| { echo "missing: libmpv development files (mpv-devel, libmpv-dev)"; exit 1; }
	@echo "build dependencies ok: $$($(CARGO) --version), libmpv $$(pkg-config --modversion mpv)"

assets: ## Generate the man page and shell completions from the CLI definition
	$(CARGO) run $(CARGO_FLAGS) --features gen-assets --bin priel-gen-assets -- $(ASSETS)

# ---- installing ----

install: release assets ## Install binary, man page, completions, licence and docs
	$(INSTALL) -Dm0755 $(BIN) $(DESTDIR)$(BINDIR)/$(NAME)
	$(INSTALL) -Dm0644 COPYING $(DESTDIR)$(LICENSEDIR)/COPYING
	$(INSTALL) -Dm0644 $(ASSETS)/$(NAME).1 $(DESTDIR)$(MANDIR)/man1/$(NAME).1
	$(INSTALL) -Dm0644 $(ASSETS)/$(NAME).bash $(DESTDIR)$(BASHCOMPDIR)/$(NAME)
	$(INSTALL) -Dm0644 $(ASSETS)/_$(NAME) $(DESTDIR)$(ZSHCOMPDIR)/_$(NAME)
	$(INSTALL) -Dm0644 $(ASSETS)/$(NAME).fish $(DESTDIR)$(FISHCOMPDIR)/$(NAME).fish
	@# Documentation is optional so a stripped-down tarball still installs; the
	@# licence above is not.
	@if [ -f README.md ]; then \
		$(INSTALL) -Dm0644 README.md $(DESTDIR)$(DOCDIR)/README.md; \
	else \
		echo "note: README.md is absent, installing without documentation"; \
	fi

uninstall: ## Remove what `install` put down
	rm -f $(DESTDIR)$(BINDIR)/$(NAME)
	rm -f $(DESTDIR)$(DOCDIR)/README.md
	rm -f $(DESTDIR)$(LICENSEDIR)/COPYING
	rm -f $(DESTDIR)$(MANDIR)/man1/$(NAME).1
	rm -f $(DESTDIR)$(BASHCOMPDIR)/$(NAME)
	rm -f $(DESTDIR)$(ZSHCOMPDIR)/_$(NAME)
	rm -f $(DESTDIR)$(FISHCOMPDIR)/$(NAME).fish
	-rmdir $(DESTDIR)$(DOCDIR) $(DESTDIR)$(LICENSEDIR) 2>/dev/null || true

# ---- packaging ----

dist: ## Source tarball of HEAD, named for the crate version
	git archive --format=tar.gz --prefix=$(NAME)-$(VERSION)/ \
		-o $(NAME)-$(VERSION).tar.gz HEAD
	@echo "wrote $(NAME)-$(VERSION).tar.gz"

vendor: ## Vendor the crate dependencies for an offline build
	$(CARGO) vendor $(CARGO_FLAGS) vendor
	@echo
	@echo "Add the printed [source] block to .cargo/config.toml, then build with"
	@echo "  make CARGO_FLAGS='--locked --offline'"

clean: ## Remove build output and generated tarballs
	$(CARGO) clean
	rm -f $(NAME)-*.tar.gz
