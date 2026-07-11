use std::sync::Arc;
use std::sync::{self, mpsc};

struct BroadcastInner<T: Clone> {
    queues: sync::Mutex<Vec<sync::mpsc::Sender<T>>>,
    is_closed: sync::atomic::AtomicBool,
}
pub struct Producer<T: Clone> {
    inner: Arc<BroadcastInner<T>>,
}
pub struct ConsumerGenerator<T: Clone> {
    inner: Arc<BroadcastInner<T>>,
}

impl<T: Clone> Producer<T> {
    pub fn send(&mut self, value: T) -> Result<(), mpsc::SendError<T>> {
        // TODO: correct ordering
        if self.inner.is_closed.load(sync::atomic::Ordering::SeqCst) {
            return Err(mpsc::SendError(value));
        }

        self.inner
            .queues
            .lock()
            .unwrap()
            // Remove elements from vec if send is error
            .retain_mut(|tx| tx.send(value.clone()).is_ok());

        Ok(())
    }
}

impl<T: Clone> ConsumerGenerator<T> {
    pub fn subscribe(&mut self) -> sync::mpsc::Receiver<T> {
        let (tx, rx) = sync::mpsc::channel();
        self.inner.queues.lock().unwrap().push(tx);
        rx
    }
    pub fn close(&mut self) {
        // TODO: correct ordering
        self.inner
            .is_closed
            .store(true, sync::atomic::Ordering::SeqCst);
    }
}

pub fn broadcast_channel<T: Clone>() -> (Producer<T>, ConsumerGenerator<T>) {
    let senders = Arc::new(BroadcastInner {
        queues: sync::Mutex::new(vec![]),
        is_closed: sync::atomic::AtomicBool::new(false),
    });

    (
        Producer {
            inner: Arc::clone(&senders),
        },
        ConsumerGenerator { inner: senders },
    )
}
