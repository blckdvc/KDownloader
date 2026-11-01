# Pulsar's copy of rqbit

Copied from `github.com/lingdiansr/rqbit`, branch `feat/mse-crypto-primitives`, revision
`9b7877cbd884f77c8923e` (rqbit PR #633, MSE encryption), without `desktop/` and tooling.

Pulsar's patches, to reapply on any update:

- `tracker_comms`: `TorrentStatsProvider::on_swarm_counts` reports each tracker's seeders and leechers.
- `librqbit`: `Session::tracker_swarm` exposes them, keeping the best count across trackers. All-zero answers are ignored: some trackers send them between real counts.
- `librqbit`: `StatsSnapshot::connected_seeds` counts live peers holding every piece.

- `tracker_comms`: log spans show the tracker host only, as private trackers keep a passkey in the URL.

`desktop/src-tauri` is removed from the workspace members in `Cargo.toml`.
