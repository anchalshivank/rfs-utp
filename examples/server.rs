use std::net::SocketAddr;
use std::str::FromStr;
use tokio::io::AsyncReadExt;
use utp::stream::UtpStream;

#[tokio::main]
async fn main() {

    let args : Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: client <port>");
    }

    let port = &args[1];

    let addr_string = format!("127.0.0.1:{}", port);
    let socket_addr = SocketAddr::from_str(&addr_string).unwrap();

    let mut server = UtpStream::bind(Some(socket_addr)).await;

    let mut buf = vec![0u8; 25];
    loop{

        match server.read(&mut buf).await{
            Ok(len) => {

                println!("Received {} bytes", len);

                let received_data = &buf[0..len];

                println!("received data: {:?}", String::from_utf8_lossy(received_data));



            }
            Err(error) => {
                println!("error: {}", error);
            }
        }

    }

}