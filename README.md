# opustagger

`opustagger` is a dependency-free Rust 1.97 command-line editor for all Vorbis
comments in an Ogg Opus file, including embedded album art. It edits the
`OpusTags` packet and repaginates the Ogg container; Opus audio packets are
copied byte for byte and are never decoded or re-encoded.

## Build

```sh
cargo build --release
```

The repository pins Rust 1.97.0 in `rust-toolchain.toml` and declares
`rust-version = "1.97"` in `Cargo.toml`.

The current implementation is tested on Unix-like systems, where in-place
replacement uses atomic `rename(2)` semantics.

Atomic replacement preserves the destination's permission bits, but not ACLs,
extended attributes, or other inode metadata. In-place edits of multiply linked
files are refused on Unix so hard-link associations are not silently broken;
use `--output` to choose a new directory entry explicitly.

## Use

```sh
# Browse every tag, including repeated fields and numbered covers
opustagger show song.opus

# Replace all TITLE fields, or append a repeated ARTIST field
opustagger set song.opus TITLE "New title"
opustagger add song.opus ARTIST "Guest artist"

# Edit or remove the exact tag index printed by `show`
opustagger edit song.opus 2 ALBUM "New album"
opustagger remove song.opus 5

# Edit the OpusTags vendor string
opustagger vendor song.opus "My encoder"

# Add, inspect, extract, and remove embedded FLAC picture blocks
opustagger cover-add song.opus cover.png "Front cover"
opustagger cover-list song.opus
opustagger cover-extract song.opus 0 extracted.png
opustagger cover-remove song.opus 0

# Dump length-framed audio packets for exact no-reencoding verification
opustagger audio-dump song.opus audio.packets

# Write a new file instead of replacing the input atomically
opustagger set song.opus DATE 2026 --output tagged.opus
```

Field names are matched case-insensitively where the Vorbis comment format
requires it. Values are UTF-8 and may contain `=`. PNG, JPEG, GIF, and WebP
covers are recognized. Their container/header structure is validated before the
file is changed; compressed image pixels are not decoded.

## Scope

The current version intentionally accepts one logical Opus stream per file. It
rejects multiplexed or chained Ogg files rather than risking damage to another
logical stream. CRCs and page sequence numbers are validated before editing and
regenerated after editing.

## Tests

```sh
cargo test
cargo build && sh tests/opustools-e2e.sh
```

The end-to-end check uses `opusenc`, `opusinfo`, and `opusdec`, then compares the
decoded WAV before and after tag and cover edits byte for byte. It exercises
every CLI command, repeated and indexed tags, UTF-8 values, large multi-page
metadata, multiple covers, separate and in-place output, and expected failures.
Every mutating step is immediately decoded and compared with the baseline; a
failure names the exact operation that first changed or broke the audio. The
length-framed Opus audio packets are also compared byte-for-byte after every
mutation, proving that packet data and packet boundaries did not change.

Custom audio and cover fixtures can be supplied independently:

```sh
# WAV and FLAC are encoded for the test; Opus/Ogg is used directly.
sh tests/opustools-e2e.sh --audio recording.flac
sh tests/opustools-e2e.sh --photo artwork.webp
sh tests/opustools-e2e.sh --audio recording.opus --photo artwork.jpg

# Headerless PCM defaults to 48 kHz, mono, signed 16-bit little-endian.
sh tests/opustools-e2e.sh --audio recording.pcm \
  --pcm-rate 44100 --pcm-channels 2 --pcm-bits 24 --pcm-endianness 0
```

Use `sh tests/opustools-e2e.sh --help` for environment-variable equivalents and
all raw PCM options. An omitted audio input uses `tests/sample.wav`, the CC0
[Bsumusictech bike-bell.wav from Wikimedia
Commons](https://commons.wikimedia.org/wiki/File:Bsumusictech_bike-bell.wav).
An omitted photo input uses `tests/cover.png`, the CC0 [Example png.png from
Wikimedia Commons](https://commons.wikimedia.org/wiki/File:Example_png.png).
Older `opusinfo` versions report compatibility warnings for valid WebP picture
blocks; the E2E test permits only those specific warnings and still performs all
decode and exact-packet checks.

## License

The opustagger code and documentation are licensed under the [MIT
License](LICENSE), copyright 2026 opustagger contributors.

Bundled test media are available under CC0 and retain their separate provenance
and licensing notices in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
