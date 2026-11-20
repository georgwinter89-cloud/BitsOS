//! BIGDIR checker finding 3: the thread that asks the owners for memory (the reclaim thread) never ends up with
//! an owner's last reference, so an ext4 volume's last commit never runs there.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, ThreadId};
use std::time::Duration;
use vfs::meta_shrink::{register, run, Level, MetaShrink};

struct Owner {
    entered: Mutex<mpsc::Sender<()>>,
    dropped_by: Arc<Mutex<Option<ThreadId>>>,
}

impl MetaShrink for Owner {
    fn shrink_meta(&self, _: Level) -> u64 {
        let _ = self.entered.lock().unwrap().send(());
        // Long enough for the unmount to arrive while the call is in flight.
        thread::sleep(Duration::from_millis(200));
        7
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        *self.dropped_by.lock().unwrap() = Some(thread::current().id());
    }
}

#[test]
fn an_unmount_beside_a_call_drops_the_owner_itself() {
    let (tx, rx) = mpsc::channel();
    let dropped_by = Arc::new(Mutex::new(None));
    let owner = Arc::new(Owner { entered: Mutex::new(tx), dropped_by: dropped_by.clone() });
    let weak: Weak<Owner> = Arc::downgrade(&owner);
    let reg = register(weak);
    let reclaim = thread::spawn(|| run(Level::Hard));
    rx.recv_timeout(Duration::from_secs(30)).expect("the owner was never asked");
    // The unmount: the registration first (it waits for the call), then the mount's own reference.
    drop(reg);
    drop(owner);
    assert_eq!(*dropped_by.lock().unwrap(), Some(thread::current().id()), "the owner's last reference was not the unmount's");
    assert!(reclaim.join().unwrap() >= 7);
    // Off the list: nobody is asked any more.
    assert!(rx.try_recv().is_err());
    run(Level::All);
    assert!(rx.try_recv().is_err());
}
