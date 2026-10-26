//! Rust std threads on V2 (AB-4): std::thread on musl's pthread_create (clone with the thread flags, a stack
//! with a guard page, a sigaltstack per thread for the overflow handler), JoinHandle results, a panic caught by
//! join (the panic message goes to stderr), Mutex/RwLock/Condvar/Barrier/Once, mpsc channels, scoped
//! threads, park/unpark, thread names and ids. Only the main thread prints, after the joins.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Condvar, Mutex, Once, RwLock};
use std::thread;
use std::time::Duration;

static ONCE: Once = Once::new();
static ONCE_RUNS: AtomicUsize = AtomicUsize::new(0);

fn main() {
    // Rust's own panic message names the thread's id; a fixed line keeps the transcripts comparable.
    std::panic::set_hook(Box::new(|_| eprintln!("panic hook: a thread panicked")));
    let total = Arc::new(Mutex::new(0u64));
    let handles: Vec<_> = (0..8u64)
        .map(|i| {
            let t = total.clone();
            thread::spawn(move || {
                for _ in 0..1000 {
                    *t.lock().unwrap() += 1;
                }
                ONCE.call_once(|| {
                    ONCE_RUNS.fetch_add(1, Ordering::SeqCst);
                });
                i * 10
            })
        })
        .collect();
    let results: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    println!("mutex total = {}, results = {results}, once ran {} time(s)", *total.lock().unwrap(), ONCE_RUNS.load(Ordering::SeqCst));

    let named = thread::Builder::new().name("worker-a".into()).spawn(|| thread::current().name().map(String::from)).unwrap();
    println!("thread name: {:?}", named.join().unwrap());
    let main_id = thread::current().id();
    let other_id = thread::spawn(|| thread::current().id()).join().unwrap();
    println!("thread ids differ: {}", main_id != other_id);

    let r = thread::spawn(|| {
        if std::env::args().count() < 100 {
            panic!("a deliberate panic in a thread");
        }
        5
    })
    .join();
    println!("panicking thread joined as error: {}", r.is_err());

    let (tx, rx) = mpsc::channel();
    for i in 0..4 {
        let tx = tx.clone();
        thread::spawn(move || {
            for k in 0..25 {
                tx.send(i * 100 + k).unwrap();
            }
        });
    }
    drop(tx);
    let got: Vec<i32> = rx.iter().collect();
    println!("channel received {} messages, sum {}", got.len(), got.iter().sum::<i32>());

    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let p2 = pair.clone();
    let waiter = thread::spawn(move || {
        let (m, c) = &*p2;
        let mut ready = m.lock().unwrap();
        while !*ready {
            ready = c.wait(ready).unwrap();
        }
        "woken"
    });
    thread::sleep(Duration::from_millis(10));
    {
        let (m, c) = &*pair;
        *m.lock().unwrap() = true;
        c.notify_all();
    }
    println!("condvar: {}", waiter.join().unwrap());

    let barrier = Arc::new(Barrier::new(5));
    let leaders = Arc::new(AtomicUsize::new(0));
    let bs: Vec<_> = (0..5)
        .map(|_| {
            let b = barrier.clone();
            let l = leaders.clone();
            thread::spawn(move || {
                if b.wait().is_leader() {
                    l.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for b in bs {
        b.join().unwrap();
    }
    println!("barrier leaders: {}", leaders.load(Ordering::SeqCst));

    let data = RwLock::new(vec![1, 2, 3]);
    let mut sums = [0; 4];
    thread::scope(|s| {
        for (i, out) in sums.iter_mut().enumerate() {
            let d = &data;
            s.spawn(move || {
                *out = d.read().unwrap().iter().sum::<i32>() * (i as i32 + 1);
            });
        }
    });
    data.write().unwrap().push(4);
    println!("scoped sums: {sums:?}, data now {:?}", data.read().unwrap());

    let parked = thread::spawn(|| {
        thread::park();
        "unparked"
    });
    thread::sleep(Duration::from_millis(10));
    parked.thread().unpark();
    println!("park: {}", parked.join().unwrap());

    let deep = thread::Builder::new().stack_size(4 << 20).spawn(|| (0..100_000u64).sum::<u64>()).unwrap();
    println!("thread with a 4 MiB stack: {}", deep.join().unwrap());
    println!("rust threads: done");
}
