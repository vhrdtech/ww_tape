//! The async side of the mux and the followers: each future runs on tokio's two schedulers and on smol, without a
//! timer or an I/O driver, and is woken from another thread: the writing thread, or a follower's own (WRITE-14).

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Waker};
use std::thread;
use std::time::Duration;

use ww_tape::{ItemNum, Layout, Tapes, TrackNum};
use ww_tape_fs::follow::{ChainFollower, FollowOptions, TrackItem};
use ww_tape_mux::{
    ChunkSender, EventsOptions, MuxError, MuxOptions, Pressure, Progress, Sent, TapeMux,
};

use crate::definitions::base;
use crate::mux::{Rigged, contiguous, items_of, pieces, rigged_mux, until, whole};
use crate::pieces::{chunk, chunk_id, column, scratch};

/// Run the future `make` gives on tokio's current-thread scheduler, spawned on its multi-thread one (so it must be
/// `Send`), and on smol. None of them has a timer or an I/O driver: only the mux's threads wake the future.
fn on_every_runtime<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(make());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    rt.block_on(async { tokio::spawn(make()).await.unwrap() });
    smol::block_on(make());
}

/// Poll `f` once with a waker that does nothing: whether it is still pending.
fn pends<F: Future>(f: F) -> bool {
    let f = pin!(f);
    f.poll(&mut Context::from_waker(Waker::noop())).is_pending()
}

/// Track `t`'s chunk `k` ([chunk]), sent as a future.
async fn send_chunk<T: Tapes>(
    s: &ChunkSender<T>,
    t: u64,
    k: u64,
) -> Result<Sent, MuxError<T::Error>> {
    let (head, starts) = chunk(t, k);
    let spec = column(&starts)[0].0;
    s.send_async(head, vec![(spec, starts)]).await
}

/// Tasks that send while the sink stalls wait for room, and the writing thread wakes them as it places units; every
/// unit lands, and the flush, the definitions and the close are futures too.
#[test]
fn tasks_waiting_for_room_are_woken_by_the_writing_thread() {
    const CHUNKS: u64 = 60;
    on_every_runtime(|| async {
        let rigged = Rigged::new();
        let mux = Arc::new(rigged_mux(&rigged, 4 << 10));
        let mut senders = Vec::new();
        for t in 0..4u32 {
            senders.push(mux.chunks_async(TrackNum::new(t + 1)).await.unwrap());
        }
        mux.flush_async().await.unwrap();
        rigged.rig.set(false);
        // opens the sink once the task waits: the task can't finish its sends before
        let opener = {
            let mux = mux.clone();
            let rig = rigged.rig.clone();
            thread::spawn(move || {
                until(|| mux.stats().waiting > 0);
                rig.set(true);
            })
        };
        for k in 0..CHUNKS {
            for (t, s) in senders.iter().enumerate() {
                assert_eq!(send_chunk(s, t as u64, k).await.unwrap(), Sent::Queued);
            }
        }
        for s in &senders {
            s.flush_async().await.unwrap();
        }
        opener.join().unwrap();
        let stats = mux.stats();
        assert!(stats.waits > 0, "{stats:?}");
        assert_eq!((stats.drops, stats.refused, stats.lost), (0, 0, 0));
        assert_eq!(mux.definitions_async().await.unwrap().tracks().count(), 10);
        mux.cut_async().await.unwrap();
        mux.sync_async().await.unwrap();
        drop(senders);
        let Ok(mux) = Arc::try_unwrap(mux) else {
            panic!("the opener holds the mux no more")
        };
        mux.close_async().await.unwrap();
        let pieces = pieces(&rigged.mem);
        assert_eq!(pieces.len(), 2);
        whole(&pieces);
        for t in 0..4 {
            assert_eq!(contiguous(&items_of(&pieces, t)), CHUNKS * 50, "track {t}");
        }
    });
}

/// A push dropped while it waits for room keeps its chunk in the handle: the next push sends it first, so the
/// track has no gap and its chunks stay in order.
#[test]
fn a_push_dropped_while_it_waits_loses_no_item() {
    const ITEMS: u64 = 2_000;
    on_every_runtime(|| async {
        let rigged = Rigged::new();
        let mux = rigged_mux(&rigged, 1 << 10);
        let options = EventsOptions {
            chunk_items: 50,
            latency: None,
            summaries: false,
            ..EventsOptions::default()
        };
        let mut h = mux.events_async(TrackNum::new(1), options).await.unwrap();
        mux.flush_async().await.unwrap();
        rigged.rig.set(false);
        let mut i = 0u64;
        // pushes fill the queue; the first that waits for room is dropped
        loop {
            let pending = pends(h.push_async(i as i64 * 10, i as i64 * 10, 0, &i.to_le_bytes()));
            i += 1;
            if pending {
                break;
            }
        }
        assert_eq!(h.next_item(), ItemNum::new(i));
        assert_eq!(mux.stats().waiting, 0, "the dropped push left the line");
        rigged.rig.set(true);
        // --8<-- [start:push_async]
        for i in i..ITEMS {
            h.push_async(i as i64 * 10, i as i64 * 10, 0, &i.to_le_bytes())
                .await
                .unwrap();
        }
        h.finish_async().await.unwrap();
        mux.close_async().await.unwrap();
        // --8<-- [end:push_async]
        let pieces = pieces(&rigged.mem);
        whole(&pieces);
        assert_eq!(contiguous(&items_of(&pieces, 0)), ITEMS);
    });
}

/// A task waiting on the progress watch is woken by the writing thread when a chunk lands, and once more when the
/// mux ends.
#[test]
fn the_progress_watch_wakes_a_task() {
    on_every_runtime(|| async {
        let rigged = Rigged::new();
        let mux = rigged_mux(&rigged, 1 << 20);
        let watch = mux.watch();
        let seen = watch.get();
        assert!(seen.tracks.is_empty());
        let sender = mux.chunks(TrackNum::new(1)).unwrap();
        // sent from another thread while the task waits
        let producer = thread::spawn(move || {
            let (head, starts) = chunk(0, 0);
            let spec = column(&starts)[0].0;
            sender.send(head, vec![(spec, starts)]).unwrap();
        });
        let mut p: Progress = watch.wait_past_async(seen.version).await;
        while !p.tracks.contains_key(&TrackNum::new(1)) {
            p = watch.wait_past_async(p.version).await;
        }
        assert_eq!(p.tracks[&TrackNum::new(1)].next_item, ItemNum::new(50));
        producer.join().unwrap();
        mux.close_async().await.unwrap();
        assert!(watch.wait_past_async(u64::MAX).await.ended);
    });
}

/// A task follows a chain in files while a thread writes it: the follower's own thread wakes it on each change
/// (inotify on Linux; the poll is an hour there, so only a change wakes it), across cuts, until the close.
#[test]
fn a_task_follows_a_chain_a_thread_writes() {
    const ROUNDS: u64 = 12;
    let runs = std::sync::atomic::AtomicU64::new(0);
    on_every_runtime(|| {
        let run = runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        async move {
            let dir = scratch(&format!("mux_async_follow_{run}"));
            let poll = if cfg!(target_os = "linux") {
                Duration::from_secs(3600)
            } else {
                Duration::from_millis(20)
            };
            let options = FollowOptions {
                poll,
                ..FollowOptions::default()
            };
            let mut items = ChainFollower::new(&dir, options)
                .unwrap()
                .track(chunk_id(0));
            // a wait dropped before anything is written takes nothing
            assert!(pends(items.wait_next_async()));
            let writer = {
                let dir = dir.clone();
                thread::spawn(move || {
                    let mux = ww_tape_fs::mux::create(
                        &dir,
                        crate::follow::OWNER,
                        0,
                        &[],
                        crate::follow::options(4),
                        MuxOptions::default(),
                    )
                    .unwrap();
                    let senders = crate::follow::define(&mux);
                    for k in 0..ROUNDS {
                        for (t, s) in senders.iter().enumerate() {
                            crate::follow::send(s, t as u64, k);
                        }
                        mux.flush().unwrap();
                    }
                    drop(senders);
                    mux.close().unwrap();
                })
            };
            let mut next = 0;
            while let Some(item) = items.wait_next_async().await.unwrap() {
                let TrackItem::Event { item, .. } = item else {
                    panic!("an events track gives events: {item:?}")
                };
                assert_eq!(item, ItemNum::new(next));
                next += 1;
            }
            assert_eq!(next, ROUNDS * 50);
            assert!(items.chain().is_closed());
            assert!(
                items.chain().piece().unwrap() > 0,
                "it followed across cuts"
            );
            writer.join().unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
        }
    });
}

/// The futures of the mux, its handles and the followers can be sent to another thread: a multi-thread runtime
/// takes them.
#[test]
fn the_futures_are_send() {
    fn is_send<F: Future + Send>(_: &F) {}
    let rigged = Rigged::new();
    let mux: TapeMux<Rigged> = rigged_mux(&rigged, 1 << 20);
    let mut events = mux
        .events(TrackNum::new(1), EventsOptions::default())
        .unwrap()
        .pressure(Pressure::Wait);
    let defs = base(Layout::UNALIGNED);
    is_send(&mux.define_all_async(&defs[..0]));
    is_send(&mux.flush_async());
    is_send(&events.push_async(0, 0, 0, &[]));
    is_send(&mux.watch().wait_past_async(0));
    let dir = scratch("mux_async_send");
    let mut f = ChainFollower::new(&dir, FollowOptions::default()).unwrap();
    is_send(&f.wait_next_async());
    let mut t = f.track(chunk_id(0));
    is_send(&t.wait_next_async());
    std::fs::remove_dir_all(&dir).unwrap();
}
