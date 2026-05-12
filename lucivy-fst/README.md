# lucivy-fst

Finite state transducer library for lucivy. Provides fast ordered sets and maps using FSTs, with multi-output support for the SFX engine.

Fork of [BurntSushi/fst](https://github.com/BurntSushi/fst) extended with:

- **Multi-output values** — `OutputTable` for entries with multiple parent tokens (shared suffixes)
- **Levenshtein prefix DFA** — fuzzy walk on suffix partitions for cross-token search
- **Partition-aware construction** — SI=0 (token start) and SI>0 (substring) entries in a single FST

Used internally by lucivy's SFX engine to store all suffixes of all indexed tokens. Not typically used directly — use `lucivy-core` instead.

## How it fits in lucivy

Each indexed segment has a `.sfx` file containing a lucivy-fst `Map`. The FST keys are `[partition_byte][suffix_bytes]` and values encode `(raw_ordinal, si, token_len)` packed into 64 bits. For shared suffixes (multiple parent tokens), values point into an `OutputTable`.

The SFX engine walks this FST byte-by-byte during search (`falling_walk`), detecting split points where a query crosses token boundaries.

## Features

- `levenshtein` — enables Levenshtein automaton for fuzzy search (requires `utf8-ranges`)

## Heritage

Based on [BurntSushi/fst](https://github.com/BurntSushi/fst). See the original [blog post](https://blog.burntsushi.net/transducers/) for background on FSTs.

## License

MIT
