use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use lazy_static::lazy_static;
use log::{error, info};
use prometheus::{
    register_int_counter, register_int_counter_vec, register_int_gauge, Encoder, IntCounter,
    IntCounterVec, IntGauge, TextEncoder,
};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::OnceLock;

lazy_static! {
    static ref BLOCK_EVENTS: IntCounterVec = register_int_counter_vec!(
        "mineshield_proxy_block_events_total",
        "Total number of times the proxy blocked an action due to safety limits.",
        &["reason"]
    )
    .unwrap();
    static ref HANDSHAKE_FAILURES: IntCounterVec = register_int_counter_vec!(
        "mineshield_proxy_handshake_failures_total",
        "Total number of handshake failures encountered by the proxy.",
        &["reason"]
    )
    .unwrap();
    static ref CONNECTION_FAILURES: IntCounterVec = register_int_counter_vec!(
        "mineshield_proxy_connection_failures_total",
        "Total number of backend connection failures.",
        &["reason"]
    )
    .unwrap();
    static ref CONNECTION_ATTEMPTS: IntCounter = register_int_counter!(
        "mineshield_proxy_connection_attempts_total",
        "Total number of player login connection attempts handled by the proxy."
    )
    .unwrap();
    static ref CONNECTIONS_ESTABLISHED: IntCounter = register_int_counter!(
        "mineshield_proxy_connections_established_total",
        "Total number of backend connections successfully established."
    )
    .unwrap();
    static ref CONNECTIONS_CLOSED: IntCounter = register_int_counter!(
        "mineshield_proxy_connections_closed_total",
        "Total number of proxied connections that have been closed."
    )
    .unwrap();
    static ref PING_REQUESTS: IntCounter = register_int_counter!(
        "mineshield_proxy_ping_requests_total",
        "Total number of ping/status requests handled."
    )
    .unwrap();
    static ref ACTIVE_CONNECTIONS: IntGauge = register_int_gauge!(
        "mineshield_proxy_active_connections",
        "Current number of proxied connections actively being forwarded."
    )
    .unwrap();
    static ref BYTES_TRANSFERRED: IntCounterVec = register_int_counter_vec!(
        "mineshield_proxy_bytes_transferred_total",
        "Total number of bytes forwarded through the proxy.",
        &["direction"]
    )
    .unwrap();
    static ref IP_BLOCKS: IntCounter = register_int_counter!(
        "mineshield_proxy_ip_blocks_total",
        "Total number of times the proxy has blocked an IP address."
    )
    .unwrap();
}

static SERVER_START: OnceLock<()> = OnceLock::new();

pub fn spawn_metrics_server(addr: SocketAddr) {
    if SERVER_START.set(()).is_err() {
        return;
    }

    match Server::try_bind(&addr) {
        Ok(builder) => {
            tokio::spawn(async move {
                let make_svc = make_service_fn(|_| async {
                    Ok::<_, Infallible>(service_fn(|req| async move { serve(req).await }))
                });

                if let Err(err) = builder.serve(make_svc).await {
                    error!("Metrics server stopped: {}", err);
                }
            });
            info!("Prometheus metrics exporter listening on {}", addr);
        }
        Err(err) => {
            error!(
                "Failed to bind Prometheus metrics exporter on {}: {}",
                addr, err
            );
        }
    }
}

async fn serve(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => {
            let encoder = TextEncoder::new();
            let metric_families = prometheus::gather();
            let mut buffer = Vec::new();
            if let Err(err) = encoder.encode(&metric_families, &mut buffer) {
                error!("Failed to encode Prometheus metrics: {}", err);
                return Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("unable to encode metrics"))
                    .unwrap());
            }

            let response = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", encoder.format_type())
                .body(Body::from(buffer))
                .unwrap();
            Ok(response)
        }
        (&Method::GET, "/healthz") => Ok(Response::new(Body::from("ok"))),
        (&Method::HEAD, "/metrics") => Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap()),
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap()),
    }
}

pub fn record_block(reason: &'static str) {
    BLOCK_EVENTS.with_label_values(&[reason]).inc();
}

pub fn record_handshake_failure(reason: &'static str) {
    HANDSHAKE_FAILURES.with_label_values(&[reason]).inc();
}

pub fn record_connection_failure(reason: &'static str) {
    CONNECTION_FAILURES.with_label_values(&[reason]).inc();
}

pub fn record_connection_attempt() {
    CONNECTION_ATTEMPTS.inc();
}

pub fn record_connection_established() {
    CONNECTIONS_ESTABLISHED.inc();
    ACTIVE_CONNECTIONS.inc();
}

pub fn record_connection_closed() {
    CONNECTIONS_CLOSED.inc();
    ACTIVE_CONNECTIONS.dec();
}

pub fn record_ping_request() {
    PING_REQUESTS.inc();
}

pub fn record_bytes(direction: &'static str, bytes: usize) {
    if bytes > 0 {
        BYTES_TRANSFERRED
            .with_label_values(&[direction])
            .inc_by(bytes as u64);
    }
}

pub fn record_ip_block() {
    IP_BLOCKS.inc();
}
