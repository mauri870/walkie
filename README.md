# walkie

Dial in by public key. Run on both machines. Share your node ID with the other person, talk freely.

Built on [iroh](https://github.com/n0-computer/iroh) for direct encrypted connections and [RTP over QUIC](https://github.com/n0-computer/iroh-roq) for audio transport. Audio is encoded with Opus at 48 kHz / 20 ms frames.

## Usage

```
walkie [--input-device <NAME>] [--output-device <NAME>]
walkie --list-devices
```

Hold Space to talk. The connection uses iroh's built-in discovery, no manual IP/port config needed.

Your identity key is persisted at `~/.config/walkie/secret.key`.

## Building

```
cargo build --release
cargo install --path=.
```
