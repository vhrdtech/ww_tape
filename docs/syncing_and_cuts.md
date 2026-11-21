# Syncing, cuts and relays

The material of this page is now in the chapters of "Writing". Each section below names where it went.

## Syncing: what a crash can lose

[Files and syncing](files.md#how-often-to-sync) compares the sync policies on a real crash.

### The writer's clock

[Files and syncing](files.md#the-writers-clock) explains the writer's clock.

### A failed write ends the file

[Files and syncing](files.md#a-failed-write-ends-the-file) explains why the writer never retries.

## Checkpoints

[Anatomy of a piece](anatomy.md#checkpoints-while-the-piece-grows) explains checkpoints, and [Files and
syncing](files.md#a-quiet-input) checkpoints by time.

## Cuts: ending one piece and starting the next

[Pieces and chains](pieces.md#what-a-cut-writes) shows what a cut writes.

### The definitions a new piece carries

[The definitions a new piece carries](carried_definitions.md) says which definitions a piece carries.

### Cutting by policy

[Pieces and chains](pieces.md#when-to-cut) explains the cut policy.

### Limits that a cut does not lift

[Format reference](format.md#tape-pieces) lists the limits on tracks per piece.

### One tape per piece or one for all

[Pieces and chains](pieces.md#writing-a-chain-into-one-file) explains a chain in one file.

### Nothing waits for a cut

[Pieces and chains](pieces.md#nothing-waits-for-a-cut) explains why.

## Closing

[Files and syncing](files.md#the-close) follows the close step by step, and [Writing a tape](writing.md#closing) says
what it returns.

## Relays

[Live feeds and relays](live_feeds.md#relays) explains relays.

### Adopting blocks on raw media

[Live feeds and relays](live_feeds.md#adopting-blocks-on-raw-media) explains adopting blocks.

## Same input, same bytes

[Anatomy of a piece](anatomy.md#same-input-same-bytes) explains the property.
