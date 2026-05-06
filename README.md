# tidepoolsync

Sync Tidepool pump settings and history into Nightscout.

Supported actions:

- dump the latest Tidepool records
- post `pumpSettings` as a Nightscout profile
- sync Tidepool glucose, bolus, basal, food, and device-event records

## Config

Roll your own `tidepoolsync.config.json` based on the `tidepoolsync.config.example.json` included. The watermark defaults to `$XDG_STATE_HOME/tidepoolsync/state.json`.

## Usage

```bash
cargo run --release -- \
  --config tidepoolsync.config.json \
  --dump-settings pump-settings.json
```

Preview profile conversion without posting:

```bash
cargo run --release -- \
  --config tidepoolsync.config.json \
  --sync-profile \
  --dry-run
```

Sync the last 7 days when no watermark exists:

```bash
cargo run --release -- \
  --config tidepoolsync.config.json \
  --sync-data \
  --backfill-days 7
```

Run data sync every 30 minutes:

```bash
cargo run --release -- \
  --config tidepoolsync.config.json \
  --sync-data \
  --daemon \
  --poll-interval-secs 1800
```
