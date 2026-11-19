# ww_tape_summary

The summary trees of [ww_tape](../ww_tape) tracks. `EventTree` builds the item tree of an event track and
`SeriesSummaries` the statistics of a fixed-rate series (minimum, maximum, sums, clipped count, flags, power), as
summary blocks a `TapeWriter` places with `defer_block`. `Summary` reads the nodes of a summary block in place:
`event_node`, `channel_stats`, `series_column`, `series_node`; `EventNode` and `ChannelStats` merge, so a reader
builds coarser bins with the bits the writer stores one level up.

The head of a summary block (`SummaryHead`, `SummaryWriter`, `SeriesSummarySpec`) is in `ww_tape`, which checks
and indexes summary blocks without reading their nodes; this crate re-exports it, so everything about summaries is
imported from here.
