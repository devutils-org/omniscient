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
#   make demo-all   the tour and then all 145 sessions
#   make matrix     the tool under other kernels in KVM (about 70 minutes)
#   make man        the manual page
#   make release    the same for an optimised build
#   make install    an optimised build into $(PREFIX), capabilities left to you;
#                   as root it only copies (`make release && sudo make install`)
#   make install-check  install into a scratch prefix and run it from there
#   make dist       the release archive and its checksum (scripts/release.sh)
#
# `cargo build` still works and is still correct; it just leaves the privileged
# backends reporting the capability they need.

PROFILE   ?= debug
BINARY     = target/$(PROFILE)/omn
comma     := ,
# `make CAPTURE=1` adds CAP_NET_RAW, which only `net.capture` needs and which is
# never granted by default: it also lets the binary send arbitrary frames, which
# is not observing. DECISIONS.md, "Settled in the tenth wave", has the reasoning.
CAPS       = cap_bpf,cap_perfmon$(if $(CAPTURE),$(comma)cap_net_raw,)+ep
CAPCHECK   = $(if $(CAPTURE),cap_net_raw,cap_bpf)
CARGOFLAGS = $(if $(filter release,$(PROFILE)),--release,)

.PHONY: vertical-server triage-fixture all build release caps test check demo demo-all matrix man fmt clippy clean \
        install uninstall install-check dist dist-verify

all: build

# `--workspace` is not optional: the preload shim is a member nothing depends
# on, so a plain `cargo build` leaves `mem.heap.alloc` without the library it
# loads, and the failure surfaces as a resolver choosing a different backend.
build:
	cargo build --workspace $(CARGOFLAGS)
	@$(MAKE) --no-print-directory caps PROFILE=$(PROFILE)
	@$(MAKE) --no-print-directory vertical-server
	@$(MAKE) --no-print-directory triage-fixture

# The vertical case's synthetic server, which `examples/vertical-*.yml` launch
# by this path. Built here so the sessions run after `make` alone, and built by
# `tests/vertical.rs` from the same source, so the two cannot drift. Never
# fatal: a machine without a C compiler or without <sys/sdt.h> loses these two
# sessions, says so, and keeps everything else.
VERTICAL_SERVER = target/omn-vertical/server
vertical-server:
	@mkdir -p $(dir $(VERTICAL_SERVER))
	@if cc -O0 -pthread -o $(VERTICAL_SERVER).partial examples/vertical/server.c 2>/dev/null; then \
	    mv $(VERTICAL_SERVER).partial $(VERTICAL_SERVER); \
	else \
	    rm -f $(VERTICAL_SERVER).partial; \
	    echo "note: the vertical-case server did not build (no cc, or no <sys/sdt.h>);"; \
	    echo "      examples/vertical-*.yml will say so when they are run."; \
	fi

# The workload `examples/triage-*.yml` launch, built by `tests/triage.rs` from
# the same source and with the same flags: frame pointers in leaf functions
# too, because `omn diff` names a function by its leaf frame. Never fatal, like
# the server above.
TRIAGE_FIXTURE = target/omn-triage-fixture/workload
triage-fixture:
	@mkdir -p $(dir $(TRIAGE_FIXTURE))
	@if cc -O1 -g -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer -fno-inline -pthread \
	      -o $(TRIAGE_FIXTURE).partial src/triage/workload.c 2>/dev/null; then \
	    mv $(TRIAGE_FIXTURE).partial $(TRIAGE_FIXTURE); \
	else \
	    rm -f $(TRIAGE_FIXTURE).partial; \
	    echo "note: the triage fixture did not build (no cc); examples/triage-*.yml need it"; \
	fi

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
# on exactly these paths -- the checkout and the per-stream worktrees beside it,
# since a sudoers `*` does not cross a `/`:
#
#   <user> ALL=(root) NOPASSWD: /usr/bin/setcap cap_bpf\,cap_perfmon+ep /path/to/omniscient*/target/debug/omn
#
# The comma is escaped because sudoers reads a bare one as the end of the
# command, and the path is absolute because sudo matches arguments literally
# (`docs/open-findings.md` finding 10). The recipe asks with the absolute path
# first, then with the relative one, which is what an older rule permits: a
# rule on `target/debug/omn` matches that name under any directory at all.
caps:
	@if getcap $(BINARY) 2>/dev/null | grep -q $(CAPCHECK); then \
	    echo "capabilities already on $(BINARY)"; \
	elif sudo -n setcap $(CAPS) $(abspath $(BINARY)) 2>/dev/null \
	    || sudo -n setcap $(CAPS) $(BINARY) 2>/dev/null; then \
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

# The portability matrix: docs/portability.md. Boots prebuilt kernels in KVM
# under virtme-ng; about 70 minutes and 2 GB of kernels under target/matrix/.
# `scripts/matrix.sh here` is this machine alone, in about two.
matrix: build
	@scripts/matrix.sh

# Installing. docs/install.md is the user's side of these targets.
#
# The layout is the release archive's, so a tree installed from here and one
# extracted from `make dist` are the same tree: the binaries and the preload
# shim in bin/, side by side, because `omn` looks for the shim beside its own
# executable and `omn-mcp` looks for `omn` beside its own; the manual page in
# share/man/man1.
#
# The build is `cargo build --release` without the capability step: a
# capability granted in target/ would not survive the copy, and granting one
# on the installed path is the user's decision. So the target says how.
#
# As root it does not build. `sudo make install` would run cargo as root,
# which is usually not on root's PATH under rustup and, when it is, leaves
# root-owned files in target/ that break the user's next `make`. So a
# system-wide install is `make release && sudo make install`: the build is the
# user's, and root only copies it, after `release.sh built` has checked that
# target/release is not older than its sources.
# DESTDIR is honoured for packaging, and the command it prints names the path
# the package will install to, which is where the grant has to happen.
PREFIX  ?= /usr/local
DESTDIR ?=
BINDIR   = $(PREFIX)/bin
MAN1DIR  = $(PREFIX)/share/man/man1
DOCDIR   = $(PREFIX)/share/doc/omn

install:
	@if [ "$$(id -u)" = 0 ]; then \
	    scripts/release.sh built; \
	else \
	    echo "cargo build --workspace --release --locked"; \
	    cargo build --workspace --release --locked; \
	fi
	install -D -m 755 target/release/omn            $(DESTDIR)$(BINDIR)/omn
	install -D -m 755 target/release/omn-mcp        $(DESTDIR)$(BINDIR)/omn-mcp
	install -D -m 644 target/release/libomn_preload.so $(DESTDIR)$(BINDIR)/libomn_preload.so
	install -D -m 644 man/omn.1                     $(DESTDIR)$(MAN1DIR)/omn.1
	install -D -m 644 README.md                     $(DESTDIR)$(DOCDIR)/README.md
	install -D -m 644 docs/install.md               $(DESTDIR)$(DOCDIR)/install.md
	@echo
	@echo "installed omn, omn-mcp and the preload shim in $(DESTDIR)$(BINDIR)."
	@echo "no capability was granted. Without them the eBPF and per-cpu perf backends"
	@echo "report what they need instead of collecting; to grant them:"
	@echo
	@echo "    sudo setcap cap_bpf,cap_perfmon+ep $(BINDIR)/omn"
	@echo
	@echo "and \`omn list\` says what this machine then serves. docs/install.md has why."

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/omn $(DESTDIR)$(BINDIR)/omn-mcp $(DESTDIR)$(BINDIR)/libomn_preload.so
	rm -f $(DESTDIR)$(MAN1DIR)/omn.1 $(DESTDIR)$(DOCDIR)/README.md $(DESTDIR)$(DOCDIR)/install.md
	-rmdir $(DESTDIR)$(DOCDIR) 2>/dev/null

# The ground truth for an installed tree: `make install` into a scratch
# prefix, then the installed `omn` run from there with target/ moved out of
# the way -- `omn list`, a launched `mem.rss` and `mem.heap.alloc` through the
# installed shim, each against a size the workload chose. scripts/release.sh
# `check` says what each check compares.
install-check:
	@prefix=$$(mktemp -d "$${TMPDIR:-/tmp}/omn-prefix.XXXXXX") \
	  && $(MAKE) --no-print-directory install PREFIX=$$prefix >/dev/null \
	  && scripts/release.sh check $$prefix; \
	  status=$$?; rm -rf "$$prefix"; exit $$status

# The release archive, its checksum, and the same checks on the extracted tree.
dist:
	@scripts/release.sh

dist-verify: dist
	@scripts/release.sh verify target/dist/$$(scripts/release.sh name).tar.gz

# The manual page, rendered.
man:
	@groff -man -Tascii man/omn.1 | less -R

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets

clean:
	cargo clean
