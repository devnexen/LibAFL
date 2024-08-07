//! The queue corpus scheduler implements an AFL-like queue mechanism

use alloc::borrow::ToOwned;
use core::marker::PhantomData;

use crate::{
    corpus::{Corpus, CorpusId, HasTestcase},
    schedulers::{HasQueueCycles, RemovableScheduler, Scheduler},
    state::{HasCorpus, State, UsesState},
    Error,
};

/// Walk the corpus in a queue-like fashion
#[derive(Debug, Clone)]
pub struct QueueScheduler<S> {
    queue_cycles: u64,
    runs_in_current_cycle: u64,
    phantom: PhantomData<S>,
}

impl<S> UsesState for QueueScheduler<S>
where
    S: State,
{
    type State = S;
}

impl<S> RemovableScheduler for QueueScheduler<S> where S: HasCorpus + HasTestcase + State {}

impl<S> Scheduler for QueueScheduler<S>
where
    S: HasCorpus + HasTestcase + State,
{
    fn on_add(&mut self, state: &mut Self::State, id: CorpusId) -> Result<(), Error> {
        // Set parent id
        let current_id = *state.corpus().current();
        state
            .corpus()
            .get(id)?
            .borrow_mut()
            .set_parent_id_optional(current_id);

        Ok(())
    }

    /// Gets the next entry in the queue
    fn next(&mut self, state: &mut Self::State) -> Result<CorpusId, Error> {
        if state.corpus().count() == 0 {
            Err(Error::empty(
                "No entries in corpus. This often implies the target is not properly instrumented."
                    .to_owned(),
            ))
        } else {
            let id = state
                .corpus()
                .current()
                .map(|id| state.corpus().next(id))
                .flatten()
                .unwrap_or_else(|| state.corpus().first().unwrap());

            self.runs_in_current_cycle += 1;
            // TODO deal with corpus_counts decreasing due to removals
            if self.runs_in_current_cycle >= state.corpus().count() as u64 {
                self.queue_cycles += 1;
            }
            self.set_current_scheduled(state, Some(id))?;
            Ok(id)
        }
    }
}

impl<S> QueueScheduler<S> {
    /// Creates a new `QueueScheduler`
    #[must_use]
    pub fn new() -> Self {
        Self {
            runs_in_current_cycle: 0,
            queue_cycles: 0,
            phantom: PhantomData,
        }
    }
}

impl<S> Default for QueueScheduler<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> HasQueueCycles for QueueScheduler<S>
where
    S: HasCorpus + HasTestcase + State,
{
    fn queue_cycles(&self) -> u64 {
        self.queue_cycles
    }
}

#[cfg(test)]
#[cfg(feature = "std")]
mod tests {

    use std::{fs, path::PathBuf};

    use libafl_bolts::rands::StdRand;

    use crate::{
        corpus::{Corpus, OnDiskCorpus, Testcase},
        feedbacks::ConstFeedback,
        inputs::bytes::BytesInput,
        schedulers::{QueueScheduler, Scheduler},
        state::{HasCorpus, StdState},
    };

    #[test]
    fn test_queuecorpus() {
        let rand = StdRand::with_seed(4);
        let mut scheduler = QueueScheduler::new();

        let mut q =
            OnDiskCorpus::<BytesInput>::new(PathBuf::from("target/.test/fancy/path")).unwrap();
        let t = Testcase::with_filename(BytesInput::new(vec![0_u8; 4]), "fancyfile".into());
        q.add(t).unwrap();

        let objective_q =
            OnDiskCorpus::<BytesInput>::new(PathBuf::from("target/.test/fancy/objective/path"))
                .unwrap();

        let mut feedback = ConstFeedback::new(false);
        let mut objective = ConstFeedback::new(false);

        let mut state = StdState::new(rand, q, objective_q, &mut feedback, &mut objective).unwrap();

        let next_id = scheduler.next(&mut state).unwrap();
        let filename = state
            .corpus()
            .get(next_id)
            .unwrap()
            .borrow()
            .filename()
            .as_ref()
            .unwrap()
            .clone();

        assert_eq!(filename, "fancyfile");

        fs::remove_dir_all("target/.test/fancy/path").unwrap();
    }
}
