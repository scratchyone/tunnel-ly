use httparse;
use reqwest;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpSocket;
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let forwarding_port = "8000";
    let forwarding_url = "http://localhost";
    let server_proxy_port = "8080";
    let server_http_port = "80";
    let domain = "rachel.test";
    let client = reqwest::Client::new();
    let service_id = client
        .post(&format!("http://{}:{}/start", domain, server_http_port))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    println!("body: {}", service_id);

    let mut socket = TcpStream::connect(format!("{}:{}", domain, server_proxy_port))
        .await
        .unwrap();
    socket.writable().await.unwrap();
    socket
        .write_all(format!("{}\0", service_id).as_bytes())
        .await
        .unwrap();
    loop {
        let mut bytes = vec![];
        loop {
            socket.readable().await.unwrap();
            let mut buf: [u8; 1] = [0; 1];
            let bytes_read = socket.read(&mut buf).await.unwrap();
            if bytes_read == 0 {
                continue;
            }
            if buf[0] == 0x00 {
                // End of message signalled
                break;
            }
            bytes.push(buf[0]);
        }
        let response = create_request(bytes, forwarding_url, forwarding_port).await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                println!("Error: {}", e);
                continue;
            }
        };
        let bytes = create_http_text(response).await;
        socket
            .write_all(format!("{}\0", bytes.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(&bytes).await.unwrap();
        socket.flush().await.unwrap();
    }
}

async fn create_request(
    bytes: Vec<u8>,
    forwarding_url: &str,
    forwarding_port: &str,
) -> Result<reqwest::Response, String> {
    let mut headers = vec![httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let p = req.parse(&bytes).unwrap();
    let pre_len = match p {
        httparse::Status::Complete(len) => len,
        httparse::Status::Partial => Err("Bad HTTP request".to_string())?,
    };
    let body = bytes[pre_len..].to_vec();
    let headers = req.headers.iter().filter(|h| **h != httparse::EMPTY_HEADER);
    let mut request = reqwest::Client::new()
        .request(
            req.method.unwrap().parse().expect("Could not parse method"),
            format!(
                "{}:{}{}",
                forwarding_url,
                forwarding_port,
                req.path.unwrap()
            ),
        )
        .body(body);
    for header in headers {
        request = request.header(header.name, header.value);
    }
    request.send().await.map_err(|e| e.to_string())
}

async fn create_http_text(req: reqwest::Response) -> Vec<u8> {
    let mut text = vec![];
    text.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            req.status().as_u16(),
            req.status().canonical_reason().unwrap()
        )
        .as_bytes(),
    );
    for (key, value) in req.headers() {
        text.extend_from_slice(format!("{}: {}\r\n", key, value.to_str().unwrap()).as_bytes());
    }
    text.extend_from_slice(b"\r\n");
    text.extend_from_slice(&req.bytes().await.unwrap());
    text
}
