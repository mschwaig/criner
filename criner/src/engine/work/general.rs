use crate::Result;

/// A processor that can do anything, as it takes a future and returns its result
pub async fn processor<T>(
    input: async_std::sync::Receiver<futures::future::BoxFuture<'static, T>>,
    output: async_std::sync::Sender<T>,
) -> Result<()> {
    while let Some(fut) = input.recv().await {
        output.send(fut.await).await;
    }
    Ok(())
}
