# walkie

Dial in by public key. Run on both machines. Share your public key with the other person, talk or chat freely.

Connections are encrypted end to end and direct using [iroh](https://github.com/n0-computer/iroh) and [RTP over QUIC](https://github.com/n0-computer/iroh-roq) for audio transport. Audio is encoded with Opus at 48 kHz / 20 ms frames.

## How it works

The node ID is an Ed25519 public key derived from a secret key generated on first run, no registration required. Iroh handles NAT traversal via hole punching and falls back to relay servers when a direct path isn't possible. Audio uses unreliable RTP datagrams over QUIC; chat and ping/pong use separate bidirectional streams.

## Usage

```
walkie [--input-device <NAME>] [--output-device <NAME>]
walkie --list-devices
```

On startup a connect screen is shown. Enter the peer's node ID to initiate a call, or press Enter with an empty field to listen for incoming connections.

Your identity key is persisted at `~/.config/walkie/secret.key`. Saved aliases are stored at `~/.config/walkie/aliases`. Logs are written to `~/.config/walkie/walkie.log`.

## Controls

| Key | Action |
|-----|--------|
| `Space` (hold) | Push to talk |
| `Tab` | Toggle between PTT and chat mode |
| `Enter` | Send chat message (chat mode) |
| `q` / `Ctrl+C` | Quit |

## Building

```
cargo build --release
cargo install --path=.
```
