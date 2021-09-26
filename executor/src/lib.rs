use std::future::Future;

use futures::{
    executor::{LocalPool, LocalSpawner},
    channel::oneshot,
    task::LocalSpawnExt,
};

pub fn run<F>(f: impl FnOnce(LocalSpawner) -> F + 'static) -> F::Output
    where F: Future, F::Output: 'static
{
    // Set up a futures::executor.
    let mut pool = LocalPool::new();
    // We'll be sending our result through this channel when done.
    let (sender, mut receiver) = oneshot::channel();
    let spawner = pool.spawner();
    // Spawn the main future, and send its result when done.
    pool.spawner().spawn_local(async {
        let output = f(spawner).await;
        if let Err(_) = sender.send(output) {
            panic!("Receiver dropped");
        }
    }).expect("Failed to spawn");

    loop {
        pool.run_until_stalled();
        // Are we done yet?
        if let Some(output) = receiver.try_recv().unwrap() {
            return output;
        }
	      // Block waiting for I/O.
        ground::reactor::REACTOR.with(|r| r.borrow_mut().dispatch())
            .expect("Failed to dispatch the reactor");
    }
}
