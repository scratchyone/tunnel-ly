use hyper::http::status;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode, Version};
use log::{debug, error, trace, warn};
use rand::prelude::*;
use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::{collections::HashMap, io};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task,
};

#[tokio::main]
async fn main() -> io::Result<()> {
    pretty_env_logger::init();

    let proxy_ip = "127.0.0.1:8080";
    let http_ip = "127.0.0.1:80";
    let domain = "rachel.test";
    let service_mgr = spawn_service_manager(domain.to_string()).await;
    spawn_socket_manager(service_mgr.clone(), proxy_ip.to_string()).await;
    let thread =
        spawn_request_manager(http_ip.to_string(), service_mgr.clone(), domain.to_string()).await;
    thread.await.unwrap();
    Ok(())
}

#[derive(Debug)]
enum ServiceManagerMessage {
    ForwardRequest {
        request: Request<Body>,
        response_sender: UnboundedSender<Response<Body>>,
    },
    RegisterService {
        service_id: String,
        sender: UnboundedSender<ServiceSessionMessage>,
    },
    ForwardPrimaryStream {
        service_id: String,
        stream: TcpStream,
    },
    UnregisterService {
        service_id: String,
    },
}

#[derive(Debug)]
enum ServiceSessionMessage {
    RecvPrimaryStream(TcpStream),
    RecvRequest(Request<Body>, UnboundedSender<Response<Body>>),
}

async fn spawn_service_manager(domain: String) -> UnboundedSender<ServiceManagerMessage> {
    debug!("Spawning service manager");

    let (sender, mut receiver) = unbounded_channel();
    task::spawn(async move {
        debug!("Service manager started");
        let mut services: HashMap<String, UnboundedSender<ServiceSessionMessage>> = HashMap::new();
        loop {
            let msg = match receiver.recv().await {
                Some(msg) => msg,
                None => {
                    error!("Service manager failed to recv message");
                    continue;
                }
            };
            trace!("Service manager received message: {:?}", msg);
            match msg {
                ServiceManagerMessage::RegisterService { service_id, sender } => {
                    debug!("Service manager registered service: {}", service_id);
                    services.insert(service_id, sender);
                }
                ServiceManagerMessage::ForwardPrimaryStream { service_id, stream } => {
                    if let Some(sender) = services.get(&service_id) {
                        match sender.send(ServiceSessionMessage::RecvPrimaryStream(stream)) {
                            Ok(_) => {
                                debug!(
                                    "Service manager forwarded primary stream to service: {}",
                                    service_id
                                );
                            }
                            Err(e) => {
                                warn!("Service manager failed to forward primary stream: {}", e);
                            }
                        };
                    } else {
                        warn!("Service manager could not find service: {}", service_id);
                    }
                }
                ServiceManagerMessage::ForwardRequest {
                    request,
                    response_sender,
                } => {
                    let host = match request.headers().get(hyper::http::header::HOST) {
                        Some(host) => host.to_str(),
                        None => {
                            warn!("Service manager could not find host header");
                            let _ = response_sender.send(
                                Response::builder()
                                    .status(StatusCode::BAD_REQUEST)
                                    .body(Body::from("400 Bad Request"))
                                    .unwrap(),
                            );
                            continue;
                        }
                    };
                    let str_host = match host {
                        Ok(host) => host,
                        Err(e) => {
                            warn!("Service manager could not parse host header: {}", e);
                            let _ = response_sender.send(
                                Response::builder()
                                    .status(StatusCode::BAD_REQUEST)
                                    .body(Body::from("400 Bad Request"))
                                    .unwrap(),
                            );
                            continue;
                        }
                    };
                    let service_id = match str_host.strip_suffix(&format!(".{}", domain)) {
                        Some(service_id) => service_id,
                        None => str_host,
                    };

                    if let Some(sender) = services.get(service_id) {
                        let service_id = service_id.to_string();
                        match sender.send(ServiceSessionMessage::RecvRequest(
                            request,
                            response_sender.clone(),
                        )) {
                            Ok(_) => {
                                debug!(
                                    "Service manager forwarded request to service: {}",
                                    service_id
                                );
                            }
                            Err(e) => {
                                warn!("Service manager failed to forward request: {}", e);
                                let _ = response_sender.send(
                                    Response::builder()
                                        .status(StatusCode::BAD_GATEWAY)
                                        .body(Body::from("502 Bad Gateway"))
                                        .unwrap(),
                                );
                            }
                        }
                    } else {
                        warn!("Service manager could not find service: {}", service_id);
                        let _ = response_sender.send(
                            Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(Body::from("404 Service Not Found"))
                                .unwrap(),
                        );
                    }
                }
            }
        }
    });
    sender
}

async fn spawn_request_manager(
    http_ip: String,
    service_mgr: UnboundedSender<ServiceManagerMessage>,
    domain: String,
) -> task::JoinHandle<()> {
    debug!("Spawning request manager");
    task::spawn(async move {
        debug!("Request manager started");
        let make_service = make_service_fn(move |_conn| {
            let service_mgr = service_mgr.clone();
            let domain = domain.clone();
            async {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    handle_incoming_request(req, service_mgr.clone(), domain.clone())
                }))
            }
        });

        // Then bind and serve...
        let server = Server::bind(&SocketAddr::from_str(&http_ip).unwrap()).serve(make_service);
        // And run forever...
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
            panic!("server error: {}", e);
        }
    })
}

async fn handle_incoming_request(
    req: Request<Body>,
    service_mgr: UnboundedSender<ServiceManagerMessage>,
    domain: String,
) -> Result<Response<Body>, Infallible> {
    trace!("Request manager received request: {:?}", req);
    if req.headers().get(hyper::http::header::HOST).unwrap() == &domain {
        handle_root_request(req, service_mgr, domain).await
    } else {
        let (sender, mut receiver) = unbounded_channel();
        service_mgr
            .send(ServiceManagerMessage::ForwardRequest {
                request: req,
                response_sender: sender,
            })
            .unwrap();
        let response = receiver.recv().await.unwrap();
        Ok(response)
    }
}

async fn handle_root_request(
    req: Request<Body>,
    service_mgr: UnboundedSender<ServiceManagerMessage>,
    domain: String,
) -> Result<Response<Body>, Infallible> {
    if req.method() == Method::POST && req.uri().path() == "/start" {
        trace!("Request manager received start request: {:?}", req);
        let service_id = phonetic_key_generator();
        spawn_service_session(service_id.clone(), service_mgr).await;
        trace!("Request manager spawned service session: {}", service_id);
        Ok(Response::builder().body(Body::from(service_id)).unwrap())
    } else {
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Hello World"))
            .unwrap())
    }
}

async fn spawn_service_session(
    service_id: String,
    service_mgr: UnboundedSender<ServiceManagerMessage>,
) {
    debug!("Spawning service session: {}", service_id);
    task::spawn(async move {
        debug!("Service session started: {}", service_id);
        let (sender, mut receiver) = unbounded_channel();
        service_mgr
            .send(ServiceManagerMessage::RegisterService {
                service_id: service_id.clone(),
                sender,
            })
            .unwrap();
        trace!(
            "Service session registered with service manager: {}",
            service_id
        );
        let mut stream = loop {
            let msg = receiver.recv().await.unwrap();
            match msg {
                ServiceSessionMessage::RecvPrimaryStream(stream) => {
                    trace!("Service session received primary stream: {}", service_id);
                    break stream;
                }
                ServiceSessionMessage::RecvRequest(req, response_sender) => {}
            }
        };
        loop {
            let msg = match receiver.recv().await {
                Some(msg) => msg,
                None => {
                    continue;
                }
            };
            match msg {
                ServiceSessionMessage::RecvPrimaryStream(stream) => {}
                ServiceSessionMessage::RecvRequest(req, response_sender) => 'block: {
                    trace!(
                        "Service session received request from socket connection manager: {}",
                        service_id
                    );
                    let http_text = create_http_text(req).await;
                    stream.write_all(&http_text).await.unwrap();
                    stream.write_all(&[0x00]).await.unwrap();

                    trace!(
                        "Service session forwarded request to client: {}",
                        service_id
                    );

                    let mut bytes = vec![];
                    loop {
                        let mut buf: [u8; 1] = [0; 1];
                        let bytes_read = stream.read(&mut buf).await.unwrap();
                        if bytes_read == 0 {
                            continue;
                        }
                        if buf[0] == 0x00 {
                            // End of message signalled
                            break;
                        }
                        bytes.push(buf[0]);
                    }
                    let content_length: usize = String::from_utf8_lossy(&bytes).parse().unwrap();
                    let buf = &mut vec![0; content_length];
                    stream.read_exact(buf).await.unwrap();
                    let mut headers = vec![httparse::EMPTY_HEADER; 64];
                    let mut resp = httparse::Response::new(&mut headers);
                    let p = resp.parse(buf).unwrap();
                    let pre_len = match p {
                        httparse::Status::Complete(len) => len,
                        httparse::Status::Partial => {
                            response_sender
                                .send(
                                    Response::builder()
                                        .status(StatusCode::BAD_REQUEST)
                                        .body(Body::from("400 Bad Request"))
                                        .unwrap(),
                                )
                                .unwrap();
                            break 'block;
                        }
                    };
                    let headers = resp
                        .headers
                        .iter()
                        .filter(|h| **h != httparse::EMPTY_HEADER);
                    let version = match resp.version.unwrap() {
                        1 => Version::HTTP_11,
                        _ => Version::HTTP_10,
                    };
                    let status = StatusCode::from_u16(resp.code.unwrap()).unwrap();
                    let body = &buf[pre_len..];
                    let mut r = Response::builder().version(version).status(status);
                    for header in headers {
                        r = r.header(header.name, header.value);
                    }
                    trace!(
                        "Service session received and parsed response from client: {}",
                        service_id
                    );
                    response_sender
                        .send(r.body(Body::from(body.to_vec())).unwrap())
                        .unwrap();
                }
            }
        }
    });
}

async fn create_http_text(req: Request<Body>) -> Vec<u8> {
    let mut text = vec![];
    text.extend_from_slice(format!("{} {} HTTP/1.1\r\n", req.method(), req.uri()).as_bytes());
    for (key, value) in req.headers() {
        text.extend_from_slice(format!("{}: {}\r\n", key, value.to_str().unwrap()).as_bytes());
    }
    text.extend_from_slice(&b"\r\n"[..]);
    let body = req.into_body();
    let body_bytes = hyper::body::to_bytes(body).await.unwrap();
    text.extend_from_slice(&body_bytes[..]);
    text
}

async fn spawn_socket_manager(
    service_mgr: UnboundedSender<ServiceManagerMessage>,
    proxy_ip: String,
) {
    debug!("Spawning socket manager");
    task::spawn(async move {
        debug!("Socket manager started");
        let listener = TcpListener::bind(proxy_ip).await.unwrap();
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    error!("Socket manager failed to accept connection: {}", e);
                    continue;
                }
            };
            socket_manager_read(socket, service_mgr.clone()).await;
        }
    });
}

async fn socket_manager_read(
    mut socket: TcpStream,
    service_mgr: UnboundedSender<ServiceManagerMessage>,
) {
    trace!("Socket manager received new connection");
    let mut bytes = vec![];
    loop {
        let mut buf: [u8; 1] = [0; 1];
        let bytes_read = socket.read(&mut buf).await.unwrap();
        if bytes_read == 0 {
            // Stream ended early
            break;
        }
        if buf[0] == 0x00 {
            // End of message signalled
            break;
        }
        bytes.push(buf[0]);
    }
    let service_id = String::from_utf8_lossy(&bytes).to_string();
    trace!(
        "Socket manager forwarding connection to service manager: {}",
        service_id
    );
    service_mgr
        .send(ServiceManagerMessage::ForwardPrimaryStream {
            service_id,
            stream: socket,
        })
        .unwrap();
}

// Sorry this code is so weird, I ported it from some old JS code
fn phonetic_key_generator() -> String {
    let vowels = "aeiou".chars().collect::<Vec<char>>();
    let cons = "bcdfghjklmnpqrstvwxyz".chars().collect::<Vec<char>>();
    let mut text = vec![];
    let mut rng = rand::thread_rng();
    let start = i32::from(rng.gen::<bool>());
    let length = 10;
    for i in 0..length {
        text.push(if i % 2 == start {
            cons.choose(&mut rng).unwrap()
        } else {
            vowels.choose(&mut rng).unwrap()
        });
    }
    text.into_iter().collect::<String>()
}
