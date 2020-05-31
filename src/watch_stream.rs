use futures_util::{
    future::{BoxFuture, FutureExt},
    stream::{Stream, StreamExt},
};
use std::{collections::HashMap, marker::Unpin, sync::Arc, time::Duration};
use tokio::sync::{oneshot, Mutex};

pub fn watch(
    mut stream: impl Stream<Item = String> + Unpin + Send + 'static,
) -> impl (Fn(String, Duration) -> BoxFuture<'static, bool>) {
    let watches: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>> = Default::default();

    let watches_task = watches.clone();

    tokio::spawn(async move {
        while let Some(message) = stream.next().await {
            let mut ws = watches_task.lock().await;

            if let Some(tx) = ws.remove(&message) {
                tx.send(()).unwrap();
            }
        }
    });

    move |text: String, timeout: Duration| {
        let watches_task = watches.clone();
        let (tx, rx) = oneshot::channel();
        async move {
            let mut ws = watches_task.lock().await;
            ws.insert(text.clone(), tx);
            drop(ws);

            if let Ok(result) = tokio::time::timeout(timeout, rx).await {
                result.unwrap();
                true
            } else {
                let mut ws = watches_task.lock().await;
                ws.remove(&text);
                false
            }
        }
        .boxed()
    }
}
