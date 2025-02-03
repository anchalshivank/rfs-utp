use std::sync::Arc;
use colored::Colorize;
// udp_client.rs
use tokio::net::UdpSocket;
use tokio::io::{self, AsyncBufReadExt};
use tokio::io::BufReader;
use tokio::task;

#[tokio::main]
async fn main() -> io::Result<()> {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?); // Bind to any available port
    let server_addr = "127.0.0.1:8080";
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let socket_rcv = Arc::clone(&socket);

    // Spawn a task to continuously receive messages
    task::spawn(async move {
        let mut buf = [0; 1024];
        loop {
            if let Ok((len, _)) = socket_rcv.recv_from(&mut buf).await {
                let ack = String::from_utf8_lossy(&buf[..len]);
                println!("Received ACK: {}", ack);
            }
        }
    });

    // Send messages concurrently
    loop {
        println!("Enter message: ");
        if let Ok(message) = reader.next_line().await {
            socket.send_to(message.clone().unwrap().as_bytes(), server_addr).await?;
            println!("Sent: {}", message.unwrap());
        }
    }
}
