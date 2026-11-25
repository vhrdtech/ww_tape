---
level: nomicon
---

# Nomicon

The Nomicon explains how the format works inside, as the Rustonomicon does for Rust. Read it to write a reader in
another language, or to know why a piece holds what it holds. After it, you know what a new piece carries over from
the pieces before and how a track's hash is folded from the index. You also know which lessons of tape drives and
flash shaped the layout.

The part assumes the Basic and Advanced parts. Every page in this part has the Nomicon badge.

## Chapters

| Chapter | What it explains |
|---|---|
| [The definitions a new piece carries](../carried_definitions.md) | Why a piece repeats the definitions of earlier pieces, and what that costs. |
| [When carried definitions don't fit](../carried_room.md) | The room a piece's index leaves them, what a cut leaves out past it, and which calls the writer refuses. |
| [Track owners and hashes](../track_hashes.md) | Tracks of several sources in one tape, and a track's hash over a range. |
| [Lessons from tape and flash](../lessons.md) | What tape drives and flash stores taught the format. |

The [Format reference](../format.md) in the Reference holds every block byte by byte.
