use futures::compat::Future01CompatExt;
use jsonrpc_core_client::*;
use starcoin_crypto::HashValue;
use starcoin_rpc_api::types::{event::Event, pubsub::EventFilter, pubsub::Kind, pubsub::ThinBlock};

const STARCOIN_SUBSCRIPTION: &str = "starcoin_subscription";
const STARCOIN_SUBSCRIBE: &str = "starcoin_subscribe";
const STARCOIN_UNSUBSCRIBE: &str = "starcoin_unsubscribe";
#[derive(Clone)]
pub struct PubSubClient {
    client: TypedClient,
}

impl std::fmt::Debug for PubSubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PubSubClient")
    }
}

impl From<RpcChannel> for PubSubClient {
    fn from(channel: RpcChannel) -> Self {
        PubSubClient {
            client: channel.into(),
        }
    }
}

impl PubSubClient {
    pub async fn subscribe_events(
        &self,
        filter: EventFilter,
    ) -> Result<TypedSubscriptionStream<Event>, RpcError> {
        self.client
            .subscribe(
                STARCOIN_SUBSCRIBE,
                (Kind::Events, filter),
                STARCOIN_SUBSCRIPTION,
                STARCOIN_UNSUBSCRIBE,
                "Event",
            )
            .compat()
            .await
    }
    pub async fn subscribe_new_block(
        &self,
    ) -> Result<TypedSubscriptionStream<ThinBlock>, RpcError> {
        self.client
            .subscribe(
                STARCOIN_SUBSCRIBE,
                vec![Kind::NewHeads],
                STARCOIN_SUBSCRIPTION,
                STARCOIN_UNSUBSCRIBE,
                "ThinBlock",
            )
            .compat()
            .await
    }
    pub async fn subscribe_new_transactions(
        &self,
    ) -> Result<TypedSubscriptionStream<Vec<HashValue>>, RpcError> {
        self.client
            .subscribe(
                STARCOIN_SUBSCRIBE,
                vec![Kind::NewPendingTransactions],
                STARCOIN_SUBSCRIPTION,
                STARCOIN_UNSUBSCRIBE,
                "Vec<HashValue>",
            )
            .compat()
            .await
    }
}
