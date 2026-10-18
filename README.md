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

In development. Linux 5.15+ is served; backend requirements and permissions vary.
Impossible measurements are refused with the reason, rather than approximated.

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

## Documentation

[Concepts and limits](REFERENCE.md) · [Quick tour](DEMO.md) ·
[Session guide](GUIDE.md) · [Catalog](CATALOG.md) ·
[Per-family guides](docs/how-to-use/) · [Current state](STATE.md)

## Development

```sh
make           # builds the engine and preload shim, restores capabilities
make check     # tests, Clippy and formatting
```

Use `make` rather than `cargo build`; see [build notes](REFERENCE.md#building-for-development)
and [Contributing](docs/contributing.md).

## License

[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
The eBPF code in `src/bpf/` is MIT or GPL-2.0; see [full terms](REFERENCE.md#license).
