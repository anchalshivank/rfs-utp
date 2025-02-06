// udp_server.rs
use tokio::io;
use tokio::net::UdpSocket;

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:8080").await?;
    println!("Server listening on 127.0.0.1:8080");

    let mut buf = [0; 1024];
    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        let received = String::from_utf8_lossy(&buf[..len]);
        println!("Received: {} from {}", received, addr);

        // Send acknowledgment back
        let ack = "ACK";
        socket.send_to(ack.as_bytes(), addr).await?;
    }
}
