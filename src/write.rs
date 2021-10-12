use crate::read::ReadHandle;
use crate::Absorb;

use crate::sync::{fence, Arc, AtomicUsize, MutexGuard, Ordering};
use std::marker::PhantomData;
use std::ops::DerefMut;
use std::ptr::NonNull;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::{fmt, thread};

/// A writer handle to a left-right guarded data structure.
///
/// All operations on the underlying data should be enqueued as operations of type `Ops` using
/// [`append`](Self::append). The effect of this operations are only exposed to readers once
/// [`publish`](Self::publish) is called.
///
/// # Reading through a `WriteHandle`
///
/// `WriteHandle` allows access to a [`ReadHandle`] through `Deref<Target = ReadHandle>`. Note that
/// since the reads go through a [`ReadHandle`], those reads are subject to the same visibility
/// restrictions as reads that do not go through the `WriteHandle`: they only see the effects of
/// operations prior to the last call to [`publish`](Self::publish).
pub struct WriteHandle<T: Absorb<Ops>, Ops: Default> {
    epochs: crate::Epochs,
    w_handle: NonNull<T>,
    partial_ops: Ops,
    pending_ops: Ops,
    r_handle: ReadHandle<T>,
    last_epochs: Vec<usize>,
    #[cfg(test)]
    refreshes: usize,
    #[cfg(test)]
    is_waiting: Arc<AtomicBool>,
    /// A publish has happened, but the two copies have not been synchronized yet.
    second: bool,
    /// If we call `Self::take` the drop needs to be different.
    taken: bool,
}

// safety: if a `WriteHandle` is sent across a thread boundary, we need to be able to take
// ownership of both Ts and Os across that thread boundary. since `WriteHandle` holds a
// `ReadHandle`, we also need to respect its Send requirements.
unsafe impl<T, Ops> Send for WriteHandle<T, Ops>
where
    T: Absorb<Ops>,
    T: Send,
    Ops: Send + Default,
    ReadHandle<T>: Send,
{
}

impl<T, Ops> fmt::Debug for WriteHandle<T, Ops>
where
    T: Absorb<Ops> + fmt::Debug,
    Ops: fmt::Debug + Default,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteHandle")
            .field("epochs", &self.epochs)
            .field("w_handle", &self.w_handle)
            .field("partial_op", &self.partial_ops)
            .field("pending_op", &self.pending_ops)
            .field("r_handle", &self.r_handle)
            .field("second", &self.second)
            .finish()
    }
}

/// A **smart pointer** to an owned backing data structure. This makes sure that the
/// data is dropped correctly (using [`Absorb::drop_second`]).
///
/// Additionally it allows for unsafely getting the inner data out using [`into_box()`](Taken::into_box).
pub struct Taken<T: Absorb<Ops>, Ops: Default> {
    inner: Option<Box<T>>,
    _marker: PhantomData<Ops>,
}

impl<T: Absorb<Ops> + std::fmt::Debug, Ops: Default> std::fmt::Debug for Taken<T, Ops> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Taken")
            .field(
                "inner",
                self.inner
                    .as_ref()
                    .expect("inner is only taken in `into_box` which drops self"),
            )
            .finish()
    }
}

impl<T: Absorb<Ops>, Ops: Default> Deref for Taken<T, Ops> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner
            .as_ref()
            .expect("inner is only taken in `into_box` which drops self")
    }
}

impl<T: Absorb<Ops>, Ops: Default> DerefMut for Taken<T, Ops> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
            .as_mut()
            .expect("inner is only taken in `into_box` which drops self")
    }
}

impl<T: Absorb<Ops>, Ops: Default> Taken<T, Ops> {
    /// This is unsafe because you must call [`Absorb::drop_second`] in
    /// case just dropping `T` is not safe and sufficient.
    ///
    /// If you used the default implementation of [`Absorb::drop_second`] (which just calls [`drop`](Drop::drop))
    /// you don't need to call [`Absorb::drop_second`].
    pub unsafe fn into_box(mut self) -> Box<T> {
        self.inner
            .take()
            .expect("inner is only taken here then self is dropped")
    }
}

impl<T: Absorb<Ops>, Ops: Default> Drop for Taken<T, Ops> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            T::drop_second(inner);
        }
    }
}

impl<T: Absorb<Ops>, Ops: Default> WriteHandle<T, Ops> {
    /// Takes out the inner backing data structure if it hasn't been taken yet. Otherwise returns `None`.
    ///
    /// Makes sure that all the pending operations are applied and waits till all the read handles
    /// have departed. Then it uses [`Absorb::drop_first`] to drop one of the copies of the data and
    /// returns the other copy as a [`Taken`] smart pointer.
    fn take_inner(&mut self) -> Option<Taken<T, Ops>> {
        use std::ptr;
        // Can only take inner once.
        if self.taken {
            return None;
        }

        // Disallow taking again.
        self.taken = true;

        // first, ensure both copies are up to date
        // (otherwise safely dropping the possibly duplicated w_handle data is a pain)
        if !T::is_empty(&self.partial_ops) || !T::is_empty(&self.pending_ops) {
            self.publish();
            // first publish moved pending into partial, publish again if not empty.
            if !T::is_empty(&self.partial_ops) {
                self.publish();
            }
        }
        // All ops are absorbed by both copies
        assert!(T::is_empty(&self.partial_ops) && T::is_empty(&self.pending_ops));

        // next, grab the read handle and set it to NULL
        let r_handle = self.r_handle.inner.swap(ptr::null_mut(), Ordering::Release);

        // now, wait for all readers to depart
        let epochs = Arc::clone(&self.epochs);
        let mut epochs = epochs.lock().unwrap();
        self.wait(&mut epochs);

        // ensure that the subsequent epoch reads aren't re-ordered to before the swap
        fence(Ordering::SeqCst);

        // all readers have now observed the NULL, so we own both handles.
        // all operations have been applied to both w_handle and r_handle.
        // give the underlying data structure an opportunity to handle the one copy differently:
        //
        // safety: w_handle was initially crated from a `Box`, and is no longer aliased.
        Absorb::drop_first(unsafe { Box::from_raw(self.w_handle.as_ptr()) });

        // next we take the r_handle and return it as a boxed value.
        //
        // this is safe, since we know that no readers are using this pointer
        // anymore (due to the .wait() following swapping the pointer with NULL).
        //
        // safety: r_handle was initially crated from a `Box`, and is no longer aliased.
        let boxed_r_handle = unsafe { Box::from_raw(r_handle) };

        Some(Taken {
            inner: Some(boxed_r_handle),
            _marker: PhantomData,
        })
    }
}

impl<T: Absorb<Ops>, Ops: Default> Drop for WriteHandle<T, Ops> {
    fn drop(&mut self) {
        if let Some(inner) = self.take_inner() {
            drop(inner);
        }
    }
}

impl<T: Absorb<Ops>, Ops: Default> WriteHandle<T, Ops> {
    pub(crate) fn new(w_handle: T, epochs: crate::Epochs, r_handle: ReadHandle<T>) -> Self {
        assert!(T::is_empty(&Ops::default()));
        Self {
            epochs,
            // safety: Box<T> is not null and covariant.
            w_handle: unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(w_handle))) },
            partial_ops: Ops::default(),
            pending_ops: Ops::default(),
            r_handle,
            last_epochs: Vec::new(),
            #[cfg(test)]
            is_waiting: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            refreshes: 0,
            second: true,
            taken: false,
        }
    }

    fn wait(&mut self, epochs: &mut MutexGuard<'_, slab::Slab<Arc<AtomicUsize>>>) {
        let mut iter = 0;
        let mut starti = 0;

        #[cfg(test)]
        {
            self.is_waiting.store(true, Ordering::Relaxed);
        }
        // we're over-estimating here, but slab doesn't expose its max index
        self.last_epochs.resize(epochs.capacity(), 0);
        'retry: loop {
            // read all and see if all have changed (which is likely)
            for (ii, (ri, epoch)) in epochs.iter().enumerate().skip(starti) {
                // if the reader's epoch was even last we read it (which was _after_ the swap),
                // then they either do not have the pointer, or must have read the pointer strictly
                // after the swap. in either case, they cannot be using the old pointer value (what
                // is now w_handle).
                //
                // note that this holds even with wrap-around since std::u{N}::MAX == 2 ^ N - 1,
                // which is odd, and std::u{N}::MAX + 1 == 0 is even.
                //
                // note also that `ri` _may_ have been re-used since we last read into last_epochs.
                // this is okay though, as a change still implies that the new reader must have
                // arrived _after_ we did the atomic swap, and thus must also have seen the new
                // pointer.
                if self.last_epochs[ri] % 2 == 0 {
                    continue;
                }

                let now = epoch.load(Ordering::Acquire);
                if now != self.last_epochs[ri] {
                    // reader must have seen the last swap, since they have done at least one
                    // operation since we last looked at their epoch, which _must_ mean that they
                    // are no longer using the old pointer value.
                } else {
                    // reader may not have seen swap
                    // continue from this reader's epoch
                    starti = ii;

                    if !cfg!(loom) {
                        // how eagerly should we retry?
                        if iter != 20 {
                            iter += 1;
                        } else {
                            thread::yield_now();
                        }
                    }

                    #[cfg(loom)]
                    loom::thread::yield_now();

                    continue 'retry;
                }
            }
            break;
        }
        #[cfg(test)]
        {
            self.is_waiting.store(false, Ordering::Relaxed);
        }
    }

    /// Publish all operations append to the log to reads.
    ///
    /// This method needs to wait for all readers to move to the "other" copy of the data so that
    /// it can replay the operational log onto the stale copy the readers used to use. This can
    /// take some time, especially if readers are executing slow operations, or if there are many
    /// of them.
    pub fn publish(&mut self) -> &mut Self {
        // we need to wait until all epochs have changed since the swaps *or* until a "finished"
        // flag has been observed to be on for two subsequent iterations (there still may be some
        // readers present since we did the previous refresh)
        //
        // NOTE: it is safe for us to hold the lock for the entire duration of the swap. we will
        // only block on pre-existing readers, and they are never waiting to push onto epochs
        // unless they have finished reading.
        let epochs = Arc::clone(&self.epochs);
        let mut epochs = epochs.lock().unwrap();

        self.wait(&mut epochs);

        // all the readers have left!
        // safety: we haven't freed the Box, and no readers are accessing the w_handle
        let w_handle = unsafe { self.w_handle.as_mut() };

        // safety: we will not swap while we hold this reference
        let r_handle = unsafe {
            self.r_handle
                .inner
                .load(Ordering::Acquire)
                .as_ref()
                .unwrap()
        };

        if self.second {
            Absorb::sync_with(w_handle, r_handle);
            self.second = false
        }

        // the w_handle copy has not seen any of the writes in the oplog
        // the r_handle copy has not seen any of the writes following swap_index
        // we can drain out the operations that only the w_handle copy needs
        //
        // NOTE: the if above is because drain(0..0) would remove 0
        T::absorb_second(w_handle, &mut self.partial_ops, r_handle);
        if !T::is_empty(&self.partial_ops) {
            self.partial_ops = Ops::default();
        }

        // we cannot give owned operations to absorb_first
        // since they'll also be needed by the r_handle copy
        T::absorb_first(w_handle, &mut self.pending_ops, r_handle);

        std::mem::swap(&mut self.partial_ops, &mut self.pending_ops);
        // the w_handle copy is about to become the r_handle, and can ignore the oplog

        // w_handle (the old r_handle) is now fully up to date!

        // at this point, we have exclusive access to w_handle, and it is up-to-date with all
        // writes. the stale r_handle is accessed by readers through an Arc clone of atomic pointer
        // inside the ReadHandle. oplog contains all the changes that are in w_handle, but not in
        // r_handle.
        //
        // it's now time for us to swap the copies so that readers see up-to-date results from
        // w_handle.

        // swap in our w_handle, and get r_handle in return
        let r_handle = self
            .r_handle
            .inner
            .swap(self.w_handle.as_ptr(), Ordering::Release);

        // NOTE: at this point, there are likely still readers using r_handle.
        // safety: r_handle was also created from a Box, so it is not null and is covariant.
        self.w_handle = unsafe { NonNull::new_unchecked(r_handle) };

        // ensure that the subsequent epoch reads aren't re-ordered to before the swap
        fence(Ordering::SeqCst);

        for (ri, epoch) in epochs.iter() {
            self.last_epochs[ri] = epoch.load(Ordering::Acquire);
        }

        #[cfg(test)]
        {
            self.refreshes += 1;
        }

        self
    }

    /// Publish as necessary to ensure that all operations are visible to readers.
    ///
    /// `WriteHandle::publish` will *always* wait for old readers to depart and swap the maps.
    /// This method will only do so if there are pending operations.
    pub fn flush(&mut self) {
        if self.has_pending_operations() {
            self.publish();
        }
    }

    /// Returns true if there are operations in the operational log that have not yet been exposed
    /// to readers.
    pub fn has_pending_operations(&self) -> bool {
        !T::is_empty(&self.pending_ops)
    }

    /// Append the given operation to the operational log.
    ///
    /// Its effects will not be exposed to readers until you call [`publish`](Self::publish).
    pub fn pending(&mut self) -> &mut Ops {
        &mut self.pending_ops
    }

    /// Returns a raw pointer to the write copy of the data (the one readers are _not_ accessing).
    ///
    /// Note that it is only safe to mutate through this pointer if you _know_ that there are no
    /// readers still present in this copy. This is not normally something you know; even after
    /// calling `publish`, readers may still be in the write copy for some time. In general, the
    /// only time you know this is okay is before the first call to `publish` (since no readers
    /// ever entered the write copy).
    // TODO: Make this return `Option<&mut T>`,
    // and only `Some` if there are indeed to readers in the write copy.
    pub fn raw_write_handle(&mut self) -> NonNull<T> {
        self.w_handle
    }

    /// Returns the backing data structure.
    ///
    /// Makes sure that all the pending operations are applied and waits till all the read handles
    /// have departed. Then it uses [`Absorb::drop_first`] to drop one of the copies of the data and
    /// returns the other copy as a [`Taken`] smart pointer.
    pub fn take(mut self) -> Taken<T, Ops> {
        // It is always safe to `expect` here because `take_inner` is private
        // and it is only called here and in the drop impl. Since we have an owned
        // `self` we know the drop has not yet been called. And every first call of
        // `take_inner` returns `Some`
        self.take_inner()
            .expect("inner is only taken here then self is dropped")
    }
}

// allow using write handle for reads
use std::ops::Deref;
impl<T: Absorb<Ops>, Ops: Default> Deref for WriteHandle<T, Ops> {
    type Target = ReadHandle<T>;
    fn deref(&self) -> &Self::Target {
        &self.r_handle
    }
}

/// `WriteHandle` can be sent across thread boundaries:
///
/// ```
/// use left_right::WriteHandle;
///
/// struct Data;
/// impl left_right::Absorb<()> for Data {
///     fn is_empty(_: &()) -> bool { true }
///     fn absorb_first(&mut self, _: &mut (), _: &Self) {}
///     fn sync_with(&mut self, _: &Self) {}
/// }
///
/// fn is_send<T: Send>() {
///   // dummy function just used for its parameterized type bound
/// }
///
/// is_send::<WriteHandle<Data, ()>>()
/// ```
///
/// As long as the inner types allow that of course.
/// Namely, the data type has to be `Send`:
///
/// ```compile_fail
/// use left_right::WriteHandle;
/// use std::rc::Rc;
///
/// struct Data(Rc<()>);
/// impl left_right::Absorb<()> for Data {
///     fn is_empty(_: &()) -> bool { true }
///     fn absorb_first(&mut self, _: &mut (), _: &Self) {}
/// }
///
/// fn is_send<T: Send>() {
///   // dummy function just used for its parameterized type bound
/// }
///
/// is_send::<WriteHandle<Data, ()>>()
/// ```
///
/// .. the operation type has to be `Send`:
///
/// ```compile_fail
/// use left_right::WriteHandle;
/// use std::rc::Rc;
///
/// struct Data;
/// impl left_right::Absorb<Rc<()>> for Data {
///     fn is_empty(_: &Rc<()>) -> bool { true }
///     fn absorb_first(&mut self, _: &mut Rc<()>, _: &Self) {}
/// }
///
/// fn is_send<T: Send>() {
///   // dummy function just used for its parameterized type bound
/// }
///
/// is_send::<WriteHandle<Data, Rc<()>>>()
/// ```
///
/// .. and the data type has to be `Sync` so it's still okay to read through `ReadHandle`s:
///
/// ```compile_fail
/// use left_right::WriteHandle;
/// use std::cell::Cell;
///
/// struct Data(Cell<()>);
/// impl left_right::Absorb<()> for Data {
///     fn is_empty(_: &()) -> bool { true }
///     fn absorb_first(&mut self, _: &mut (), _: &Self) {}
/// }
///
/// fn is_send<T: Send>() {
///   // dummy function just used for its parameterized type bound
/// }
///
/// is_send::<WriteHandle<Data, ()>>()
/// ```
#[allow(dead_code)]
struct CheckWriteHandleSend;

#[cfg(test)]
mod tests {
    use crate::sync::{AtomicUsize, Mutex, Ordering};
    use crate::Absorb;
    use slab::Slab;
    include!("./utilities.rs");

    #[test]
    fn append_test() {
        let (mut w, _r) = crate::new::<i32, _>();
        w.pending().0.push(1);
        assert_eq!(w.partial_ops.0.len(), 0);
        assert_eq!(w.pending_ops.0.len(), 1);
        w.publish();
        w.pending().0.push(2);
        w.pending().0.push(3);
        assert_eq!(w.partial_ops.0.len(), 1);
        assert_eq!(w.pending_ops.0.len(), 2);
    }

    #[test]
    fn take_test() {
        // publish twice then take with no pending operations
        let (mut w, _r) = crate::new_from_empty::<i32, _>(2);
        w.pending().0.push(1);
        w.publish();
        w.pending().0.push(1);
        w.publish();
        assert_eq!(*w.take(), 4);

        // publish twice then pending operation published by take
        let (mut w, _r) = crate::new_from_empty::<i32, _>(2);
        w.pending().0.push(1);
        w.publish();
        w.pending().0.push(2);
        w.publish();
        w.pending().0.push(3);
        assert_eq!(*w.take(), 8);

        // normal publish then pending operations published by take
        let (mut w, _r) = crate::new_from_empty::<i32, _>(2);
        w.pending().0.push(1);
        w.publish();
        w.pending().0.push(1);
        assert_eq!(*w.take(), 4);

        // pending operations published by take
        let (mut w, _r) = crate::new_from_empty::<i32, _>(2);
        w.pending().0.push(1);
        assert_eq!(*w.take(), 3);

        // emptry op queue
        let (mut w, _r) = crate::new_from_empty::<i32, _>(2);
        w.pending().0.push(1);
        w.publish();
        assert_eq!(*w.take(), 3);

        // no operations
        let (w, _r) = crate::new_from_empty::<i32, _>(2);
        assert_eq!(*w.take(), 2);
    }

    #[test]
    fn wait_test() {
        use std::sync::{Arc, Barrier};
        use std::thread;
        let (mut w, _r) = crate::new::<i32, _>();

        // Case 1: If epoch is set to default.
        let test_epochs: crate::Epochs = Default::default();
        let mut test_epochs = test_epochs.lock().unwrap();
        // since there is no epoch to waiting for, wait function will return immediately.
        w.wait(&mut test_epochs);

        // Case 2: If one of the reader is still reading(epoch is odd and count is same as in last_epoch)
        // and wait has been called.
        let held_epoch = Arc::new(AtomicUsize::new(1));

        w.last_epochs = vec![2, 2, 1];
        let mut epochs_slab = Slab::new();
        epochs_slab.insert(Arc::new(AtomicUsize::new(2)));
        epochs_slab.insert(Arc::new(AtomicUsize::new(2)));
        epochs_slab.insert(Arc::clone(&held_epoch));

        let barrier = Arc::new(Barrier::new(2));

        let is_waiting = Arc::clone(&w.is_waiting);

        // check writers waiting state before calling wait.
        let is_waiting_v = is_waiting.load(Ordering::Relaxed);
        assert_eq!(false, is_waiting_v);

        let barrier2 = Arc::clone(&barrier);
        let test_epochs = Arc::new(Mutex::new(epochs_slab));
        let wait_handle = thread::spawn(move || {
            barrier2.wait();
            let mut test_epochs = test_epochs.lock().unwrap();
            w.wait(&mut test_epochs);
        });

        barrier.wait();

        // make sure that writer wait() will call first, only then allow to updates the held epoch.
        while !is_waiting.load(Ordering::Relaxed) {
            thread::yield_now();
        }

        held_epoch.fetch_add(1, Ordering::SeqCst);

        // join to make sure that wait must return after the progress/increment
        // of held_epoch.
        let _ = wait_handle.join();
    }

    #[test]
    fn flush_noblock() {
        let (mut w, r) = crate::new::<i32, _>();
        w.pending().0.push(42);
        w.publish();
        assert_eq!(*r.enter().unwrap(), 42);

        // pin the epoch
        let _count = r.enter();
        // refresh would hang here
        assert_eq!(w.partial_ops.0.len(), 1);
        assert_eq!(w.pending_ops.0.len(), 0);
        assert!(!w.has_pending_operations());
    }

    #[test]
    fn flush_no_refresh() {
        let (mut w, _) = crate::new::<i32, _>();

        // Until we refresh, writes are written directly instead of going to the
        // oplog (because there can't be any readers on the w_handle table).
        assert!(!w.has_pending_operations());
        w.publish();
        assert!(!w.has_pending_operations());
        assert_eq!(w.refreshes, 1);

        w.pending().0.push(42);
        assert!(w.has_pending_operations());
        w.publish();
        assert!(!w.has_pending_operations());
        assert_eq!(w.refreshes, 2);

        w.pending().0.push(42);
        assert!(w.has_pending_operations());
        w.publish();
        assert!(!w.has_pending_operations());
        assert_eq!(w.refreshes, 3);

        // Sanity check that a refresh would have been visible
        assert!(!w.has_pending_operations());
        w.publish();
        assert_eq!(w.refreshes, 4);
    }
}
