//! The tables of the case pages that the example's code gives, not a bench: what each track of the protocol analyser
//! takes.

use crate::OrDebug;
use example_ethernet::Link;

/// What 10 ms of the link at its line rate take, track by track: items, bytes per item stored and as received.
pub fn ethernet_sizes() -> anyhow::Result<String> {
    let s = Link::full().micros(10_000).sizes().or_debug()?;
    let (frames, frame_bytes, _) = s.frames();
    let mut out = String::from(
        "| Track | Items | Bytes per item | Bytes per item in the chain | Chunks | Summary bytes |\n\
         |---|---:|---:|---:|---:|---:|\n",
    );
    let raw = [
        frame_bytes as f64 / frames.max(1) as f64,
        example_ethernet::IPV4_BYTES as f64,
        example_ethernet::L4_BYTES as f64,
    ];
    for (t, (name, raw)) in ["frames", "ipv4", "l4"].into_iter().zip(raw).enumerate() {
        let (items, b) = s.track(t);
        let stored = (b.chunk_bytes + b.summary_bytes) as f64 / items.max(1) as f64;
        out.push_str(&format!(
            "| `{name}` | {items} | {raw:.1} | {stored:.1} | {} | {} |\n",
            b.chunks, b.summary_bytes
        ));
    }
    out.push_str(
        "\n10 ms of both directions at the line rate, two receive queues a port, one chain. Drawn by \
         `ww_tape_figures` from `example_ethernet`.\n",
    );
    Ok(out)
}
