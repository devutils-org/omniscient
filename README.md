<p align="center"><code>devutils / OBSERVE</code></p>

<h1 align="center">Omniscient</h1>

<p align="center">Linux observability, straight from the kernel.</p>

<p align="center">
  <a href="https://devutils-org.github.io/tools/omn/">Website</a> ·
  <a href="#documentation">Documentation</a> ·
  <a href="https://github.com/devutils-org">devutils</a>
</p>

## Overview

- Measure a program or a machine through kernel APIs, without wrapping other tools.
- Follow live measurements, compose sessions and correlate events with explicit evidence.
- See collection loss, backend selection and measurement cost beside the results.

In development. Installed releases were tested on 17 distro images and 5
variants, spanning Linux 4.15–7.2, with availability probed per source.
Backend requirements and permissions vary; measurements that cannot be served
are refused with the reason.

## Install

From a source checkout with Rust 1.82+:

```sh
make install PREFIX=~/.local
```

Keep `~/.local/bin` on your `PATH`. eBPF sources need `CAP_BPF` and `CAP_PERFMON`;
without them, fewer sources are available. See [installation](docs/install.md)
for capability setup and release archives.

## Use

```sh
omn list                                   # what this machine can serve
omn list mem                               # memory sources and requirements
omn mem.rss --emit timeline --mode poll@100ms -- ./build.sh
omn -f examples/allocation-cost.yml         # a composed measurement session
```

Replace `./build.sh` with your program. Sessions also accept JSON and TOML;
`omn formats` lists output formats and their supported measurements.

## Portability, tested with distroproof

The latest [distroproof campaign](docs/distroproof.md) found 27 defects that
one development machine had not shown: 13 in the tool and 14 in its tests.
After the fixes, none of the source runs advertised as usable refused at
start, and every refusal reason agreed with envprobe on all 33 targets: 22
x86_64 distributions and variants (NixOS among them), 3 arm64 images and 8
rootless containers.
This checks availability and refusal reasons; reference comparisons against
distro-packaged tools on six distributions still have open differences.

Release archives target **glibc 2.17+**, with a separate **musl** archive for
Alpine, for x86_64 and aarch64. The static musl build cannot load NVIDIA's libraries and refuses
those sources explicitly. See [installation](docs/install.md).

## Documentation

[Concepts and limits](REFERENCE.md) · [Quick tour](docs/DEMO.md) ·
[Session guide](docs/GUIDE.md) · [Catalog](docs/CATALOG.md) ·
[Per-family guides](docs/how-to-use/) · [Design decisions](docs/DECISIONS.md) ·
[Roadmap](docs/ROADMAP.md) · [Current state](docs/dev/STATE.md)

## Development

```sh
make           # builds the engine and preload shim, restores capabilities
make check     # tests, Clippy and formatting
```

Use `make` rather than `cargo build`; see [build notes](REFERENCE.md#building-for-development)
and [Contributing](docs/contributing.md).

Omniscient is developed with coding agents, in waves of parallel work. Their
working notes are kept rather than cleaned away, because the other documents
cite them: [`docs/dev/`](docs/dev/) holds the measured state of the tool
(`STATE.md`), the current wave's brief (`BRIEF.md`) and every earlier one
(`briefs/`), and [`CLAUDE.md`](CLAUDE.md) is what an agent reads first.

## License

[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
The eBPF code in `src/bpf/` is MIT or GPL-2.0; see [full terms](REFERENCE.md#license).
