use futures_core::future::{FusedFuture, Future};
use futures_core::task::{Context, Poll, Waker};
use slab::Slab;
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use std::{fmt, mem};

/// A futures-aware mutex.
pub struct Mutex<T: ?Sized> {
    state: AtomicUsize,
    waiters: StdMutex<Slab<Waiter>>,
    value: UnsafeCell<T>,
}

impl<T: ?Sized> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.load(Ordering::SeqCst);
        f.debug_struct("Mutex")
            .field("is_locked", &((state & IS_LOCKED) != 0))
            .field("has_waiters", &((state & HAS_WAITERS) != 0))
            .finish()
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(t: T) -> Self {
        Self::new(t)
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Mutex<T> {
        Mutex::new(Default::default())
    }
}

enum Waiter {
    Waiting(Waker),
    Woken,
}

impl Waiter {
    fn register(&mut self, waker: &Waker) {
        println!("Waiter::register: Registering...");
        match self {
            Waiter::Waiting(w) if waker.will_wake(w) => {
                println!("Waiter::register: will_wake...");
            }
            _ => {
                println!("Waiter::register: waker.clone...");
                *self = Waiter::Waiting(waker.clone());
                println!("Waiter::register: waker.clone...");
            }
        }
    }

    fn wake(&mut self) {
        println!("wake: start...");
        match mem::replace(self, Waiter::Woken) {
            Waiter::Waiting(waker) => {
                println!("wake: waking...");
                waker.wake();
                println!("wake: woke...")
            }
            Waiter::Woken => println!("wake: already woken..."),
        }
        println!("wake: done.");
    }
}

#[allow(clippy::identity_op)] // https://github.com/rust-lang/rust-clippy/issues/3445
const IS_LOCKED: usize = 1 << 0;
const HAS_WAITERS: usize = 1 << 1;

impl<T> Mutex<T> {
    /// Creates a new futures-aware mutex.
    pub fn new(t: T) -> Mutex<T> {
        println!("== Mutex::new");
        let m = Mutex {
            state: AtomicUsize::new(0),
            waiters: StdMutex::new(Slab::new()),
            value: UnsafeCell::new(t),
        };
        println!("-- Mutex::new");
        m
    }

    /// Consumes this mutex, returning the underlying data.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures::lock::Mutex;
    ///
    /// let mutex = Mutex::new(0);
    /// assert_eq!(mutex.into_inner(), 0);
    /// ```
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Attempt to acquire the lock immediately.
    ///
    /// If the lock is currently held, this will return `None`.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        println!("== Mutex::try_lock");
        let old_state = self.state.fetch_or(IS_LOCKED, Ordering::Acquire);
        let g = if (old_state & IS_LOCKED) == 0 {
            Some(MutexGuard { mutex: self })
        } else {
            None
        };

        println!("-- Mutex::try_lock");
        g
    }

    /// Acquire the lock asynchronously.
    ///
    /// This method returns a future that will resolve once the lock has been
    /// successfully acquired.
    pub fn lock(&self) -> MutexLockFuture<'_, T> {
        println!("== Mutex::lock");
        let f = MutexLockFuture {
            mutex: Some(self),
            wait_key: WAIT_KEY_NONE,
        };
        println!("-- Mutex::lock");
        f
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `Mutex` mutably, no actual locking needs to
    /// take place -- the mutable borrow statically guarantees no locks exist.
    ///
    /// # Examples
    ///
    /// ```
    /// # futures::executor::block_on(async {
    /// use futures::lock::Mutex;
    ///
    /// let mut mutex = Mutex::new(0);
    /// *mutex.get_mut() = 10;
    /// assert_eq!(*mutex.lock().await, 10);
    /// # });
    /// ```
    pub fn get_mut(&mut self) -> &mut T {
        println!("== Mutex::get_mut");
        // We know statically that there are no other references to `self`, so
        // there's no need to lock the inner mutex.
        let m = unsafe { &mut *self.value.get() };

        println!("-- Mutex::get_mut");
        m
    }

    fn remove_waker(&self, wait_key: usize, wake_another: bool) {
        println!("Mutex::remove_waker...");
        if wait_key != WAIT_KEY_NONE {
            println!("Mutex::remove_waker wait_key != WAIT_KEY_NONE... lock waiters.");
            let mut waiters = self.waiters.lock().unwrap();
            println!("Mutex::remove_waker locked.");
            match waiters.remove(wait_key) {
                Waiter::Waiting(_) => {
                    println!("Mutex::remove_waker Waiting...");
                }
                Waiter::Woken => {
                    println!("Mutex::remove_waker Woken...");
                    // We were awoken, but then dropped before we could
                    // wake up to acquire the lock. Wake up another
                    // waiter.
                    if wake_another {
                        if let Some((_i, waiter)) = waiters.iter_mut().next() {
                            println!("Mutex::remove_waker waiter.wake()...");
                            waiter.wake();
                            println!("Mutex::remove_waker waiter.wake() done.");
                        }
                    }
                }
            }
            if waiters.is_empty() {
                println!("Mutex::remove_waker waiters empty...");
                self.state.fetch_and(!HAS_WAITERS, Ordering::Relaxed); // released by mutex unlock
                println!("Mutex::remove_waker waiters empty done.");
            }
        } else {
            println!("Mutex::wait_key == WAIT_KEY_NONE.");
        }
    }
}

// Sentinel for when no slot in the `Slab` has been dedicated to this object.
const WAIT_KEY_NONE: usize = usize::max_value();

/// A future which resolves when the target mutex has been successfully acquired.
pub struct MutexLockFuture<'a, T: ?Sized> {
    // `None` indicates that the mutex was successfully acquired.
    mutex: Option<&'a Mutex<T>>,
    wait_key: usize,
}

impl<T: ?Sized> fmt::Debug for MutexLockFuture<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutexLockFuture")
            .field("was_acquired", &self.mutex.is_none())
            .field("mutex", &self.mutex)
            .field(
                "wait_key",
                &(if self.wait_key == WAIT_KEY_NONE {
                    None
                } else {
                    Some(self.wait_key)
                }),
            )
            .finish()
    }
}

impl<T: ?Sized> FusedFuture for MutexLockFuture<'_, T> {
    fn is_terminated(&self) -> bool {
        self.mutex.is_none()
    }
}

impl<'a, T: ?Sized> Future for MutexLockFuture<'a, T> {
    type Output = MutexGuard<'a, T>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Self::Output> {
        println!("== MutexLockFuture::poll");
        let mutex =
            self.mutex.expect("polled MutexLockFuture after completion");
        println!("Mutex::poll... 2");

        if let Some(lock) = mutex.try_lock() {
            println!("Mutex::poll... try_lock...");
            mutex.remove_waker(self.wait_key, false);
            println!("Mutex::poll... remove_waker...");
            self.mutex = None;
            println!("Mutex::poll... returning Ready.");
            println!("-- MutexLockFuture::poll");
            return Poll::Ready(lock);
        } else {
            println!("Mutex::poll... did not get try_lock");
        }

        println!("Mutex::poll... try_lock failed.");

        {
            println!("Mutex::poll... getting waiters.");
            let mut waiters = mutex.waiters.lock().unwrap();
            println!("Mutex::poll... locked waiters.");
            if self.wait_key == WAIT_KEY_NONE {
                println!("Mutex::poll... got WAIT_KEY_NONE");
                self.wait_key =
                    waiters.insert(Waiter::Waiting(cx.waker().clone()));
                if waiters.len() == 1 {
                    mutex.state.fetch_or(HAS_WAITERS, Ordering::Relaxed); // released by mutex unlock
                }
            } else {
                println!("Mutex::poll... registering");
                waiters[self.wait_key].register(cx.waker());
            }
        }

        println!("Mutex::poll... try_lock again");

        // Ensure that we haven't raced `MutexGuard::drop`'s unlock path by
        // attempting to acquire the lock again.
        if let Some(lock) = mutex.try_lock() {
            println!("Mutex::poll... got try_lock again");
            mutex.remove_waker(self.wait_key, false);
            self.mutex = None;
            println!("-- MutexLockFuture::poll");
            return Poll::Ready(lock);
        } else {
            println!("Mutex::poll... did not get try_lock again");
        }

        println!("-- MutexLockFuture::poll");
        Poll::Pending
    }
}

impl<T: ?Sized> Drop for MutexLockFuture<'_, T> {
    fn drop(&mut self) {
        println!("== MutexLockFuture::drop");
        if let Some(mutex) = self.mutex {
            // This future was dropped before it acquired the mutex.
            //
            // Remove ourselves from the map, waking up another waiter if we
            // had been awoken to acquire the lock.
            mutex.remove_waker(self.wait_key, true);
        }
        println!("-- MutexLockFuture::drop");
    }
}

/// An RAII guard returned by the `lock` and `try_lock` methods.
/// When this structure is dropped (falls out of scope), the lock will be
/// unlocked.
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutexGuard")
            .field("value", &*self)
            .field("mutex", &self.mutex)
            .finish()
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        println!("== MutexGuard::drop");
        println!("MutexGuard dropping...");
        let old_state =
            self.mutex.state.fetch_and(!IS_LOCKED, Ordering::AcqRel);
        println!("MutexGuard drop fetch...");
        if (old_state & HAS_WAITERS) != 0 {
            println!("MutexGuard drop waiters.lcok()...");
            let mut waiters = self.mutex.waiters.lock().unwrap();
            println!("MutexGuard drop got waiters...");
            if let Some((_i, waiter)) = waiters.iter_mut().next() {
                println!("MutexGuard drop waking...");
                waiter.wake();
                println!("MutexGuard drop wake done...");
            }
        }
        println!("MutexGuard dropped.");
        println!("-- MutexGuard::drop");
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

// Mutexes can be moved freely between threads and acquired on any thread so long
// as the inner value can be safely sent between threads.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

// It's safe to switch which thread the acquire is being attempted on so long as
// `T` can be accessed on that thread.
unsafe impl<T: ?Sized + Send> Send for MutexLockFuture<'_, T> {}
// doesn't have any interesting `&self` methods (only Debug)
unsafe impl<T: ?Sized> Sync for MutexLockFuture<'_, T> {}

// Safe to send since we don't track any thread-specific details-- the inner
// lock is essentially spinlock-equivalent (attempt to flip an atomic bool)
unsafe impl<T: ?Sized + Send> Send for MutexGuard<'_, T> {}
unsafe impl<T: ?Sized + Sync> Sync for MutexGuard<'_, T> {}
