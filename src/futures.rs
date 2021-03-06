use super::{trap, Error, Signal};
use crossbeam_channel as xchan;
use futures::stream::Stream;
use futures::task::AtomicTask;
use futures::{Async, Poll};
use std::sync::Arc;

/// An asynchronous stream of [Signal]s generated by [trap_stream].
pub struct SignalStream {
    task: Arc<AtomicTask>,
    recv: xchan::Receiver<Signal>,
}

impl Stream for SignalStream {
    type Item = Signal;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.task.register();
        match self.recv.try_recv() {
            Ok(signal) => Ok(Async::Ready(Some(signal))),
            Err(xchan::TryRecvError::Empty) => Ok(Async::NotReady),
            Err(xchan::TryRecvError::Disconnected) => Ok(Async::Ready(None)),
        }
    }
}

/// Traps one or more [Signal]s into a [SignalStream]. During the
/// execution of the body function, all signals specified will be yielded as
/// items in the stream.
///
/// # Arguments
///
/// * `signals` - A list of signals to trap during the execution of `body`.
///
/// * `body` - A function which accepts a [SignalStream] that generates the
/// specified signals in the order they are received.
pub fn trap_stream<RT: Sized>(
    signals: &'static [Signal],
    body: impl FnOnce(SignalStream) -> RT,
) -> Result<RT, Error> {
    let (send, recv) = xchan::bounded(1);
    let task = Arc::new(AtomicTask::new());
    let stream = SignalStream {
        task: task.clone(),
        recv,
    };
    trap(
        signals,
        move |signal| {
            send.send(signal).unwrap();
            task.notify();
        },
        move || body(stream),
    )
}
