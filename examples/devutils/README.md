# Omniscient with the other devutils tools

| File | What |
|---|---|
| `atfq-read-only.yml`, `xpac-plan-read-only.yml`, `envprobe-read-only.yml` | each tool's README promise, measured: no file changed, no TCP connection, no program but its own. `scripts/devutils/promises.py` runs and judges them |
| `xpac-pacman.toml` | the manifest the xpac session plans, for an Arch machine |
| `mcp.json` | one MCP client configuration with both servers: an agent asks atfq what a setting or a number means (`lookup`, `run_probe`), and Omniscient what it measured (`omn-mcp`). When a source is refused over a kernel setting, omn's hint names atfq's `kernel.tracing` card, which the agent can then look up |

The cross-tool chain that runs every tool inside distroproof's guests is
`scripts/distroproof/devutils/` and `distroproof-devutils.toml`.
