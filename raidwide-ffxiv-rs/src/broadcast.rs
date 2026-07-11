use std::sync;
use std::sync::Arc;

struct Inner<T: Clone>(sync::Mutex<Vec<sync::mpsc::Sender<T>>>);
pub struct Producer<T: Clone>(Arc<Inner<T>>);
pub struct ConsumerGenerator<T: Clone>(Arc<Inner<T>>);

impl<T: Clone> Producer<T> {
    pub fn send(&mut self, value: T) {
        let mut txs = self.0.0.lock().unwrap();
        for tx in txs.iter_mut() {
            tx.send(value.clone()).unwrap();
        }
    }
}

impl<T: Clone> ConsumerGenerator<T> {
    pub fn subscribe(&mut self) -> sync::mpsc::Receiver<T> {
        let (tx, rx) = sync::mpsc::channel();
        self.0.0.lock().unwrap().push(tx);
        rx
    }
}

pub fn broadcast_channel<T: Clone>() -> (Producer<T>, ConsumerGenerator<T>) {
    let senders = Arc::new(Inner(sync::Mutex::new(vec![])));

    (Producer(Arc::clone(&senders)), ConsumerGenerator(senders))
}
