# ww_tape_summary

The summary trees of [ww_tape](../ww_tape) tracks. Both builders below produce summary blocks that a `TapeWriter`
places with `defer_block`:

- `EventTree` builds the item tree of an event track.
- `SeriesSummaries` builds the statistics of a fixed-rate series: minimum, maximum, sums, clipped count, flags, power.

`Summary` reads the nodes of a summary block in place: `event_node`, `channel_stats`, `series_column`, `series_node`.

The types `EventNode` and `ChannelStats` merge. A reader therefore builds coarser bins with the bits the writer stores
one level up.

The types of a summary block's head (`SummaryHead`, `SummaryWriter`, `SeriesSummarySpec`) live in `ww_tape` itself.
That crate checks and indexes summary blocks without reading their nodes.

This crate re-exports the head. An application imports everything about summaries from here.
