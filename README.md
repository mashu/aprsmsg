# aprsmsg

[![CI](https://github.com/mashu/aprsmsg/actions/workflows/ci.yml/badge.svg)](https://github.com/mashu/aprsmsg/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/mashu/aprsmsg/branch/main/graph/badge.svg)](https://codecov.io/gh/mashu/aprsmsg)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Interactive APRS messaging over Direwolf (KISS) or APRS-IS. Also runs in the browser ([live demo](https://mashu.github.io/aprsmsg/)).

## CLI

```bash
# radio via Direwolf
cargo run -- --call SA0KAM-1
msg SM0YOS-1 hello from the radio

# internet (no radio)
cargo run -- --call SA0KAM-1 --aprs-is
msg EMAIL-2 friend@example.com Hello from APRS-IS
```

Prebuilt binaries for Linux, macOS, and Windows are on the [Releases](https://github.com/mashu/aprsmsg/releases) page (and linked from the web app).

## Browser

| Mode | WebSocket URL | Bridge? |
| --- | --- | --- |
| APRS-IS | `wss://ametx.com:8888` | no |
| Direwolf | `ws://127.0.0.1:8765` | yes — download `aprsmsg-bridge` for your OS |

**Direwolf setup:** start Direwolf with KISS TCP on `127.0.0.1:8001` first, then the bridge, then Connect in the browser. The bridge only proxies WebSocket → Direwolf; `Connection refused` on `:8001` means Direwolf is not listening yet.

APRS-IS from HTTPS pages needs **WSS** (browsers cannot open plain TCP `:14580`). Host spelling is **ametx.com**, not amtex. The local bridge is only for Direwolf.
