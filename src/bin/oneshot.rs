use futures::executor::ThreadPool;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = ThreadPool::new()?;

    let pool_c = pool.clone();
    let fut = async move {
        let (tx, rx) = my_channel::oneshot::channel();

        pool_c.spawn_ok(async move {
            if let Err(e) = tx.send(1) {
                eprintln!("send err: {}", e);
            }
        });
        println!("{}", rx.await.unwrap());
    };

    futures::executor::block_on(fut);

    Ok(())
}
