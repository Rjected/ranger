use crate::{P2PRelay, P2PRelayError};
use ethp2p_rs::EthMessage;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tower::Service;
use tracing::debug;

impl Service<EthMessage> for P2PRelay {
    /// The Response is an Option<EthMessage> because peers can return either a response to a
    /// request, or the message received is a broadcast message which does not require a response.
    type Response = Option<EthMessage>;
    type Error = P2PRelayError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: EthMessage) -> Self::Future {
        let status_map = Arc::clone(&self.status_map);
        Box::pin(async move {
            let response = match req {
                // respond to status messages by default
                EthMessage::Status(their_status) => {
                    debug!("Decoded status message from peer: {:?}", their_status);
                    let mut unlocked_status = status_map.write();

                    // if we can't find the other peer's status, add it to the map so we can send
                    // it to peers later
                    match unlocked_status.get(&u64::from(their_status.chain)) {
                        Some(&status) => Ok(Some(status.into())),
                        None => {
                            unlocked_status.insert(u64::from(their_status.chain), their_status);
                            Ok(Some(their_status.into()))
                        }
                    }
                }
                _ => Ok(None),
            };
            response
        })
    }
}
