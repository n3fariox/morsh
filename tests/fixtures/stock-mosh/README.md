# Stock mosh 1.4.0 golden exchange

`capture.json` was captured on Linux on 2026-11-05 from installed stock
`mosh-client` and `mosh-server` 1.4.0 (Debian build reports `mosh-1.4.0-dirty`).
Both endpoints are stock binaries; morsh is not involved in generation.
`scripts/capture-stock.py` relays UDP on loopback, records the ephemeral test
key and exact datagrams, and independently authenticates/decrypts them with
PyCryptodome AES-128-OCB. It decompresses each single-fragment instruction with
Python zlib. Hex preserves timestamps, random chaff, nonce, direction, tag,
and protobuf field presence exactly. The key is disposable test data.

The command prints STOCK_FIXTURE_READY, reads `fixture-input`, prints
REPLY:fixture-input, then exits. The capture includes resize, empty ack,
HostBytes, Keystroke, EchoAck, throwaway numbers, and shutdown (`UINT64_MAX`).
Tests compare the exact encryption and transport protobuf and replay both
receiver histories, including diffs based on older states.

Regenerate deliberately (new timestamps, keys and chaff will change the file):

```sh
python3 -m venv /tmp/morsh-fixtures
/tmp/morsh-fixtures/bin/pip install pycryptodome==3.23.0
/tmp/morsh-fixtures/bin/python scripts/capture-stock.py
cargo test -p morsh-statesync --test stock_vectors
```

Stock binaries and Python/PyCryptodome are only needed to regenerate; fixture
replay is offline Rust. Capture requires Unix PTYs and a C.UTF-8 locale.
Regeneration rejects multi-fragment captures rather than silently decoding a
partial message. Fragmentation itself is covered with fixed incompressible
payloads; this fixture does not claim coverage of all stock versions or traffic.
