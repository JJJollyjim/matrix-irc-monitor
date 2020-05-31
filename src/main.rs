use futures_util::{
    future::{self, BoxFuture, FutureExt},
    stream::{self, Stream, StreamExt},
};
use hyper::{
    server::Server,
    service::{make_service_fn, service_fn},
};
use irc::client::prelude::{Client as IrcClient, Command, Config as IrcLibConfig};
use listenfd::ListenFd;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use ruma_client::{
    api::r0::{
        filter::{FilterDefinition, RoomEventFilter, RoomFilter},
        sync::sync_events::{Filter, SetPresence},
    },
    events::{collections::all::RoomEvent, room::message::MessageEventContent, EventType},
    identifiers::RoomId,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    convert::{Infallible, TryInto},
    error::Error,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::signal;
use tracing::*;

mod watch_stream;

#[derive(Deserialize, Debug)]
struct Config {
    #[serde(flatten)]
    matrix: MatrixConfig,
    #[serde(flatten)]
    irc: IrcConfig,
}

#[derive(Deserialize, Debug)]
struct MatrixConfig {
    homeserver: String,
    token: String,
    room: String,
}

#[derive(Deserialize, Debug)]
struct IrcConfig {
    irc_server: String,
    irc_port: String,
    irc_tls: String,
    irc_nick: String,
    irc_channel: String,
}

async fn irc_stream<'a>(
    config: &'a IrcConfig,
) -> (
    impl Fn(&str) -> Result<(), irc::error::Error> + 'a,
    impl Stream<Item = String>,
) {
    let mut client = IrcClient::from_config(IrcLibConfig {
        nickname: Some(config.irc_nick.clone()),
        server: Some(config.irc_server.clone()),
        port: Some(config.irc_port.parse().unwrap()),
        use_tls: Some(config.irc_tls.parse().unwrap()),
        channels: vec![config.irc_channel.clone()],
        realname: Some("IRC Bridge Monitor".to_owned()),
        ping_timeout: Some(30),
        ..Default::default()
    })
    .await
    .unwrap();
    client.identify().unwrap();

    let msg_stream = Box::pin(client.stream().unwrap().filter_map(|m| async move {
        if let Command::PRIVMSG(_, msg) = m.expect("irc msg err").command {
            Some(msg)
        } else {
            None
        }
    }));

    let channel: &str = &config.irc_channel;

    (
        move |text: &'_ str| client.send_privmsg(channel, text),
        msg_stream,
    )
}

async fn matrix_stream<'a>(
    config: &'a MatrixConfig,
) -> (
    impl Fn(String) -> BoxFuture<'a, Result<(), String>>, // + 'a,
    impl Stream<Item = String>,
) {
    let homeserver_url = config.homeserver.parse().unwrap();
    let room: RoomId = config.room.clone().try_into().unwrap();

    let session = ruma_client::Session {
        access_token: config.token.clone(),
        identification: None,
    };
    let client = ruma_client::Client::https(homeserver_url, Some(session));

    let stream = client.sync(
        Some(Filter::FilterDefinition(FilterDefinition {
            room: Some(RoomFilter {
                rooms: Some(vec![room.clone()]),
                timeline: Some(RoomEventFilter {
                    types: Some(vec!["m.room.message".to_owned()]),
                    ..Default::default()
                }),

                state: Some(RoomEventFilter::ignore_all()),
                ephemeral: Some(RoomEventFilter::ignore_all()),
                account_data: Some(RoomEventFilter::ignore_all()),
                ..Default::default()
            }),
            ..FilterDefinition::ignore_all()
        })),
        None,
        SetPresence::Offline,
        Some(Duration::from_secs(60)),
    );

    let msg_stream = Box::pin(
        stream
            .filter_map(|m| async {
                match m {
                    Ok(m) => Some(m),
                    Err(e) => {
                        error!(?e, "matrix sync error");
                        None
                    }
                }
            })
            .flat_map(|m| {
                stream::iter(
                    if let Some((_id, room)) = m.rooms.join.iter().next() {
                        room.timeline.events.clone()
                    } else {
                        vec![]
                    },
                )
            })
            .filter_map(|e| async move { e.deserialize().ok() })
            .filter_map(|e| async {
                if let RoomEvent::RoomMessage(msg_ev) = e {
                    Some(msg_ev.content)
                } else {
                    None
                }
            })
            .filter_map(|e| async {
                if let MessageEventContent::Text(text_content) = e {
                    Some(text_content.body)
                } else {
                    None
                }
            }),
    );

    let client = Arc::new(client);

    (
        move |text: String| {
            let room = config.room.clone().try_into().unwrap();
            client
                .clone()
                .request(
                    ruma_client::api::r0::message::create_message_event::Request {
                        room_id: room,
                        event_type: EventType::RoomMessage,
                        txn_id: text.to_string(),
                        data: serde_json::value::to_raw_value(&json!({
                            "msgtype": "m.text",
                            "body": text
                        }))
                        .unwrap(),
                    },
                )
                .map(|res| res.map(|_| ()).map_err(|e| e.to_string()))
                .boxed()
        },
        msg_stream,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    main2().await
}

async fn main2() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();

    let config = Box::leak(Box::new(
        envy::prefixed("MATRIX_IRC_MONITOR_").from_env::<Config>()?,
    ));

    let (irc_send, irc_stream) = irc_stream(&config.irc).await;
    let irc_watch = watch_stream::watch(irc_stream);

    let (matrix_send, matrix_stream) = matrix_stream(&config.matrix).await;
    let matrix_watch = watch_stream::watch(matrix_stream);

    let irc_send = Arc::new(irc_send);
    let irc_watch = Arc::new(irc_watch);
    let matrix_send = Arc::new(matrix_send);
    let matrix_watch = Arc::new(matrix_watch);

    let make_svc = make_service_fn(|_conn| {
        let irc_send = irc_send.clone();
        let irc_watch = irc_watch.clone();
        let matrix_send = matrix_send.clone();
        let matrix_watch = matrix_watch.clone();
        async move {
            // service_fn converts our function into a `Service`
            Ok::<_, Infallible>(service_fn(move |_| {
                let irc_send = irc_send.clone();
                let irc_watch = irc_watch.clone();
                let matrix_send = matrix_send.clone();
                let matrix_watch = matrix_watch.clone();
                async move {
                let i2m_str: String = thread_rng().sample_iter(&Alphanumeric).take(30).collect();
                let m2i_str: String = thread_rng().sample_iter(&Alphanumeric).take(30).collect();

                let start_time = Instant::now();

                let (i2m_res, m2i_res) = future::join(
                    async {
                        let irc_send_result = irc_send(&i2m_str);
                        info!(?irc_send_result);
                        (
                            matrix_watch(i2m_str, Duration::from_secs(50)).await,
                            start_time.elapsed(),
                        )
                    },
                    async {
                        let m_send_result = matrix_send(m2i_str.clone()).await;
                        info!(?m_send_result);
                        (
                            irc_watch(m2i_str, Duration::from_secs(50)).await,
                            start_time.elapsed(),
                        )
                    },
                )
                .await;

                let res: Result<hyper::Response<hyper::Body>, Infallible> =
                    Ok(hyper::Response::new(
                        format!(
                            r#"
# HELP bridgemon_m2i_up Success of Matrix->IRC messages
# TYPE bridgemon_m2i_up gauge
bridgemon_m2i_up {}

# HELP bridgemon_m2i_time RTT of Matrix->IRC messages
# TYPE bridgemon_m2i_time gauge
bridgemon_m2i_time {}

# HELP bridgemon_i2m_up Success of IRC->Matrix messages
# TYPE bridgemon_i2m_up gauge
bridgemon_i2m_up {}

# HELP bridgemon_i2m_time RTT of IRC->Matrix messages
# TYPE bridgemon_i2m_time gauge
bridgemon_i2m_time {}
"#,
                            m2i_res.0 as u8,
                            m2i_res.1.as_secs_f64(),
                            i2m_res.0 as u8,
                            i2m_res.1.as_secs_f64()
                        )
                        .into(),
                    ));
                res
            }}))
        }
    });

    Server::from_tcp(
        ListenFd::from_env()
            .take_tcp_listener(0_usize)
            .expect("can't listenfd")
            .expect("no listenfd 0"),
    )
    .expect("can't make server")
    .serve(make_svc)
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("can't serve");

    Ok(())
}

async fn shutdown_signal() {
    signal::unix::signal(signal::unix::SignalKind::interrupt())
        .unwrap()
        .recv()
        .await
        .expect("failed to install SIGINT handler");
}
