//! [producers::threads](example_drive::producers::threads) and its async twin on tokio tasks, each a producer
//! thread or task per track, read back after the close.
#![allow(clippy::expect_used, reason = "tests fail by panicking")]

use example_drive::Error;
use example_drive::producers::{self, Counts, LEVELS, OWNER, PINGS};
use ww_tape::{ChainOptions, MemPieces};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};

fn counts() -> Counts {
    Counts {
        pings_a: u64::from(PINGS),
        pings_b: u64::from(PINGS),
        levels: u64::try_from(LEVELS).expect("LEVELS fits u64"),
    }
}

#[test]
fn threads_write_every_item() {
    let got = producers::threads().expect("the threads write");
    assert_eq!(got, counts());
}

// --8<-- [start:tasks]
/// [producers::threads], the same calls as `_async`, on tokio tasks: two push pings, a third pushes levels, each
/// its own task. `smol::spawn` schedules the same futures the same way.
async fn tasks() -> Result<Counts, Error> {
    let tapes = MemPieces::new();
    let mux = TapeMux::create(
        tapes.clone(),
        OWNER,
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )?;
    let clock = mux.define_clock_async(producers::clock()).await?;
    let pings_a = mux
        .define_track_async(producers::pings_track(clock, "pings_a"))
        .await?;
    let pings_b = mux
        .define_track_async(producers::pings_track(clock, "pings_b"))
        .await?;
    let levels = mux
        .define_track_async(producers::levels_track(clock))
        .await?;

    let mut a = mux.events_async(pings_a, EventsOptions::default()).await?;
    let mut b = mux.events_async(pings_b, EventsOptions::default()).await?;
    let mut c = mux
        .series_async::<i16>(levels, SeriesOptions::default())
        .await?;

    let t_a = tokio::spawn(async move {
        let events: Vec<Event<'_>> = (0..PINGS)
            .map(|i| Event {
                start: i64::from(i),
                end: i64::from(i),
                ..Event::default()
            })
            .collect();
        a.push_all_async(&events).await?;
        a.finish_async().await
    });
    let t_b = tokio::spawn(async move {
        let events: Vec<Event<'_>> = (0..PINGS)
            .map(|i| Event {
                start: i64::from(i),
                end: i64::from(i),
                ..Event::default()
            })
            .collect();
        b.push_all_async(&events).await?;
        b.finish_async().await
    });
    let t_c = tokio::spawn(async move {
        c.push_async(&vec![0i16; LEVELS]).await?;
        c.finish_async().await
    });

    t_a.await
        .map_err(|_join| Error::Settings("a task panicked"))??;
    t_b.await
        .map_err(|_join| Error::Settings("a task panicked"))??;
    t_c.await
        .map_err(|_join| Error::Settings("a task panicked"))??;

    let pieces = producers::pieces_of(&tapes, &mux.close_async().await?)?;
    producers::read_back(&pieces, pings_a, pings_b, levels)
}
// --8<-- [end:tasks]

#[test]
fn tasks_write_every_item() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .build()
        .expect("the runtime builds");
    let got = rt.block_on(tasks()).expect("the tasks write");
    assert_eq!(got, counts());
}
