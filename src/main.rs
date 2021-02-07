mod juniper_graphql_ws;
mod juniper_hyper;
mod serve;

use std::{pin::Pin, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use futures::Stream;
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Method, Response, Server, StatusCode,
};
use juniper::{EmptyMutation, FieldError, RootNode};
use juniper_graphql_ws::ConnectionConfig;
use serve::is_websocket_upgrade;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let filter = EnvFilter::from_default_env().add_directive("taskrunner=trace".parse().unwrap());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let context = Arc::new(Context {});
    let addr = ([127, 0, 0, 1], 3001).into();
    let schema = Arc::new(create_schema());

    let new_service = make_service_fn(move |_| {
        let schema = schema.clone();
        let cloned_context = context.clone();

        async {
            Ok::<_, hyper::Error>(service_fn(move |request| {
                tracing::info!("{} {}", request.method(), request.uri());
                let schema = schema.clone();
                let context = cloned_context.clone();

                async move {
                    match (request.method(), request.uri().path()) {
                        (&Method::OPTIONS, _) => {
                            // Add CORS headers
                            Ok(Response::builder()
                                .header("Access-Control-Allow-Origin", "*")
                                .header("Access-Control-Allow-Headers", "*")
                                .body(Body::empty())
                                .unwrap())
                        }
                        (&Method::GET, "/") => {
                            Ok(Response::new(Body::from("The Taskrunner API is up.")))
                        }
                        (&Method::GET, "/playground") => {
                            juniper_hyper::playground("/graphql", Some("/graphql")).await
                        }
                        (&Method::GET, "/graphql") if is_websocket_upgrade(&request) => {
                            Ok(serve::serve_graphql_ws(
                                request,
                                schema,
                                ConnectionConfig::new(Context {}),
                            ))
                        }
                        (&Method::GET, "/graphql") | (&Method::POST, "/graphql") => {
                            juniper_hyper::graphql(schema, context, request).await
                        }
                        _ => {
                            let mut response = Response::new(Body::from("Not found"));
                            *response.status_mut() = StatusCode::NOT_FOUND;
                            Ok(response)
                        }
                    }
                }
            }))
        }
    });

    let server = Server::bind(&addr).serve(new_service);
    tracing::info!("Listening on http://{}", addr);

    if let Err(e) = server.await {
        tracing::error!("server error: {}", e)
    }
}

pub type Schema = RootNode<'static, Query, EmptyMutation<Context>, Subscription>;

pub fn create_schema() -> Schema {
    Schema::new(Default::default(), Default::default(), Default::default())
}

#[derive(Default)]
pub struct Query;

#[derive(Default)]
pub struct Subscription;

pub struct Context {}

impl juniper::Context for Context {}

#[juniper::graphql_object(context = Context)]
impl Query {
    fn hello() -> DateTime<Utc> {
        Utc::now()
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<Event, FieldError>> + Send>>;

#[derive(juniper::GraphQLEnum)]
enum Event {
    NewUpdate,
}

#[juniper::graphql_subscription(context = Context)]
impl Subscription {
    async fn watch_updates() -> EventStream {
        let interval = tokio::time::interval(Duration::from_secs(3));
        let mut stream = tokio_stream::wrappers::IntervalStream::new(interval);

        Box::pin(async_stream::stream! {
            while let Some(_) = stream.next().await {
                yield Ok(Event::NewUpdate);
            }
        })
    }
}
