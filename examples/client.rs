use std::net::SocketAddr;
use std::str::FromStr;
use futures_util::io::AsyncWriteExt;
use tokio::io::{AsyncReadExt};
use tokio::sync::Mutex;
use std::sync::Arc;
use utp::stream::UtpStream;
use tokio::{join, task};
use tokio::io::{self, AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: client <port>");
        std::process::exit(1);
    }
    let port: &String = &args[1];
    let addr = format!("127.0.0.1:{}", &port);
    let client_addr = SocketAddr::from_str(&addr).unwrap();

    let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();

    let client_stream = Arc::new(Mutex::new(UtpStream::bind(Some(client_addr)).await));
    let client_stream_clone = Arc::clone(&client_stream);
    let client_stream_clone_read = Arc::clone(&client_stream_clone);

    {
        let mut stream = client_stream.lock().await;
        match stream.connect(server_addr).await {
            Ok(_) => {
                println!("Connected to {}", server_addr);
            }
            Err(error) => {
                println!("Error: {}", error);
                return;
            }
        }
    }





    let write_task = task::spawn(async move {

        let stdin = io::stdin();

        let mut reader = BufReader::new(stdin).lines();

        while let Ok(Some(message)) = reader.next_line().await {
            println!("1 ---------- {}", message);
            {
                println!("locking ");
                let mut stream = client_stream.lock().await;
                println!("1 -------- {} ------ ", message);
                match stream.write_all(message.as_bytes()).await {
                    Ok(_) => println!("Sent: {}", message),
                    Err(error) => println!("Error writing: {}", error),
                }
                drop(stream);
            }
        }
    });

    let read_task = task::spawn(async move {
        let mut buf = vec![0u8; 1024];
        loop {
            println!("Client reading");
            {
                println!("locking reading");
                let mut stream = client_stream_clone_read.lock().await;
                println!("Client reading -------- ");
                match stream.read(&mut buf).await {
                    Ok(len) => {
                        println!("Received {} bytes", len);
                        let received_data = &buf[..len];
                        println!("Data: {:?}", String::from_utf8_lossy(received_data));
                    }
                    Err(error) => {
                        eprintln!("Error reading from stream: {}", error);
                        break;
                    }
                }
                drop(stream);
            }
        }
    });

    let _ = join!(read_task, write_task);
}