use ops;
use query;
use shortcut;

use std::borrow::Cow;
use std::sync;
use std::sync::atomic;
use std::sync::atomic::AtomicPtr;

type S = shortcut::Store<query::DataType, sync::Arc<Vec<query::DataType>>>;
pub struct WriteHandle {
    w_store: Option<Box<sync::Arc<S>>>,
    w_log: Vec<ops::Record>,
    bs: BufferedStore,
}

#[derive(Clone)]
pub struct BufferedStore(sync::Arc<AtomicPtr<sync::Arc<S>>>);

pub struct BufferedStoreBuilder {
    r_store: S,
    w_store: S,
}

impl WriteHandle {
    pub fn swap(&mut self) {
        use std::thread;

        // at this point, we have exclusive access to w_store, and it is up-to-date with all writes
        // r_store is accessed by readers through a sync::Weak upgrade, and has old data
        // w_log contains all the changes that are in w_store, but not in r_store
        //
        // we're going to do the following:
        //
        //  - atomically swap in a weak pointer to the current w_store into the BufferedStore,
        //    letting readers see new and updated state
        //  - store r_store as our new w_store
        //  - wait until we have exclusive access to this new w_store
        //  - replay w_log onto w_store

        // prepare w_store
        let w_store = self.w_store.take().unwrap();
        let w_store: *mut sync::Arc<S> = Box::into_raw(w_store);

        // swap in our w_store, and get r_store in return
        let r_store = self.bs.0.swap(w_store, atomic::Ordering::AcqRel);
        self.w_store = Some(unsafe { Box::from_raw(r_store) });

        // let readers go so they will be done with the old read Arc
        thread::yield_now();

        // now, wait for all existing readers to go away
        loop {
            if let Some(w_store) = sync::Arc::get_mut(&mut *self.w_store.as_mut().unwrap()) {
                // they're all gone
                // OR ARE THEY?
                // some poor reader could have *read* the pointer right before we swapped it,
                // *but not yet cloned the Arc*. we then check that there's only one strong
                // reference, *which there is*. *then* that reader upgrades their Arc => Uh-oh.
                // TODO XXX TODO XXX XXX TODO XXX XXX TODO XXX XXX TODO XXX XXX TODO XXX

                // put in all the updates the read store hasn't seen
                for r in self.w_log.drain(..) {
                    Self::apply(w_store, Cow::Owned(r));
                }

                // w_store (the old r_store) is now fully up to date!
                break;
            } else {
                thread::yield_now();
            }
        }
    }

    /// Add a new set of records to the backlog.
    ///
    /// These will be made visible to readers after the next call to `swap()`.
    pub fn add<I>(&mut self, rs: I)
        where I: IntoIterator<Item = ops::Record>
    {
        for r in rs {
            // apply to the current write set
            {
                let arc: &mut sync::Arc<_> = &mut *self.w_store.as_mut().unwrap();
                let s: &mut S = sync::Arc::get_mut(arc)
                    .expect("writer should always be sole owner outside of swap");
                Self::apply(s, Cow::Borrowed(&r));
            }
            // and also log it to later apply to the reads
            self.w_log.push(r);
        }
    }

    fn apply(store: &mut S, r: Cow<ops::Record>) {
        if let ops::Record::Positive(..) = *r {
            let (r, _) = r.into_owned().extract();
            store.insert(r);
            return;
        }

        match *r {
            ops::Record::Negative(ref r) => {
                // we need a cond that will match this row.
                let conds = r.iter()
                    .enumerate()
                    .map(|(coli, v)| {
                        shortcut::Condition {
                            column: coli,
                            cmp: shortcut::Comparison::Equal(shortcut::Value::using(v)),
                        }
                    })
                    .collect::<Vec<_>>();

                // however, multiple rows may have the same values as this row for
                // every column. afaict, it is safe to delete any one of these rows. we
                // do this by returning true for the first invocation of the filter
                // function, and false for all subsequent invocations.
                let mut first = true;
                store.delete_filter(&conds[..], |_| if first {
                    first = false;
                    true
                } else {
                    false
                });
            }
            _ => unreachable!(),
        }
    }
}

/// Allocate a new buffered `Store`.
pub fn new(cols: usize) -> BufferedStoreBuilder {
    BufferedStoreBuilder {
        w_store: shortcut::Store::new(cols),
        r_store: shortcut::Store::new(cols),
    }
}

impl BufferedStoreBuilder {
    pub fn index<I>(&mut self, column: usize, indexer: I)
        where I: Clone + Into<shortcut::Index<query::DataType>>
    {
        let i1 = indexer.clone();
        let i2 = indexer;
        self.w_store.index(column, i1);
        self.r_store.index(column, i2);
    }

    pub fn commit(self) -> (BufferedStore, WriteHandle) {
        let r =
            BufferedStore(sync::Arc::new(AtomicPtr::new(Box::into_raw(Box::new(sync::Arc::new(self.r_store))))));
        let w = WriteHandle {
            w_store: Some(Box::new(sync::Arc::new(self.w_store))),
            w_log: Vec::new(),
            bs: r.clone(),
        };
        (r, w)
    }
}

impl BufferedStore {
    /// Find all entries that matched the given conditions.
    ///
    /// Returned records are passed to `then` before being returned.
    ///
    /// Note that not all writes will be included with this read -- only those that have been
    /// swapped in by the writer.
    pub fn find_and<F, T>(&self, q: &[shortcut::cmp::Condition<query::DataType>], then: F) -> T
        where F: FnOnce(Vec<&sync::Arc<Vec<query::DataType>>>) -> T
    {
        use std::mem;
        let r_store = unsafe { Box::from_raw(self.0.load(atomic::Ordering::Acquire)) };
        let rs: sync::Arc<_> = (&*r_store).clone();
        mem::forget(r_store); // don't free the Box!
        let res = then(rs.find(q).collect());
        res
    }
}

pub mod index;

#[cfg(test)]
mod tests {
    use super::*;
    use ops;

    #[test]
    fn store_works() {
        let a = sync::Arc::new(vec![1.into(), "a".into()]);

        let (r, mut w) = new(2).commit();

        // nothing there initially
        assert_eq!(r.find_and(&[], |rs| rs.len()), 0);

        w.add(vec![ops::Record::Positive(a.clone())]);

        // not even after an add (we haven't swapped yet)
        assert_eq!(r.find_and(&[], |rs| rs.len()), 0);

        w.swap();

        // but after the swap, the record is there!
        assert_eq!(r.find_and(&[], |rs| rs.len()), 1);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == a[0] && r[1] == a[1])));
    }

    #[test]
    fn busybusybusy() {
        use shortcut;
        use std::thread;

        let mut db = new(1);
        db.index(0, shortcut::idx::HashIndex::new());

        let n = 10000;
        let (r, mut w) = db.commit();
        thread::spawn(move || for i in 0..n {
            w.add(vec![ops::Record::Positive(sync::Arc::new(vec![i.into()]))]);
            w.swap();
        });

        let mut cmp = vec![shortcut::Condition {
                               column: 0,
                               cmp: shortcut::Comparison::Equal(shortcut::Value::new(0)),
                           }];
        for i in 0..n {
            cmp[0].cmp = shortcut::Comparison::Equal(shortcut::Value::new(i));
            loop {
                let rows = r.find_and(&cmp[..], |rs| rs.len());
                match rows {
                    0 => continue,
                    1 => break,
                    i => assert_ne!(i, 1),
                }
            }
        }
    }

    #[test]
    fn minimal_query() {
        let a = sync::Arc::new(vec![1.into(), "a".into()]);
        let b = sync::Arc::new(vec![2.into(), "b".into()]);

        let (r, mut w) = new(2).commit();
        w.add(vec![ops::Record::Positive(a.clone())]);
        w.swap();
        w.add(vec![ops::Record::Positive(b.clone())]);

        assert_eq!(r.find_and(&[], |rs| rs.len()), 1);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == a[0] && r[1] == a[1])));
    }

    #[test]
    fn non_minimal_query() {
        let a = sync::Arc::new(vec![1.into(), "a".into()]);
        let b = sync::Arc::new(vec![2.into(), "b".into()]);
        let c = sync::Arc::new(vec![3.into(), "c".into()]);

        let (r, mut w) = new(2).commit();
        w.add(vec![ops::Record::Positive(a.clone())]);
        w.add(vec![ops::Record::Positive(b.clone())]);
        w.swap();
        w.add(vec![ops::Record::Positive(c.clone())]);

        assert_eq!(r.find_and(&[], |rs| rs.len()), 2);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == a[0] && r[1] == a[1])));
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == b[0] && r[1] == b[1])));
    }

    #[test]
    fn absorb_negative_immediate() {
        let a = sync::Arc::new(vec![1.into(), "a".into()]);
        let b = sync::Arc::new(vec![2.into(), "b".into()]);

        let (r, mut w) = new(2).commit();
        w.add(vec![ops::Record::Positive(a.clone())]);
        w.add(vec![ops::Record::Positive(b.clone())]);
        w.add(vec![ops::Record::Negative(a.clone())]);
        w.swap();

        assert_eq!(r.find_and(&[], |rs| rs.len()), 1);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == b[0] && r[1] == b[1])));
    }

    #[test]
    fn absorb_negative_later() {
        let a = sync::Arc::new(vec![1.into(), "a".into()]);
        let b = sync::Arc::new(vec![2.into(), "b".into()]);

        let (r, mut w) = new(2).commit();
        w.add(vec![ops::Record::Positive(a.clone())]);
        w.add(vec![ops::Record::Positive(b.clone())]);
        w.swap();
        w.add(vec![ops::Record::Negative(a.clone())]);
        w.swap();

        assert_eq!(r.find_and(&[], |rs| rs.len()), 1);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == b[0] && r[1] == b[1])));
    }

    #[test]
    fn absorb_multi() {
        let a = sync::Arc::new(vec![1.into(), "a".into()]);
        let b = sync::Arc::new(vec![2.into(), "b".into()]);
        let c = sync::Arc::new(vec![3.into(), "c".into()]);

        let (r, mut w) = new(2).commit();
        w.add(vec![ops::Record::Positive(a.clone()), ops::Record::Positive(b.clone())]);
        w.swap();

        assert_eq!(r.find_and(&[], |rs| rs.len()), 2);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == a[0] && r[1] == a[1])));
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == b[0] && r[1] == b[1])));

        w.add(vec![ops::Record::Negative(a.clone()),
                   ops::Record::Positive(c.clone()),
                   ops::Record::Negative(c.clone())]);
        w.swap();

        assert_eq!(r.find_and(&[], |rs| rs.len()), 1);
        assert!(r.find_and(&[], |rs| rs.iter().any(|r| r[0] == b[0] && r[1] == b[1])));
    }
}
