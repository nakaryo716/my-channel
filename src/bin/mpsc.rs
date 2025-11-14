use futures::{StreamExt, executor::ThreadPool};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = ThreadPool::new()?;

    let fut = async move {
        let (mut tx, mut rx) = my_channel::mpsc::channel(5);

        let mut tx_c = tx.clone();
        pool.spawn_ok(async move {
            for i in 0..10 {
                tx_c.send(i).await.unwrap();
            }
        });

        pool.spawn_ok(async move {
            for i in 10..20 {
                tx.send(i).await.unwrap();
            }
        });

        while let Some(msg) = rx.next().await {
            println!("{}", msg);
        }
    };

    futures::executor::block_on(fut);

    Ok(())
}
