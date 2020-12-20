pub mod inmemory;

use core::marker::PhantomData;

use crate::inputs::{HasTargetBytes, Input};
use crate::observers::ObserversTuple;
use crate::tuples::{MatchNameAndType, MatchType, Named, TupleList};
use crate::AflError;

/// How an execution finished.
#[derive(FromPrimitive)]
pub enum ExitKind {
    Ok = 1,
    Crash = 2,
    OOM = 3,
    Timeout = 4,
}

pub trait HasObservers<OT>
where
    OT: ObserversTuple,
{
    /// Get the linked observers
    fn observers(&self) -> &OT;

    /// Get the linked observers
    fn observers_mut(&mut self) -> &mut OT;

    /// Reset the state of all the observes linked to this executor
    #[inline]
    fn reset_observers(&mut self) -> Result<(), AflError> {
        self.observers_mut().reset_all()
    }

    /// Run the post exec hook for all the observes linked to this executor
    #[inline]
    fn post_exec_observers(&mut self) -> Result<(), AflError> {
        self.observers_mut().post_exec_all()
    }
}

/// A simple executor that does nothing.
/// If intput len is 0, run_target will return Err
struct NopExecutor<I> {
    phantom: PhantomData<I>,
}

impl<I> Executor<I> for NopExecutor<I>
where
    I: Input + HasTargetBytes,
{
    fn run_target(&mut self, input: &I) -> Result<ExitKind, AflError> {
        if input.target_bytes().as_slice().len() == 0 {
            Err(AflError::Empty("Input Empty".into()))
        } else {
            Ok(ExitKind::Ok)
        }
    }
}

impl<I> Named for NopExecutor<I> {
    fn name(&self) -> &str {
        &"NopExecutor"
    }
}

/// An executor takes the given inputs, and runs the harness/target.
pub trait Executor<I>: Named
where
    I: Input,
{
    /// Instruct the target about the input and run
    fn run_target(&mut self, input: &I) -> Result<ExitKind, AflError>;
}

pub trait ExecutorsTuple<I>: MatchType + MatchNameAndType
where
    I: Input,
{
    fn for_each(&self, f: fn(&dyn Executor<I>));
    fn for_each_mut(&mut self, f: fn(&mut dyn Executor<I>));
}

impl<I> ExecutorsTuple<I> for ()
where
    I: Input,
{
    fn for_each(&self, _f: fn(&dyn Executor<I>)) {}
    fn for_each_mut(&mut self, _f: fn(&mut dyn Executor<I>)) {}
}

impl<Head, Tail, I> ExecutorsTuple<I> for (Head, Tail)
where
    Head: Executor<I> + 'static,
    Tail: ExecutorsTuple<I> + TupleList,
    I: Input,
{
    fn for_each(&self, f: fn(&dyn Executor<I>)) {
        f(&self.0);
        self.1.for_each(f)
    }

    fn for_each_mut(&mut self, f: fn(&mut dyn Executor<I>)) {
        f(&mut self.0);
        self.1.for_each_mut(f)
    }
}

#[cfg(test)]
mod test {
    use core::marker::PhantomData;

    use crate::executors::Executor;
    use crate::inputs::BytesInput;

    use super::NopExecutor;

    #[test]
    fn nop_executor() {
        let empty_input = BytesInput::new(vec![]);
        let nonempty_input = BytesInput::new(vec![1u8]);
        let mut executor = NopExecutor {
            phantom: PhantomData,
        };
        assert!(executor.run_target(&empty_input).is_err());
        assert!(executor.run_target(&nonempty_input).is_ok());
    }
}
