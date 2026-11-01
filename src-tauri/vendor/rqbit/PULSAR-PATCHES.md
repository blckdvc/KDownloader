# Pulsar's copy of rqbit

Copied from `github.com/lingdiansr/rqbit`, branch `feat/mse-crypto-primitives`, revision
`9b7877cbd884f77c8923e` (rqbit PR #633, MSE encryption), without `desktop/` and tooling.

Pulsar's patches, to reapply on any update:

- `tracker_comms`: `TorrentStatsProvider::on_swarm_counts` reports each tracker's seeders and leechers.
- `librqbit`: `Session::tracker_swarm` exposes them, keeping the best count across trackers.
- `librqbit`: `StatsSnapshot::connected_seeds` counts live peers holding every piece.

`desktop/src-tauri` is removed from the workspace members in `Cargo.toml`.
