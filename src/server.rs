//! Server process
use crate::account::write_user_events;
use crate::authentication::authenticate;
use crate::authentication::generate_auth_token;
use crate::authentication::get_token_value;
use crate::authentication::validate_auth_token;
use crate::close::Close;
use crate::close::CloseCmd;
use crate::config::{Settings, VerifiedUsersMode};
use crate::conn;
use crate::db;
use crate::db::SubmittedEvent;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::event::EventCmd;
use crate::event::EventWrapper;
use crate::info::RelayInfo;
use crate::nip05;
use crate::notice::Notice;
use crate::payment;
use crate::payment::InvoiceInfo;
use crate::payment::PaymentMessage;
use crate::repo::NostrRepo;
use crate::server::Error::CommandUnknownError;
use crate::server::EventWrapper::{WrappedAuth, WrappedEvent};
use crate::subscription::Subscription;
use crate::utils::to_map;
use futures::SinkExt;
use futures::StreamExt;
use governor::{Jitter, Quota, RateLimiter};
use http::header::HeaderMap;
use http::header::SET_COOKIE;
use http::Method;
use hyper::body::to_bytes;
use hyper::header::ACCEPT;
use hyper::service::{make_service_fn, service_fn};
use hyper::upgrade::Upgraded;
use hyper::{
    header, server::conn::AddrStream, upgrade, Body, Request, Response, Server, StatusCode,
};
use nostr::key::FromPkStr;
use nostr::key::Keys;
use prometheus::IntCounterVec;
use prometheus::IntGauge;
use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, Opts, Registry, TextEncoder};
use qrcode::render::svg;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver as MpscReceiver;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::runtime::Builder;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, error, info, trace, warn};
use tungstenite::error::CapacityError::MessageTooLong;
use tungstenite::error::Error as WsError;
use tungstenite::handshake;
use tungstenite::protocol::Message;
use tungstenite::protocol::WebSocketConfig;
use tera::{Context, Tera};
use hyper_staticfile::Static;

fn status_and_text(status: StatusCode, msg: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .body(Body::from(msg))
        .unwrap()
}

fn template(tera: &Tera, template: &'static str, ctx: &Context) -> Response<Body> {
    // re-compile templates on each request when in debug mode
    let html = if cfg!(debug_assertions) {
        let mut mutable_tera = tera.clone();
        mutable_tera.full_reload().unwrap();
        mutable_tera.render(template, &ctx).unwrap()
    } else {
        tera.render(template, &ctx).unwrap()
    };
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(html))
        .unwrap()
}

fn redirect(location: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header("location", location)
        .body(Body::empty())
        .unwrap()
}

/// Handle arbitrary HTTP requests, including for `WebSocket` upgrades.
#[allow(clippy::too_many_arguments)]
async fn handle_web_request(
    mut request: Request<Body>,
    repo: Arc<dyn NostrRepo>,
    settings: Settings,
    remote_addr: SocketAddr,
    broadcast: Sender<Event>,
    event_tx: tokio::sync::mpsc::Sender<SubmittedEvent>,
    payment_tx: tokio::sync::broadcast::Sender<PaymentMessage>,
    shutdown: Receiver<()>,
    registry: Registry,
    metrics: NostrMetrics,
    tera: Arc<Tera>,
    static_: Static,
) -> Result<Response<Body>, Infallible> {
    match (
        request.uri().path(),
        request.headers().contains_key(header::UPGRADE),
    ) {
        // Request for / as websocket
        ("/", true) => {
            trace!("websocket with upgrade request");
            //assume request is a handshake, so create the handshake response
            let response = match handshake::server::create_response_with_body(&request, || {
                Body::empty()
            }) {
                Ok(response) => {
                    //in case the handshake response creation succeeds,
                    //spawn a task to handle the websocket connection
                    tokio::spawn(async move {
                        //using the hyper feature of upgrading a connection
                        match upgrade::on(&mut request).await {
                            //if successfully upgraded
                            Ok(upgraded) => {
                                // set WebSocket configuration options
                                let config = WebSocketConfig {
                                    max_send_queue: Some(1024),
                                    max_message_size: settings.limits.max_ws_message_bytes,
                                    max_frame_size: settings.limits.max_ws_frame_bytes,
                                    ..Default::default()
                                };
                                //create a websocket stream from the upgraded object
                                let ws_stream = WebSocketStream::from_raw_socket(
                                    //pass the upgraded object
                                    //as the base layer stream of the Websocket
                                    upgraded,
                                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                                    Some(config),
                                )
                                .await;
                                let origin = get_header_string("origin", request.headers());
                                let user_agent = get_header_string("user-agent", request.headers());
                                // determine the remote IP from headers if the exist
                                let header_ip = settings
                                    .network
                                    .remote_ip_header
                                    .as_ref()
                                    .and_then(|x| get_header_string(x, request.headers()));
                                // use the socket addr as a backup
                                let remote_ip =
                                    header_ip.unwrap_or_else(|| remote_addr.ip().to_string());
                                let client_info = ClientInfo {
                                    remote_ip,
                                    user_agent,
                                    origin,
                                };
                                // spawn a nostr server with our websocket
                                tokio::spawn(nostr_server(
                                    repo,
                                    client_info,
                                    settings,
                                    ws_stream,
                                    broadcast,
                                    event_tx,
                                    shutdown,
                                    metrics,
                                ));
                            }
                            // todo: trace, don't print...
                            Err(e) => println!(
                                "error when trying to upgrade connection \
                                 from address {remote_addr} to websocket connection. \
                                 Error is: {e}",
                            ),
                        }
                    });
                    //return the response to the handshake request
                    response
                }
                Err(error) => {
                    warn!("websocket response failed");
                    let mut res =
                        Response::new(Body::from(format!("Failed to create websocket: {error}")));
                    *res.status_mut() = StatusCode::BAD_REQUEST;
                    return Ok(res);
                }
            };
            Ok::<_, Infallible>(response)
        }
        // Request for Relay info
        ("/", false) => {
            // handle request at root with no upgrade header
            // Check if this is a nostr server info request
            let accept_header = &request.headers().get(ACCEPT);
            // check if application/nostr+json is included
            if let Some(media_types) = accept_header {
                if let Ok(mt_str) = media_types.to_str() {
                    if mt_str.contains("application/nostr+json") {
                        // build a relay info response
                        debug!("Responding to server info request");
                        let rinfo = RelayInfo::from(settings);
                        let b = Body::from(serde_json::to_string_pretty(&rinfo).unwrap());
                        return Ok(Response::builder()
                            .status(200)
                            .header("Content-Type", "application/nostr+json")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(b)
                            .unwrap());
                    }
                }
            }

            // Redirect users to join page when pay to relay enabled
            if settings.pay_to_relay.enabled {
                return Ok(redirect("/join"));
            }

            if let Some(relay_file_path) = settings.info.relay_page {
                match file_bytes(&relay_file_path) {
                    Ok(file_content) => {
                        return Ok(Response::builder()
                            .status(200)
                            .header("Content-Type", "text/html; charset=UTF-8")
                            .body(Body::from(file_content))
                            .expect("request builder"));
                    },
                    Err(err) => {
                        error!("Failed to read relay_page file: {}. Will use default", err);
                    }
                }
            }

            Ok(status_and_text(StatusCode::OK, "Please use a Nostr client to connect."))
        }
        ("/metrics", false) => {
            let mut buffer = vec![];
            let encoder = TextEncoder::new();
            let metric_families = registry.gather();
            encoder.encode(&metric_families, &mut buffer).unwrap();

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/plain")
                .body(Body::from(buffer))
                .unwrap())
        }
        // LN bits callback endpoint for paid invoices
        ("/lnbits", false) => {
            let callback: payment::lnbits::LNBitsCallback =
                serde_json::from_slice(&to_bytes(request.into_body()).await.unwrap()).unwrap();
            debug!("LNBits callback: {callback:?}");

            if let Err(e) = payment_tx.send(PaymentMessage::InvoicePaid(callback.payment_hash)) {
                warn!("Could not send invoice update: {}", e);
                return Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Error processing callback"));
            }

            Ok(status_and_text(StatusCode::OK, "ok"))
        }
        // Endpoint for relays terms
        ("/terms", false) => {
            let mut ctx = Context::new();
            ctx.insert("terms_message", &settings.pay_to_relay.terms_message);
            Ok(template(&tera, "terms.html", &ctx))
        }
        // Endpoint to allow users to sign up
        ("/join", false) => {
            // Stops sign ups if disabled
            if !settings.pay_to_relay.sign_ups {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Sorry, joining is not allowed at the moment"));
            }

            Ok(template(&tera, "join.html", &Context::default()))
        }
        // Endpoint to display invoice
        ("/invoice", false) => {
            // Stops sign ups if disabled
            if !settings.pay_to_relay.sign_ups {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Sorry, joining is not allowed at the moment"));
            }

            // Get query pubkey from query string
            let pubkey = get_pubkey(&request);

            // Redirect back to join page if no pub key is found in query string
            if pubkey.is_none() {
                return Ok(redirect("/join"));
            }

            // Checks key is valid
            let pubkey = pubkey.unwrap();
            let key = Keys::from_pk_str(&pubkey);
            if key.is_err() {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Looks like your key is invalid"));
            }

            // Checks if user is already admitted
            let payment_message;
            if let Ok((admission_status, _)) = repo.get_account_balance(&key.unwrap()).await {
                if admission_status {
                    return Ok(redirect(&format!("/account?pubkey={}", &pubkey)));
                } else {
                    payment_message = PaymentMessage::CheckAccount(pubkey.clone());
                }
            } else {
                payment_message = PaymentMessage::NewAccount(pubkey.clone());
            }

            // Send message on payment channel requesting invoice
            if payment_tx.send(payment_message).is_err() {
                warn!("Could not send payment tx");
                return Ok(status_and_text(StatusCode::NOT_IMPLEMENTED, "Sorry, something went wrong"));
            }

            // wait for message with invoice back that matched the pub key
            let mut invoice_info: Option<InvoiceInfo> = None;
            while let Ok(msg) = payment_tx.subscribe().recv().await {
                match msg {
                    PaymentMessage::Invoice(m_pubkey, m_invoice_info) => {
                        if m_pubkey == pubkey.clone() {
                            invoice_info = Some(m_invoice_info);
                            break;
                        }
                    }
                    PaymentMessage::AccountAdmitted(m_pubkey) => {
                        if m_pubkey == pubkey.clone() {
                            return Ok(redirect(&format!("/account?pubkey={}", &pubkey)));
                        }
                    }
                    _ => (),
                }
            }

            // Return early if cant get invoice
            if invoice_info.is_none() {
                return Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Sorry, could not get invoice"));
            }

            // Since invoice is checked to be not none, unwrap
            let invoice_info = invoice_info.unwrap();

            let qr_code: String;
            if let Ok(code) = QrCode::new(invoice_info.bolt11.as_bytes()) {
                qr_code = code
                    .render()
                    .min_dimensions(200, 200)
                    .dark_color(svg::Color("#800000"))
                    .light_color(svg::Color("#ffff80"))
                    .build();
            } else {
                qr_code = "Could not render image".to_string();
            }

            let mut ctx = Context::new();
            ctx.insert("admission_cost", &settings.pay_to_relay.admission_cost);
            ctx.insert("qr_code", &qr_code);
            ctx.insert("bolt11", &invoice_info.bolt11);
            ctx.insert("pubkey", &pubkey);

            Ok(template(&tera, "invoice.html", &ctx))
        }
        ("/authorize", false) => {
            if request.method() != Method::POST {
                return Ok(status_and_text(StatusCode::METHOD_NOT_ALLOWED, "Invalid HTTP method"));
            }

            if !settings.pay_to_relay.enabled {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "This relay is not paid"));
            }

            let form_data = to_map(request.into_body()).await;
            let form_vals = (form_data.get("pubkey"), form_data.get("signature"));

            if let (Some(pub_key), Some(signature)) = form_vals {
                info!("Authorization request from user {}", pub_key);
                if let Ok(key) = Keys::from_pk_str(&pub_key) {
                    if authenticate(&key, signature) {
                        info!("User {} successfully authenticated", pub_key);
                        let token = generate_auth_token(&key, &settings);
                        return match repo.get_account_statistics(&key).await {
                            Ok(stats) => {
                                let mut ctx = Context::new();
                                ctx.insert("pubkey", &pub_key);
                                ctx.insert("stats", &stats);
                                ctx.insert("status", "authorized");

                                let resp = Response::builder()
                                    .status(StatusCode::OK)
                                    .header(SET_COOKIE, format!("token={}; HttpOnly; Secure; SameSite=Lax", token))
                                    .body(Body::from(tera.render("_statistics.html", &ctx).unwrap()))
                                    .unwrap();
                                Ok(resp)
                            }
                            Err(e) => {
                                warn!("Error getting statistics: {}", e);
                                Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Error getting account statistics"))
                            }
                        }
                    }
                    warn!("Received invalid signature from user {}: '{}'", pub_key, signature);
                    return Ok(status_and_text(StatusCode::BAD_REQUEST, "Invalid signature"));
                }
            }

            Ok(status_and_text(StatusCode::BAD_REQUEST, "Missing required values"))
        }
        ("/account", false) => {
            // Stops sign ups if disabled
            if !settings.pay_to_relay.enabled {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "This relay is not paid"));
            }

            // Gets the pubkey from query string
            let pubkey = get_pubkey(&request);

            // Redirect back to join page if no pub key is found in query string
            if pubkey.is_none() {
                return Ok(redirect("/join"));
            }

            // Checks key is valid
            let pubkey = pubkey.unwrap();
            let key = Keys::from_pk_str(&pubkey);
            if key.is_err() {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Looks like your key is invalid"));
            }
            let key = key.unwrap();

            // Account is checked async so user will have to refresh the page a couple times after
            // they have paid.
            if let Err(e) = payment_tx.send(PaymentMessage::CheckAccount(pubkey.clone())) {
                warn!("Could not check account: {}", e);
            }

            let mut status = "unknown";
            if let Ok((admission_status, _)) = repo.get_account_balance(&key).await {
                if admission_status {
                    status = "admitted";
                    if let Some(cookie) = request.headers().get("Cookie") {
                        if let Some(token) = get_token_value(cookie) {
                            if validate_auth_token(token, &key, &settings) {
                                status = "authorized";
                            }
                        }
                    }
                } else {
                    status = "denied";
                }
            }

            let mut ctx = Context::new();
            ctx.insert("pubkey", &pubkey);
            ctx.insert("status", &status);

            Ok(template(&tera, "account.html", &ctx))
        }
        ("/summary", false) => {
            if !settings.pay_to_relay.enabled {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "This relay is not paid"));
            }

            // Gets the pubkey from query string
            let pubkey = get_pubkey(&request);

            // Redirect back to join page if no pub key is found in query string
            if pubkey.is_none() {
                return Ok(redirect("/join"));
            }

            // Checks key is valid
            let pubkey = pubkey.unwrap();
            let key = Keys::from_pk_str(&pubkey);
            if key.is_err() {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Looks like your key is invalid"));
            }
            let key = key.unwrap();

            if let Ok((admission_status, _)) = repo.get_account_balance(&key).await {
                if !admission_status {
                    return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Statistics are for registered members only"));
                }
            } else {
                return Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Unable to get registration"));
            }

            if let Some(cookie) = request.headers().get("Cookie") {
                if let Some(token) = get_token_value(cookie) {
                    if validate_auth_token(token, &key, &settings) {
                        return match repo.get_account_statistics(&key).await {
                            Ok(stats) => {
                                let mut ctx = Context::new();
                                ctx.insert("pubkey", &pubkey);
                                ctx.insert("stats", &stats);
                                ctx.insert("status", "authorized");
                                Ok(template(&tera, "_statistics.html", &ctx))
                            }
                            Err(e) => {
                                warn!("Error getting statistics: {}", e);
                                Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Error getting account statistics"))
                            }
                        };
                    }
                }
            }

            Ok(status_and_text(StatusCode::UNAUTHORIZED, "You are not authorized to perform this operation"))
        }
        ("/download", false) => {
            if request.method() != Method::POST {
                return Ok(status_and_text(StatusCode::METHOD_NOT_ALLOWED, "Invalid HTTP method"));
            }

            if !settings.pay_to_relay.enabled {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "This relay is not paid"));
            }

            let cookie_header = request.headers().get("Cookie").cloned();
            let form_data = to_map(request.into_body()).await;

            let pubkey = form_data.get("pubkey");
            // Redirect back to join page if no pub key is found in the body
            if pubkey.is_none() {
                return Ok(redirect("/join"));
            }

            // Checks key is valid
            let pubkey = pubkey.unwrap();
            let key = Keys::from_pk_str(&pubkey);
            if key.is_err() {
                return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Looks like your key is invalid"));
            }

            let key = key.unwrap();
            if let Ok((admission_status, _)) = repo.get_account_balance(&key).await {
                if !admission_status {
                    return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Download events is for registered members only"));
                }
            } else {
                return Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Unable to get registration"));
            }

            if let Some(cookie) = cookie_header {
                if let Some(token) = get_token_value(&cookie) {
                    info!("Download request from user {}", pubkey);
                    if validate_auth_token(token, &key, &settings) {
                        let (results_tx, results_rx) = mpsc::channel::<Vec<Event>>(10);

                        if let Err(e) = repo.get_all_user_events(&key, results_tx, shutdown.resubscribe()).await {
                            warn!("Error getting user events: {}", e);
                            return Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Unable to get user events"));
                        }

                        return match write_user_events(pubkey, results_rx, shutdown.resubscribe()).await {
                            Ok((file_name, buff)) => {
                                Ok(Response::builder()
                                    .status(StatusCode::OK)
                                    .header("Content-Type", "text/csv")
                                    .header("Content-Disposition", format!("attachment; filename=\"{}\"", file_name))
                                    .body(Body::from(buff))
                                    .unwrap())
                            }
                            Err(e) => {
                                warn!("Error writing user events: {}", e);
                                Ok(status_and_text(StatusCode::INTERNAL_SERVER_ERROR, "Unable to write user events"))
                            }
                        }
                    }
                    warn!("Received invalid token from user {}: '{}'", pubkey, token);
                }
            }

            return Ok(status_and_text(StatusCode::UNAUTHORIZED, "Not logged in."));
        }
        // later balance
        (_, _) => Ok(static_.serve(request).await.unwrap())
    }
}

// Get pubkey from request query string
fn get_pubkey(request: &Request<Body>) -> Option<String> {
    let query = request.uri().query().unwrap_or("").to_string();

    // Gets the pubkey value from query string
    query.split('&').fold(None, |acc, pair| {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next();
        let value = parts.next();
        if key == Some("pubkey") {
            return value.map(|s| s.to_owned());
        }
        acc
    })
}

fn get_header_string(header: &str, headers: &HeaderMap) -> Option<String> {
    headers
        .get(header)
        .and_then(|x| x.to_str().ok().map(std::string::ToString::to_string))
}

// return on a control-c or internally requested shutdown signal
async fn ctrl_c_or_signal(mut shutdown_signal: Receiver<()>) {
    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("could not define signal");
    #[allow(clippy::never_loop)]
    loop {
        tokio::select! {
            _ = shutdown_signal.recv() => {
                info!("Shutting down webserver as requested");
                // server shutting down, exit loop
                break;
            },
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down webserver due to SIGINT");
                break;
            },
            _ = term_signal.recv() => {
                info!("Shutting down webserver due to SIGTERM");
                break;
            },
        }
    }
}

fn create_metrics() -> (Registry, NostrMetrics) {
    // setup prometheus registry
    let registry = Registry::new();

    let query_sub = Histogram::with_opts(HistogramOpts::new(
        "nostr_query_seconds",
        "Subscription response times",
    ))
    .unwrap();
    let query_db = Histogram::with_opts(HistogramOpts::new(
        "nostr_filter_seconds",
        "Filter SQL query times",
    ))
    .unwrap();
    let write_events = Histogram::with_opts(HistogramOpts::new(
        "nostr_events_write_seconds",
        "Event writing response times",
    ))
    .unwrap();
    let sent_events = IntCounterVec::new(
        Opts::new("nostr_events_sent_total", "Events sent to clients"),
        vec!["source"].as_slice(),
    )
    .unwrap();
    let connections =
        IntCounter::with_opts(Opts::new("nostr_connections_total", "New connections")).unwrap();
    let db_connections = IntGauge::with_opts(Opts::new(
        "nostr_db_connections",
        "Active database connections",
    ))
    .unwrap();
    let query_aborts = IntCounterVec::new(
        Opts::new("nostr_query_abort_total", "Aborted queries"),
        vec!["reason"].as_slice(),
    )
    .unwrap();
    let cmd_req = IntCounter::with_opts(Opts::new("nostr_cmd_req_total", "REQ commands")).unwrap();
    let cmd_event =
        IntCounter::with_opts(Opts::new("nostr_cmd_event_total", "EVENT commands")).unwrap();
    let cmd_close =
        IntCounter::with_opts(Opts::new("nostr_cmd_close_total", "CLOSE commands")).unwrap();
    let cmd_auth =
        IntCounter::with_opts(Opts::new("nostr_cmd_auth_total", "AUTH commands")).unwrap();
    let disconnects = IntCounterVec::new(
        Opts::new("nostr_disconnects_total", "Client disconnects"),
        vec!["reason"].as_slice(),
    )
    .unwrap();
    registry.register(Box::new(query_sub.clone())).unwrap();
    registry.register(Box::new(query_db.clone())).unwrap();
    registry.register(Box::new(write_events.clone())).unwrap();
    registry.register(Box::new(sent_events.clone())).unwrap();
    registry.register(Box::new(connections.clone())).unwrap();
    registry.register(Box::new(db_connections.clone())).unwrap();
    registry.register(Box::new(query_aborts.clone())).unwrap();
    registry.register(Box::new(cmd_req.clone())).unwrap();
    registry.register(Box::new(cmd_event.clone())).unwrap();
    registry.register(Box::new(cmd_close.clone())).unwrap();
    registry.register(Box::new(cmd_auth.clone())).unwrap();
    registry.register(Box::new(disconnects.clone())).unwrap();
    let metrics = NostrMetrics {
        query_sub,
        query_db,
        write_events,
        sent_events,
        connections,
        db_connections,
        disconnects,
        query_aborts,
        cmd_req,
        cmd_event,
        cmd_close,
        cmd_auth,
    };
    (registry, metrics)
}

fn file_bytes(path: &str) -> Result<Vec<u8>> {
    let f = File::open(path)?;
    let mut reader = BufReader::new(f);
    let mut buffer = Vec::new();
    // Read file into vector.
    reader.read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Start running a Nostr relay server.
pub fn start_server(settings: &Settings, shutdown_rx: MpscReceiver<()>) -> Result<(), Error> {
    trace!("Config: {:?}", settings);
    // do some config validation.
    if !Path::new(&settings.database.data_directory).is_dir() {
        error!("Database directory does not exist");
        return Err(Error::DatabaseDirError);
    }
    let addr = format!(
        "{}:{}",
        settings.network.address.trim(),
        settings.network.port
    );
    let socket_addr = addr.parse().expect("listening address not valid");
    // address whitelisting settings
    if let Some(addr_whitelist) = &settings.authorization.pubkey_whitelist {
        info!(
            "Event publishing restricted to {} pubkey(s)",
            addr_whitelist.len()
        );
    }
    // check if NIP-05 enforced user verification is on
    if settings.verified_users.is_active() {
        info!(
            "NIP-05 user verification mode:{:?}",
            settings.verified_users.mode
        );
        if let Some(d) = settings.verified_users.verify_update_duration() {
            info!("NIP-05 check user verification every:   {:?}", d);
        }
        if let Some(d) = settings.verified_users.verify_expiration_duration() {
            info!("NIP-05 user verification expires after: {:?}", d);
        }
        if let Some(wl) = &settings.verified_users.domain_whitelist {
            info!("NIP-05 domain whitelist: {:?}", wl);
        }
        if let Some(bl) = &settings.verified_users.domain_blacklist {
            info!("NIP-05 domain blacklist: {:?}", bl);
        }
    }
    // configure tokio runtime
    let rt = Builder::new_multi_thread()
        .enable_all()
        .thread_name_fn(|| {
            // give each thread a unique numeric name
            static ATOMIC_ID: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("tokio-ws-{id}")
        })
        // limit concurrent SQLite blocking threads
        .max_blocking_threads(settings.limits.max_blocking_threads)
        .on_thread_start(|| {
            trace!("started new thread: {:?}", std::thread::current().name());
        })
        .on_thread_stop(|| {
            trace!("stopped thread: {:?}", std::thread::current().name());
        })
        .build()
        .unwrap();
    // start tokio
    rt.block_on(async {
        let broadcast_buffer_limit = settings.limits.broadcast_buffer;
        let persist_buffer_limit = settings.limits.event_persist_buffer;
        let verified_users_active = settings.verified_users.is_active();
        let settings = settings.clone();
        info!("listening on: {}", socket_addr);
        // all client-submitted valid events are broadcast to every
        // other client on this channel.  This should be large enough
        // to accommodate slower readers (messages are dropped if
        // clients can not keep up).
        let (bcast_tx, _) = broadcast::channel::<Event>(broadcast_buffer_limit);
        // validated events that need to be persisted are sent to the
        // database on via this channel.
        let (event_tx, event_rx) = mpsc::channel::<SubmittedEvent>(persist_buffer_limit);
        // establish a channel for letting all threads now about a
        // requested server shutdown.
        let (invoke_shutdown, shutdown_listen) = broadcast::channel::<()>(1);
        // create a channel for sending any new metadata event.  These
        // will get processed relatively slowly (a potentially
        // multi-second blocking HTTP call) on a single thread, so we
        // buffer requests on the channel.  No harm in dropping events
        // here, since we are protecting against DoS.  This can make
        // it difficult to setup initial metadata in bulk, since
        // overwhelming this will drop events and won't register
        // metadata events.
        let (metadata_tx, metadata_rx) = broadcast::channel::<Event>(4096);

        let (payment_tx, payment_rx) = broadcast::channel::<PaymentMessage>(4096);

        let (registry, metrics) = create_metrics();

        // build a repository for events
        let repo = db::build_repo(&settings, metrics.clone()).await;
        // start the database writer task.  Give it a channel for
        // writing events, and for publishing events that have been
        // written (to all connected clients).
        tokio::task::spawn(db::db_writer(
            repo.clone(),
            settings.clone(),
            event_rx,
            bcast_tx.clone(),
            metadata_tx.clone(),
            payment_tx.clone(),
            shutdown_listen,
        ));
        info!("db writer created");

        // create a nip-05 verifier thread; if enabled.
        if settings.verified_users.mode != VerifiedUsersMode::Disabled {
            let verifier_opt = nip05::Verifier::new(
                repo.clone(),
                metadata_rx,
                bcast_tx.clone(),
                settings.clone(),
            );
            if let Ok(mut v) = verifier_opt {
                if verified_users_active {
                    tokio::task::spawn(async move {
                        info!("starting up NIP-05 verifier...");
                        v.run().await;
                    });
                }
            }
        }

        // Create payments thread if pay to relay enabled
        if settings.pay_to_relay.enabled {
            let payment_opt = payment::Payment::new(
                repo.clone(),
                payment_tx.clone(),
                payment_rx,
                bcast_tx.clone(),
                settings.clone(),
            );
            match payment_opt {
                Ok(mut p) => {
                    tokio::task::spawn(async move {
                        info!("starting payment process ...");
                        p.run().await;
                    });
                },
                Err(e) => {
                    error!("Failed to start payment process {e}");
                    std::process::exit(1);
                }
            }
        }

        // listen for (external to tokio) shutdown request
        let controlled_shutdown = invoke_shutdown.clone();
        tokio::spawn(async move {
            info!("control message listener started");
            match shutdown_rx.recv() {
                Ok(()) => {
                    info!("control message requesting shutdown");
                    controlled_shutdown.send(()).ok();
                }
                Err(std::sync::mpsc::RecvError) => {
                    trace!("shutdown requestor is disconnected (this is normal)");
                }
            };
        });
        // listen for ctrl-c interruupts
        let ctrl_c_shutdown = invoke_shutdown.clone();
        // listener for webserver shutdown
        let webserver_shutdown_listen = invoke_shutdown.subscribe();

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
            info!("shutting down due to SIGINT (main)");
            ctrl_c_shutdown.send(()).ok();
        });
        // spawn a task to check the pool size.
        //let pool_monitor = pool.clone();
        //tokio::spawn(async move {db::monitor_pool("reader", pool_monitor).await;});
        let mut template_path = settings.info.template_path.clone().unwrap_or("templates/".into());
        if !template_path.ends_with('/') {
            template_path += "/";
        }
        let mut _tera = match Tera::new(&(template_path + "**/*.html")) {
            Ok(t) => t,
            Err(e) => {
                println!("Parsing error(s): {}", e);
                ::std::process::exit(1);
            }
        };
        _tera.autoescape_on(vec![".html"]);
        let tera = Arc::new(_tera);
        let static_ = Static::new(Path::new("./public"));

        // A `Service` is needed for every connection, so this
        // creates one from our `handle_request` function.
        let make_svc = make_service_fn(|conn: &AddrStream| {
            let repo = repo.clone();
            let remote_addr = conn.remote_addr();
            let bcast = bcast_tx.clone();
            let event = event_tx.clone();
            let payment_tx = payment_tx.clone();
            let stop = invoke_shutdown.clone();
            let settings = settings.clone();
            let registry = registry.clone();
            let metrics = metrics.clone();
            let tera = tera.clone();
            let static_ = static_.clone();
            async move {
                // service_fn converts our function into a `Service`
                Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                    handle_web_request(
                        request,
                        repo.clone(),
                        settings.clone(),
                        remote_addr,
                        bcast.clone(),
                        event.clone(),
                        payment_tx.clone(),
                        stop.subscribe(),
                        registry.clone(),
                        metrics.clone(),
                        tera.clone(),
                        static_.clone(),
                    )
                }))
            }
        });
        let server = Server::bind(&socket_addr)
            .serve(make_svc)
            .with_graceful_shutdown(ctrl_c_or_signal(webserver_shutdown_listen));
        // run hyper in this thread.  This is why the thread does not return.
        if let Err(e) = server.await {
            eprintln!("server error: {e}");
        }
    });
    Ok(())
}

/// Nostr protocol messages from a client
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, Debug)]
#[serde(untagged)]
pub enum NostrMessage {
    /// `EVENT` and  `AUTH` messages
    EventMsg(EventCmd),
    /// A `REQ` message
    SubMsg(Subscription),
    /// A `CLOSE` message
    CloseMsg(CloseCmd),
}

/// Convert Message to `NostrMessage`
fn convert_to_msg(msg: &str, max_bytes: Option<usize>) -> Result<NostrMessage> {
    let parsed_res: Result<NostrMessage> =
        serde_json::from_str(msg).map_err(std::convert::Into::into);
    match parsed_res {
        Ok(m) => {
            if let NostrMessage::SubMsg(_) = m {
                // note; this only prints the first 16k of a REQ and then truncates.
                trace!("REQ: {:?}", msg);
            };
            if let NostrMessage::EventMsg(_) = m {
                if let Some(max_size) = max_bytes {
                    // check length, ensure that some max size is set.
                    if msg.len() > max_size && max_size > 0 {
                        return Err(Error::EventMaxLengthError(msg.len()));
                    }
                }
            }
            Ok(m)
        }
        Err(e) => {
            trace!("proto parse error: {:?}", e);
            trace!("parse error on message: {:?}", msg.trim());
            Err(Error::ProtoParseError)
        }
    }
}

/// Turn a string into a NOTICE message ready to send over a `WebSocket`
fn make_notice_message(notice: &Notice) -> Message {
    let json = match notice {
        Notice::Message(ref msg) => json!(["NOTICE", msg]),
        Notice::EventResult(ref res) => json!(["OK", res.id, res.status.to_bool(), res.msg]),
        Notice::AuthChallenge(ref challenge) => json!(["AUTH", challenge]),
    };

    Message::text(json.to_string())
}

fn allowed_to_send(event_str: &str, conn: &conn::ClientConn, settings: &Settings) -> bool {
    // TODO: pass in kind so that we can avoid deserialization for most events
    if settings.authorization.nip42_dms {
        match serde_json::from_str::<Event>(event_str) {
            Ok(event) => {
                if event.kind == 4 || event.kind == 44 || event.kind == 1059 {
                    match (conn.auth_pubkey(), event.tag_values_by_name("p").first()) {
                        (Some(auth_pubkey), Some(recipient_pubkey)) => {
                            recipient_pubkey == auth_pubkey || &event.pubkey == auth_pubkey
                        }
                        (_, _) => false,
                    }
                } else {
                    true
                }
            }
            Err(_) => false,
        }
    } else {
        true
    }
}

struct ClientInfo {
    remote_ip: String,
    user_agent: Option<String>,
    origin: Option<String>,
}

/// Handle new client connections.  This runs through an event loop
/// for all client communication.
#[allow(clippy::too_many_arguments)]
async fn nostr_server(
    repo: Arc<dyn NostrRepo>,
    client_info: ClientInfo,
    settings: Settings,
    mut ws_stream: WebSocketStream<Upgraded>,
    broadcast: Sender<Event>,
    event_tx: mpsc::Sender<SubmittedEvent>,
    mut shutdown: Receiver<()>,
    metrics: NostrMetrics,
) {
    // the time this websocket nostr server started
    let orig_start = Instant::now();
    // get a broadcast channel for clients to communicate on
    let mut bcast_rx = broadcast.subscribe();
    // Track internal client state
    let mut conn = conn::ClientConn::new(client_info.remote_ip);
    // subscription creation rate limiting
    let mut sub_lim_opt = None;
    // 100ms jitter when the rate limiter returns
    let jitter = Jitter::up_to(Duration::from_millis(100));
    let sub_per_min_setting = settings.limits.subscriptions_per_min;
    if let Some(sub_per_min) = sub_per_min_setting {
        if sub_per_min > 0 {
            trace!("Rate limits for sub creation ({}/min)", sub_per_min);
            let quota_time = core::num::NonZeroU32::new(sub_per_min).unwrap();
            let quota = Quota::per_minute(quota_time);
            sub_lim_opt = Some(RateLimiter::direct(quota));
        }
    }
    // Use the remote IP as the client identifier
    let cid = conn.get_client_prefix();
    // Create a channel for receiving query results from the database.
    // we will send out the tx handle to any query we generate.
    // this has capacity for some of the larger requests we see, which
    // should allow the DB thread to release the handle earlier.
    let (query_tx, mut query_rx) = mpsc::channel::<db::QueryResult>(20_000);
    // Create channel for receiving NOTICEs
    let (notice_tx, mut notice_rx) = mpsc::channel::<Notice>(128);

    // last time this client sent data (message, ping, etc.)
    let mut last_message_time = Instant::now();

    // ping interval (every 5 minutes)
    let default_ping_dur = Duration::from_secs(settings.network.ping_interval_seconds.into());

    // disconnect after 20 minutes without a ping response or event.
    let max_quiet_time = Duration::from_secs(60 * 20);

    let start = tokio::time::Instant::now() + default_ping_dur;
    let mut ping_interval = tokio::time::interval_at(start, default_ping_dur);

    // maintain a hashmap of a oneshot channel for active subscriptions.
    // when these subscriptions are cancelled, make a message
    // available to the executing query so it knows to stop.
    let mut running_queries: HashMap<String, oneshot::Sender<()>> = HashMap::new();
    // for stats, keep track of how many events the client published,
    // and how many it received from queries.
    let mut client_published_event_count: usize = 0;
    let mut client_received_event_count: usize = 0;

    let unspec = "<unspecified>".to_string();
    info!("new client connection (cid: {}, ip: {:?})", cid, conn.ip());
    let origin = client_info.origin.as_ref().unwrap_or(&unspec);
    let user_agent = client_info.user_agent.as_ref().unwrap_or(&unspec);
    info!(
        "cid: {}, origin: {:?}, user-agent: {:?}",
        cid, origin, user_agent
    );

    // Measure connections
    metrics.connections.inc();

    if settings.authorization.nip42_auth {
        conn.generate_auth_challenge();
        if let Some(challenge) = conn.auth_challenge() {
            ws_stream
                .send(make_notice_message(&Notice::AuthChallenge(
                    challenge.to_string(),
                )))
                .await
                .ok();
        }
    }

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                metrics.disconnects.with_label_values(&["shutdown"]).inc();
                    info!("Close connection down due to shutdown, client: {}, ip: {:?}, connected: {:?}", cid, conn.ip(), orig_start.elapsed());
                    // server shutting down, exit loop
                    break;
                },
            _ = ping_interval.tick() => {
                // check how long since we talked to client
                // if it has been too long, disconnect
                if last_message_time.elapsed() > max_quiet_time {
                    debug!("ending connection due to lack of client ping response");
                    metrics.disconnects.with_label_values(&["timeout"]).inc();
                    break;
                }
                // Send a ping
                ws_stream.send(Message::Ping(Vec::new())).await.ok();
            },
            Some(notice_msg) = notice_rx.recv() => {
                ws_stream.send(make_notice_message(&notice_msg)).await.ok();
            },
            Some(query_result) = query_rx.recv() => {
                // database informed us of a query result we asked for
                let subesc = query_result.sub_id.replace('"', "");
                if query_result.event == "EOSE" {
                    let send_str = format!("[\"EOSE\",\"{subesc}\"]");
                    ws_stream.send(Message::Text(send_str)).await.ok();
                } else if allowed_to_send(&query_result.event, &conn, &settings) {
                    metrics.sent_events.with_label_values(&["db"]).inc();
                    client_received_event_count += 1;
                    // send a result
                    let send_str = format!("[\"EVENT\",\"{}\",{}]", subesc, &query_result.event);
                    ws_stream.send(Message::Text(send_str)).await.ok();
                }
            },
            // TODO: consider logging the LaggedRecv error
            Ok(global_event) = bcast_rx.recv() => {
                // an event has been broadcast to all clients
                // first check if there is a subscription for this event.
                for (s, sub) in conn.subscriptions() {
                    if !sub.interested_in_event(&global_event) {
                        continue;
                    }
                    // TODO: serialize at broadcast time, instead of
                    // once for each consumer.
                    if let Ok(event_str) = serde_json::to_string(&global_event) {
                        if allowed_to_send(&event_str, &conn, &settings) {
                            // create an event response and send it
                            trace!("sub match for client: {}, sub: {:?}, event: {:?}",
                               cid, s,
                               global_event.get_event_id_prefix());
                            let subesc = s.replace('"', "");
                            metrics.sent_events.with_label_values(&["realtime"]).inc();
                            ws_stream.send(Message::Text(format!("[\"EVENT\",\"{subesc}\",{event_str}]"))).await.ok();
                        }
                    } else {
                        warn!("could not serialize event: {:?}", global_event.get_event_id_prefix());
                    }
                }
            },
            ws_next = ws_stream.next() => {
                // update most recent message time for client
                last_message_time = Instant::now();
                // Consume text messages from the client, parse into Nostr messages.
                let nostr_msg = match ws_next {
                    Some(Ok(Message::Text(m))) => {
                        convert_to_msg(&m,settings.limits.max_event_bytes)
                    },
                    Some(Ok(Message::Binary(_))) => {
                        ws_stream.send(
                            make_notice_message(&Notice::message("binary messages are not accepted".into()))).await.ok();
                        continue;
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        // get a ping/pong, ignore.  tungstenite will
                        // send responses automatically.
                        continue;
                    },
                    Some(Err(WsError::Capacity(MessageTooLong{size, max_size}))) => {
                        ws_stream.send(
                            make_notice_message(&Notice::message(format!("message too large ({size} > {max_size})")))).await.ok();
                        continue;
                    },
                    None |
                    Some(Ok(Message::Close(_)) |
                         Err(WsError::AlreadyClosed | WsError::ConnectionClosed |
                             WsError::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake)))
                        => {
                            debug!("websocket close from client (cid: {}, ip: {:?})",cid, conn.ip());
                            metrics.disconnects.with_label_values(&["normal"]).inc();
                            break;
                        },
                    Some(Err(WsError::Io(e))) => {
                        // IO errors are considered fatal
                        warn!("IO error (cid: {}, ip: {:?}): {:?}", cid, conn.ip(), e);
                        metrics.disconnects.with_label_values(&["error"]).inc();

                        break;
                    }
                    x => {
                        // default condition on error is to close the client connection
                        info!("unknown error (cid: {}, ip: {:?}): {:?} (closing conn)", cid, conn.ip(), x);
                        metrics.disconnects.with_label_values(&["error"]).inc();

                        break;
                    }
                };

                // convert ws_next into proto_next
                match nostr_msg {
                    Ok(NostrMessage::EventMsg(ec)) => {
                        // An EventCmd needs to be validated to be converted into an Event
                        // handle each type of message
                        let evid = ec.event_id().to_owned();
                        let parsed : Result<EventWrapper> = Result::<EventWrapper>::from(ec);
                        match parsed {
                            Ok(WrappedEvent(e)) => {
                                metrics.cmd_event.inc();
                                let id_prefix:String = e.id.chars().take(8).collect();
                                debug!("successfully parsed/validated event: {:?} (cid: {}, kind: {})", id_prefix, cid, e.kind);
                                // check if event is expired
                                if e.is_expired() {
                                    let notice = Notice::invalid(e.id, "The event has already expired");
                                    ws_stream.send(make_notice_message(&notice)).await.ok();
                                    // check if the event is too far in the future.
                                } else if e.is_valid_timestamp(settings.options.reject_future_seconds) {
                                    // Write this to the database.
                                    let auth_pubkey = conn.auth_pubkey().and_then(|pubkey| hex::decode(pubkey).ok());
                                    let submit_event = SubmittedEvent {
                                        event: e.clone(),
                                        notice_tx: notice_tx.clone(),
                                        source_ip: conn.ip().to_string(),
                                        origin: client_info.origin.clone(),
                                        user_agent: client_info.user_agent.clone(),
                                        auth_pubkey };
                                    event_tx.send(submit_event).await.ok();
                                    client_published_event_count += 1;
                                } else {
                                    info!("client: {} sent a far future-dated event", cid);
                                    if let Some(fut_sec) = settings.options.reject_future_seconds {
                                        let msg = format!("The event created_at field is out of the acceptable range (+{fut_sec}sec) for this relay.");
                                        let notice = Notice::invalid(e.id, &msg);
                                        ws_stream.send(make_notice_message(&notice)).await.ok();
                                    }
                                }
                            },
                            Ok(WrappedAuth(event)) => {
                                metrics.cmd_auth.inc();
                                if settings.authorization.nip42_auth {
                                    let id_prefix:String = event.id.chars().take(8).collect();
                                    debug!("successfully parsed auth: {:?} (cid: {})", id_prefix, cid);
                                    match &settings.info.relay_url {
                                        None => {
                                            error!("AUTH command received, but relay_url is not set in the config file (cid: {})", cid);
                                        },
                                        Some(relay) => {
                                            match conn.authenticate(&event, relay) {
                                                Ok(_) => {
                                                    let pubkey = match conn.auth_pubkey() {
                                                        Some(k) => k.chars().take(8).collect(),
                                                        None => "<unspecified>".to_string(),
                                                    };
                                                    info!("client is authenticated: (cid: {}, pubkey: {:?})", cid, pubkey);
                                                },
                                                Err(e) => {
                                                    info!("authentication error: {} (cid: {})", e, cid);
                                                    ws_stream.send(make_notice_message(&Notice::restricted(event.id, format!("authentication error: {e}").as_str()))).await.ok();
                                                },
                                            }
                                        }
                                    }
                                } else {
                                    let e = CommandUnknownError;
                                    info!("client sent an invalid event (cid: {})", cid);
                                    ws_stream.send(make_notice_message(&Notice::invalid(evid, &format!("{e}")))).await.ok();
                                }
                            },
                            Err(e) => {
                                metrics.cmd_event.inc();
                                info!("client sent an invalid event (cid: {})", cid);
                                ws_stream.send(make_notice_message(&Notice::invalid(evid, &format!("{e}")))).await.ok();
                            }
                        }
                    },
                    Ok(NostrMessage::SubMsg(s)) => {
                        debug!("subscription requested (cid: {}, sub: {:?})", cid, s.id);
                        // subscription handling consists of:
                        // * check for rate limits
                        // * registering the subscription so future events can be matched
                        // * making a channel to cancel to request later
                        // * sending a request for a SQL query
                        // Do nothing if the sub already exists.
                        if conn.has_subscription(&s) {
                            info!("client sent duplicate subscription, ignoring (cid: {}, sub: {:?})", cid, s.id);
                        } else {
                            metrics.cmd_req.inc();
                            if let Some(ref lim) = sub_lim_opt {
                                lim.until_ready_with_jitter(jitter).await;
                            }
                            if settings.limits.limit_scrapers && s.is_scraper() {
                                info!("subscription was scraper, ignoring (cid: {}, sub: {:?})", cid, s.id);
                                ws_stream.send(Message::Text(format!("[\"EOSE\",\"{}\"]", s.id))).await.ok();
                                continue
                            }
                            let (abandon_query_tx, abandon_query_rx) = oneshot::channel::<()>();
                            match conn.subscribe(s.clone()) {
                                Ok(()) => {
                                    // when we insert, if there was a previous query running with the same name, cancel it.
                                    if let Some(previous_query) = running_queries.insert(s.id.clone(), abandon_query_tx) {
                                        previous_query.send(()).ok();
                                    }
                                    if s.needs_historical_events() {
                                        // start a database query.  this spawns a blocking database query on a worker thread.
                                        repo.query_subscription(s, cid.clone(), query_tx.clone(), abandon_query_rx).await.ok();
                                    }
                                },
                                Err(e) => {
                                    info!("Subscription error: {} (cid: {}, sub: {:?})", e, cid, s.id);
                                    ws_stream.send(make_notice_message(&Notice::message(format!("Subscription error: {e}")))).await.ok();
                                }
                            }
                        }
                    },
                    Ok(NostrMessage::CloseMsg(cc)) => {
                        // closing a request simply removes the subscription.
                        let parsed : Result<Close> = Result::<Close>::from(cc);
                        if let Ok(c) = parsed {
                            metrics.cmd_close.inc();
                            // check if a query is currently
                            // running, and remove it if so.
                            let stop_tx = running_queries.remove(&c.id);
                            if let Some(tx) = stop_tx {
                                tx.send(()).ok();
                            }
                            // stop checking new events against
                            // the subscription
                            conn.unsubscribe(&c);
                        } else {
                            info!("invalid command ignored");
                            ws_stream.send(make_notice_message(&Notice::message("could not parse command".into()))).await.ok();
                        }
                    },
                    Err(Error::ConnError) => {
                        debug!("got connection close/error, disconnecting cid: {}, ip: {:?}",cid, conn.ip());
                        break;
                    }
                    Err(Error::EventMaxLengthError(s)) => {
                        info!("client sent command larger ({} bytes) than max size (cid: {})", s, cid);
                        ws_stream.send(make_notice_message(&Notice::message("event exceeded max size".into()))).await.ok();
                    },
                    Err(Error::ProtoParseError) => {
                        info!("client sent command that could not be parsed (cid: {})", cid);
                        ws_stream.send(make_notice_message(&Notice::message("could not parse command".into()))).await.ok();
                    },
                    Err(e) => {
                        info!("got non-fatal error from client (cid: {}, error: {:?}", cid, e);
                    },
                }
            },
        }
    }
    // connection cleanup - ensure any still running queries are terminated.
    for (_, stop_tx) in running_queries {
        stop_tx.send(()).ok();
    }
    info!(
        "stopping client connection (cid: {}, ip: {:?}, sent: {} events, recv: {} events, connected: {:?})",
        cid,
        conn.ip(),
        client_published_event_count,
        client_received_event_count,
        orig_start.elapsed()
    );
}

#[derive(Clone)]
pub struct NostrMetrics {
    pub query_sub: Histogram,        // response time of successful subscriptions
    pub query_db: Histogram,         // individual database query execution time
    pub db_connections: IntGauge,    // database connections in use
    pub write_events: Histogram,     // response time of event writes
    pub sent_events: IntCounterVec,  // count of events sent to clients
    pub connections: IntCounter,     // count of websocket connections
    pub disconnects: IntCounterVec,  // client disconnects
    pub query_aborts: IntCounterVec, // count of queries aborted by server
    pub cmd_req: IntCounter,         // count of REQ commands received
    pub cmd_event: IntCounter,       // count of EVENT commands received
    pub cmd_close: IntCounter,       // count of CLOSE commands received
    pub cmd_auth: IntCounter,        // count of AUTH commands received
}
