use crate::tests::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct BlockingAuth(Arc<AtomicUsize>);
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl AuthService for BlockingAuth {
    fn status(&self) -> AuthFuture<'_, Vec<AuthStatus>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            let _active = Active(self.0.clone());
            std::future::pending().await
        })
    }
    fn login(&self, _: String, _: AuthMode, _: Option<String>, _: bool) -> AuthFuture<'_, LoginResult> {
        Box::pin(std::future::pending())
    }
    fn login_complete(&self, _: String, _: Option<String>) -> AuthFuture<'_, AuthStatus> {
        Box::pin(std::future::pending())
    }
    fn logout(&self, _: String) -> AuthFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
    fn list_models(&self, _: Option<String>) -> AuthFuture<'_, Vec<ModelInfo>> {
        Box::pin(std::future::pending())
    }
}
async fn flood(f: &Fixture) -> tokio::net::UnixStream {
    let mut stream = tokio::net::UnixStream::connect(paths::socket(f.dir.path())).await.unwrap();
    write_frame(&mut stream, &Frame::json(kind::HELLO, &ClientHello::ours("flood", ClientRole::Owner)).unwrap()).await.unwrap();
    read_frame(&mut stream).await.unwrap().unwrap();
    for id in 1..=100 {
        write_frame(&mut stream, &Frame::json(kind::CONTROL, &Request { id, method: Method::AuthStatus {} }).unwrap()).await.unwrap();
    }
    stream
}
async fn wait_count(count: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(Ordering::SeqCst) != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn blocked_requests_are_capped_per_connection_and_globally() {
    let f = Fixture::new(vec![]).await;
    let count = Arc::new(AtomicUsize::new(0));
    let mut config = f.config();
    config.auth = Arc::new(BlockingAuth(count.clone()));
    let server = Server::start(config).await.unwrap();
    let healthy = f.connect().await;
    let mut streams = vec![flood(&f).await];
    wait_count(&count, 64).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(count.load(Ordering::SeqCst), 64);
    healthy.call(Method::ServerInfo {}).await.unwrap();
    for expected in [128, 192, 256] {
        streams.push(flood(&f).await);
        wait_count(&count, expected).await;
    }
    assert!(matches!(healthy.call(Method::AuthStatus {}).await, Err(otto_client::ClientError::Rpc(RpcError { code, .. })) if code == RpcError::BUSY));
    healthy.call(Method::Ping {}).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 256);
    tokio::time::timeout(Duration::from_secs(5), server.shutdown()).await.unwrap().unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(streams);
}
