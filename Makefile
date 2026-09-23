# Building is not enough on this project.
#
# The eBPF and perf backends need CAP_BPF and CAP_PERFMON, and file
# capabilities do not survive a relink: every `cargo build` drops them. On this
# machine that is the difference between 86 usable sources and 51, and nothing
# announces it -- the tool simply reports fewer things as available, which looks
# like the machine changed rather than the binary. So the build grants them.
#
#   make            build the workspace and grant the capabilities
#   make test       the full suite, after a build that has them
#   make check      test, clippy and fmt, the way CI would
#   make demo       the guided tour, against this machine (about seven seconds)
#   make demo-all   the tour and then all 141 sessions
#   make man        the manual page
#   make release    the same for an optimised build
#
# `cargo build` still works and is still correct; it just leaves the privileged
# backends reporting the capability they need.

PROFILE   ?= debug
BINARY     = target/$(PROFILE)/omn
CAPS       = cap_bpf,cap_perfmon+ep
CARGOFLAGS = $(if $(filter release,$(PROFILE)),--release,)

.PHONY: all build release caps test check demo demo-all man fmt clippy clean

all: build

# `--workspace` is not optional: the preload shim is a member nothing depends
# on, so a plain `cargo build` leaves `mem.heap.alloc` without the library it
# loads, and the failure surfaces as a resolver choosing a different backend.
build:
	cargo build --workspace $(CARGOFLAGS)
	@$(MAKE) --no-print-directory caps PROFILE=$(PROFILE)

release:
	@$(MAKE) --no-print-directory build PROFILE=release

# Asks for the capabilities without ever prompting.
#
# `sudo -n` on purpose: a build that asks for a password blocks forever wherever
# nobody is there to type one, and "nobody is there" is not the same as "no
# terminal" -- a pty with no human behind it is exactly what a build hook, a CI
# runner and an agent shell all have. This target found that out by hanging on
# its own author.
#
# So it succeeds silently when sudo is already available, and otherwise prints
# the one command to run. To stop needing it at all, one narrowly scoped line
# under `visudo -f /etc/sudoers.d/omniscient`, permitting exactly this command
# on exactly this path:
#
#   <user> ALL=(root) NOPASSWD: /usr/sbin/setcap $(CAPS) <absolute path to $(BINARY)>
caps:
	@if getcap $(BINARY) 2>/dev/null | grep -q cap_bpf; then \
	    echo "capabilities already on $(BINARY)"; \
	elif sudo -n setcap $(CAPS) $(BINARY) 2>/dev/null; then \
	    echo "granted $(CAPS) to $(BINARY)"; \
	else \
	    echo "note: $(BINARY) has no capabilities, so the eBPF and per-cpu perf"; \
	    echo "      backends will report the one they need instead of collecting."; \
	    echo "      grant them with: sudo setcap $(CAPS) $(BINARY)"; \
	    echo "      or once and for all, see the sudoers line in the Makefile."; \
	fi

# The integration tests run `target/<profile>/omn`, so they take the privileged
# path once the capabilities are on it. The in-process unit tests do not: their
# binaries are rebuilt under a fresh hash every run and cannot carry
# capabilities, so the eBPF ones skip and say so. A green suite on an
# unprivileged machine is not evidence about the eBPF layer.
test: build
	cargo test --workspace $(CARGOFLAGS)

check: test
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

# Eight questions, answered against this machine. A block that cannot run here
# says why, which is half of what is being demonstrated.
#
# `scripts/demo.sh --list` names every session, `scripts/demo.sh <family>` runs
# one family's, and GUIDE.md explains what each one shows.
demo: build
	@scripts/demo.sh

demo-all: build
	@scripts/demo.sh --all

# The manual page, rendered.
man:
	@groff -man -Tascii man/omn.1 | less -R

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets

clean:
	cargo clean
