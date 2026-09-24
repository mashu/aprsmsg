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

## Browser

APRS-IS from HTTPS pages needs **WSS** (browsers cannot open plain TCP `:14580`). Default: `wss://ametx.com:8888`.

For Direwolf, run a local bridge, then connect the web UI to `ws://127.0.0.1:8765`:

```bash
cargo run --bin aprsmsg-bridge
```
