//! The accept queue a listener hands its connections through. One queue can be fed from several places at
//! once: the loopback (127.0.0.1) and every network interface a `0.0.0.0` listener listens on. Dropping the
//! listener closes the queue: connections still waiting in it are reset (Linux does the same for connections
//! nobody accepted), later ones are refused.

use crate::conn::Conn;
use crate::{NetError, TcpStream};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{SpinLock, WaitQueue};

pub struct AcceptQueue {
    q: SpinLock<VecDeque<Box<dyn Conn>>>,
    wq: WaitQueue,
    closed: AtomicBool,
}

impl AcceptQueue {
    pub fn new() -> Self {
        AcceptQueue { q: SpinLock::named("net.accept", VecDeque::new()), wq: WaitQueue::new(), closed: AtomicBool::new(false) }
    }

    /// Queues an established connection. A closed queue resets it and says so.
    pub fn push(&self, s: Box<dyn Conn>) -> Result<(), NetError> {
        if self.is_closed() {
            s.shutdown();
            return Err(NetError::ConnectionRefused);
        }
        self.q.lock().push_back(s);
        self.wq.wake_all();
        crate::sock::inet::notify();
        Ok(())
    }

    /// The oldest waiting connection, without waiting (program sockets wait on their own terms).
    pub fn try_pop(&self) -> Option<Box<dyn Conn>> {
        self.q.lock().pop_front()
    }

    /// Connections waiting to be accepted.
    pub fn len(&self) -> usize {
        self.q.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Takes the oldest waiting connection; waits up to `timeout_ns` (None: until one comes or the queue closes).
    pub fn accept(&self, timeout_ns: Option<u64>) -> Result<Box<dyn TcpStream>, NetError> {
        let ready = || !self.q.lock().is_empty() || self.is_closed();
        let ok = match timeout_ns {
            Some(t) => self.wq.wait_until_timeout(ready, t),
            None => {
                self.wq.wait_until(ready);
                true
            }
        };
        if let Some(s) = self.q.lock().pop_front() {
            return Ok(s as Box<dyn TcpStream>);
        }
        if self.is_closed() {
            return Err(NetError::Closed);
        }
        Err(if ok { NetError::WouldBlock } else { NetError::TimedOut })
    }

    /// Refuses from now on and resets every connection still waiting; wakes a blocked `accept`.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let waiting: VecDeque<Box<dyn Conn>> = core::mem::take(&mut *self.q.lock());
        for s in waiting {
            s.shutdown();
        }
        self.wq.wake_all();
        crate::sock::inet::notify();
    }
}

impl Default for AcceptQueue {
    fn default() -> Self {
        Self::new()
    }
}
