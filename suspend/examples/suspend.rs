use std::sync::{
    atomic::{spin_loop_hint, AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use suspend::{block_on, Suspend};

fn main() {
    let exchange1 = Arc::new(AtomicUsize::new(0));
    let exchange2 = exchange1.clone();
    let mut susp = Suspend::new();
    let notifier = susp.notifier();

    let handle = thread::spawn(move || {
        let listen = susp.listen();
        let exchange1_1 = exchange1.clone();
        block_on(async move {
            exchange1_1.store(1, Ordering::Release);
            // wait in an async context
            listen.await
        });

        let listener = susp.listen();
        exchange1.store(2, Ordering::Release);
        // block on a notification
        listener.wait()
    });

    thread::spawn(move || {
        for i in 1..=2 {
            while exchange2.load(Ordering::Acquire) != i {
                spin_loop_hint();
            }
            notifier.notify();
        }
    });

    handle.join().unwrap();
}
