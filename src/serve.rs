use std::{fmt, sync::Arc};

use futures::{
    future::{self, Either},
    FutureExt, SinkExt, StreamExt,
};
use hyper::{upgrade::Upgraded, Body, Request, Response, StatusCode};
use juniper::{GraphQLSubscriptionType, GraphQLTypeAsync, RootNode, ScalarValue};
use sha1::Digest;
use tokio_tungstenite::{
    tungstenite::{self, protocol::Role},
    WebSocketStream,
};

use crate::juniper_graphql_ws::{ArcSchema, ClientMessage, Connection, Init};

const WS_UUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub fn websocket_upgrade_response(key: &str, protocol: Option<&str>) -> Response<Body> {
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_UUID);
    let b64 = base64::encode(&hasher.finalize());

    let mut b = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", b64);

    if let Some(p) = protocol {
        b = b.header("sec-websocket-protocol", p);
    }


    let response = b.body(Body::empty()).unwrap();
    tracing::trace!("websocket upgrade response: {:?}", &response);
    response
}

pub fn is_websocket_upgrade(request: &Request<Body>) -> bool {
    let headers = request.headers();

    if !headers
        .get("connection")
        .map(|x| {
            x.to_str()
                .map(|s| s.split(",").any(|y| y.trim().to_lowercase() == "upgrade"))
                .unwrap_or(false)
        })
        .unwrap_or(false)
    {
        return false;
    }

    if !headers
        .get("upgrade")
        .map(|x| x == "websocket")
        .unwrap_or(false)
    {
        return false;
    }

    if !headers
        .get("sec-websocket-version")
        .map(|x| x == "13")
        .unwrap_or(false)
    {
        return false;
    }

    headers.contains_key("sec-websocket-key")
}

pub fn websocket_key(request: &Request<Body>) -> Option<String> {
    request
        .headers()
        .get("sec-websocket-key")?
        .to_str()
        .map(|x| x.to_string())
        .ok()
}

struct Message(String);

impl<S: ScalarValue> std::convert::TryFrom<Message> for ClientMessage<S> {
    type Error = serde_json::Error;

    fn try_from(msg: Message) -> serde_json::Result<Self> {
        serde_json::from_slice(msg.0.as_bytes())
    }
}

#[derive(Debug)]
pub enum Error {
    Tungstenite(tungstenite::Error),
    Serde(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tungstenite(e) => write!(f, "tungstenite error: {}", e),
            Self::Serde(e) => write!(f, "serde error: {}", e),
        }
    }
}

impl std::error::Error for Error {}

pub fn serve_graphql_ws<Query, Mutation, Subscription, CtxT, S, I>(
    mut request: Request<Body>,
    schema: Arc<RootNode<'static, Query, Mutation, Subscription, S>>,
    init: I,
) -> Response<Body>
where
    Query: GraphQLTypeAsync<S, Context = CtxT> + Send + 'static,
    Query::TypeInfo: Send + Sync,
    Mutation: GraphQLTypeAsync<S, Context = CtxT> + Send + 'static,
    Mutation::TypeInfo: Send + Sync,
    Subscription: GraphQLSubscriptionType<S, Context = CtxT> + Send + 'static,
    Subscription::TypeInfo: Send + Sync,
    CtxT: Unpin + Send + Sync + 'static,
    S: ScalarValue + Send + Sync + 'static,
    I: Init<S, CtxT> + Send,
{
    let key = websocket_key(&request).unwrap();

    tokio::task::spawn(async move {
        match hyper::upgrade::on(&mut request).await {
            Ok(upgraded) => {
                let websocket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    upgraded,
                    Role::Server,
                    None,
                )
                .await;
                tracing::debug!("Establishing websocket");
                bind_streams(websocket, schema, init).await.unwrap();
            }
            Err(e) => eprintln!("upgrade error: {:?}", e),
        }
    });

    let response = websocket_upgrade_response(&key, Some("graphql-ws"));
    response
}

async fn bind_streams<Query, Mutation, Subscription, CtxT, S, I>(
    websocket: WebSocketStream<Upgraded>,
    root_node: Arc<RootNode<'static, Query, Mutation, Subscription, S>>,
    init: I,
) -> Result<(), Error>
where
    Query: GraphQLTypeAsync<S, Context = CtxT> + Send + 'static,
    Query::TypeInfo: Send + Sync,
    Mutation: GraphQLTypeAsync<S, Context = CtxT> + Send + 'static,
    Mutation::TypeInfo: Send + Sync,
    Subscription: GraphQLSubscriptionType<S, Context = CtxT> + Send + 'static,
    Subscription::TypeInfo: Send + Sync,
    CtxT: Unpin + Send + Sync + 'static,
    S: ScalarValue + Send + Sync + 'static,
    I: Init<S, CtxT> + Send,
{
    let (mut ws_tx, mut ws_rx) = websocket.split();
    let (mut s_tx, mut s_rx) = Connection::new(ArcSchema(root_node), init).split();

    let ws_to_s = async move {
        //Box::pin(stream! {
        while let Some(msg) = ws_rx.next().await {
            tracing::trace!("{:?}", &msg);
            match msg {
                Ok(msg) => {
                    let msg = msg.into_text().map(Message).map_err(Error::Tungstenite)?;
                    s_tx.send(msg).await.unwrap();
                }
                Err(e) => return Err(Error::Tungstenite(e)),
            }
        }
        Ok(())
    }
    .boxed();

    let s_to_ws = async move {
        //Box::pin(stream! {
        while let Some(msg) = s_rx.next().await {
            tracing::trace!("{:?}", &msg);
            let msg = serde_json::to_string(&msg)
                .map(|x| tungstenite::Message::text(x))
                .map_err(|e| Error::Serde(e))?;

            ws_tx.send(msg).await.unwrap();
        }
        Ok(())
    }
    .boxed();

    match future::select(ws_to_s, s_to_ws).await {
        Either::Left((r, _)) => r, //.map_err(|e| e.into()),
        Either::Right((r, _)) => r,
    }
}
