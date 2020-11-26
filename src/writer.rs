use log::debug;
use mkit::{
    cbor::{FromCbor, IntoCbor},
    thread,
};

use std::{
    borrow::BorrowMut,
    sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Arc, Mutex,
    },
    time,
};

use crate::{entry, journal::Journal, state, wral::Config, Error, Result};

pub enum Req {
    AddEntry { op: Vec<u8> },
}

pub enum Res {
    Seqno(u64),
    None,
}

pub struct Writer<S> {
    config: Config,
    seqno: Arc<AtomicU64>,
    journals: Vec<Journal<S>>,
    journal: Journal<S>,
}

impl<S> Writer<S> {
    pub(crate) fn start(
        config: Config,
        journals: Vec<Journal<S>>,
        journal: Journal<S>,
        seqno: u64,
    ) -> (Arc<Mutex<Writer<S>>>, thread::Thread<Req, Res, Result<u64>>)
    where
        S: 'static + Send + Clone + IntoCbor + FromCbor + state::State,
    {
        let seqno = Arc::new(AtomicU64::new(seqno));
        let w = Arc::new(Mutex::new(Writer {
            config: config.clone(),
            seqno: Arc::clone(&seqno),
            journals,
            journal,
        }));
        let name = format!("wral-writer-{}", config.name);
        // TODO: avoid magic number
        let thread_w = Arc::clone(&w);
        let t = thread::Thread::new_sync(&name, 1000, move |rx: thread::Rx<Req, Res>| MainLoop {
            config,
            seqno,
            w: thread_w,
            rx,
        });
        (w, t)
    }

    pub fn close(&self) -> Result<u64> {
        let n_batches: usize = self.journals.iter().map(|j| j.len_batches()).sum();
        let (n, m) = match self.journal.len_batches() {
            0 => (self.journals.len(), n_batches),
            n => (self.journals.len() + 1, n_batches + n),
        };
        let seqno = self.seqno.load(SeqCst);
        debug!(
            target: "wral",
            "{:?}/{} closed at seqno {}, with {} journals and {} batches",
            self.config.dir, self.config.name, seqno, m, n
        );
        Ok(self.seqno.load(SeqCst).saturating_sub(1))
    }

    pub fn purge(mut self) -> Result<u64> {
        self.close()?;

        for j in self.journals.drain(..) {
            j.purge()?
        }
        self.journal.purge()?;

        Ok(self.seqno.load(SeqCst).saturating_sub(1))
    }
}

struct MainLoop<S> {
    config: Config,
    seqno: Arc<AtomicU64>,
    w: Arc<Mutex<Writer<S>>>,
    rx: thread::Rx<Req, Res>,
}

impl<S> FnOnce<()> for MainLoop<S>
where
    S: Clone + IntoCbor + FromCbor + state::State,
{
    type Output = Result<u64>;

    extern "rust-call" fn call_once(mut self, _args: ()) -> Self::Output {
        let mut batch_size = self.config.batch_size;
        let mut last_flush = time::Instant::now();
        for req in self.rx {
            match req {
                (Req::AddEntry { op }, tx) => {
                    let mut w = err_at!(Fatal, self.w.lock())?;
                    let seqno = self.seqno.fetch_add(1, SeqCst);
                    w.journal.add_entry(entry::Entry::new(seqno, op))?;

                    batch_size = batch_size.saturating_sub(1);

                    if batch_size == 0 || last_flush.elapsed() > self.config.batch_period {
                        w.journal.flush();
                        // reload the flush thresholds
                        batch_size = self.config.batch_size;
                        last_flush = time::Instant::now();
                    }

                    match tx {
                        Some(tx) => err_at!(IPCFail, tx.send(Res::Seqno(seqno)))?,
                        None => (),
                    }

                    if w.journal.file_size()? > self.config.journal_limit {
                        Self::rotate(w.borrow_mut());
                    }
                }
            }
        }

        Ok(self.seqno.load(SeqCst).saturating_sub(1))
    }
}

impl<S> MainLoop<S> {
    fn rotate(_w: &mut Writer<S>) -> Result<()> {
        todo!()
    }
}
